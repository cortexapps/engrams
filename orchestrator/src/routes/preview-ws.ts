/**
 * Live-host preview reverse-proxy (ADR 0064 P2b-ws) — WebSocket upgrades.
 *
 * The HTTP half (preview-proxy.ts) can't carry WebSockets — WS upgrades hit
 * node:http's `upgrade` event, not Hono's request path. This module handles a
 * preview-host upgrade and bridges the client WS to the guest port over the
 * same `PortRelay` raw-byte tunnel, so a dev server's live WS (Vite HMR, etc.)
 * works through a preview URL.
 *
 * Bun constraints drive the shape:
 *   - `socket.write()` in node:http's `upgrade` handler is a no-op under Bun
 *     (see server.ts), so we can't raw-pipe the client socket. We complete the
 *     client upgrade with the `ws` package's `handleUpgrade` (Bun-safe, as the
 *     shell terminal already proves) — giving a decoded client WS.
 *   - Bun's `http.request` ignores a custom `createConnection` (see
 *     preview-proxy.ts), so the guest side reuses the loopback trick: a one-shot
 *     `net.Server` raw-pipes a real local socket into the tunnel, and we open
 *     Bun's native `WebSocket` client to that local port. The guest dev server
 *     does its own WS handshake over the tunnel; we bridge messages both ways.
 *
 * Auth is identical to the HTTP path (owner / admin / share-token), evaluated
 * BEFORE completing the upgrade.
 */

import net from "node:net";
import type { IncomingMessage } from "node:http";
import type { Socket } from "node:net";
import { WebSocketServer, type WebSocket as WsConn, type RawData } from "ws";

import { ConnectError, Code } from "@connectrpc/connect";
import { config } from "../config.ts";
import { portRelay as defaultPortRelay } from "../control-plane/client.ts";
import {
  authorizePreview,
  previewSlugFromHost,
  tunnelSocket,
  type PortRelayClient,
} from "./preview-proxy.ts";
import { makePortExposureStore, type PortExposureStore } from "../db/port-exposures.ts";
import type { GetSession } from "./guard.ts";

export interface PreviewWsDeps {
  store?: PortExposureStore;
  portRelay?: PortRelayClient;
  getSession?: GetSession;
  previewBaseDomain?: string;
}

/** RFC 6455 reserved codes that can't be sent on the wire — map to 1000. */
function safeCloseCode(code: number): number {
  if (code === 1005 || code === 1006 || code === 1015 || code < 1000 || code > 4999) {
    return 1000;
  }
  return code;
}

/** ADR 0066: map a relay-tunnel failure to a WebSocket close code/reason. The
 * coordinator returns `resource_exhausted` when a session is at its concurrent
 * preview-connection cap → 1013 "Try Again Later" (retryable); anything else is
 * a generic 1011 upstream failure. */
function previewWsCloseFor(err: unknown): [number, string] {
  if (err instanceof ConnectError && err.code === Code.ResourceExhausted) {
    return [1013, "too many concurrent preview connections"];
  }
  return [1011, "preview upstream error"];
}

function rawDataToBufferSource(data: RawData): BufferSource {
  if (data instanceof ArrayBuffer) return data;
  if (Array.isArray(data)) return Buffer.concat(data) as unknown as BufferSource;
  return data as unknown as BufferSource;
}

/** Build the `server.on("upgrade")` hook. Returns `true` if it handled the
 * request (a preview host), `false` to let the normal (shell) path run. */
export function makePreviewUpgradeHandler(
  deps?: PreviewWsDeps,
): (req: IncomingMessage, socket: Socket, head: Buffer) => Promise<boolean> {
  const baseDomain = deps?.previewBaseDomain ?? config.previewBaseDomain;
  const relay: PortRelayClient =
    (deps?.portRelay as PortRelayClient | undefined) ??
    (defaultPortRelay as unknown as PortRelayClient);
  let store = deps?.store;
  const getStore = (): PortExposureStore => (store ??= makePortExposureStore());
  const resolveSession: GetSession =
    deps?.getSession ??
    (async (headers) => {
      const { auth } = await import("../auth/better-auth.ts");
      return auth.api.getSession({ headers } as Parameters<typeof auth.api.getSession>[0]);
    });

  // A dedicated WS server (noServer) — NOT the shell's, whose handleProtocols is
  // pinned to `tty` and would reject e.g. Vite's `vite-hmr` subprotocol. Echo
  // the client's first requested subprotocol so negotiation passes through.
  const wss = new WebSocketServer({
    noServer: true,
    handleProtocols: (protocols: Set<string>) => {
      const first = protocols.values().next().value;
      return first ?? false;
    },
  });

  return async function tryPreviewUpgrade(req, socket, head) {
    const slug = previewSlugFromHost(req.headers.host, baseDomain);
    if (!slug) return false; // not a preview host — let the shell handler run

    const url = new URL(req.url ?? "/", "http://localhost");
    const headers = new Headers();
    for (const key in req.headers) {
      const v = req.headers[key];
      if (v) headers.set(key, Array.isArray(v) ? v[0]! : v);
    }

    const authz = await authorizePreview({
      slug,
      token: url.searchParams.get("token"),
      headers,
      store: getStore(),
      getSession: resolveSession,
    });

    if (!authz.ok) {
      // Reject Bun-safely: complete the upgrade, then close with 4000+status so
      // the browser can read the reason (mirrors server.ts's shell rejection).
      wss.handleUpgrade(req, socket, head, (ws) =>
        ws.close(4000 + authz.status, `preview ${authz.status}`),
      );
      return true;
    }

    const subprotocols = (req.headers["sec-websocket-protocol"] ?? "")
      .split(",")
      .map((s) => s.trim())
      .filter(Boolean);

    wss.handleUpgrade(req, socket, head, (clientWs) => {
      bridgeClientToGuest(clientWs, relay, authz.row.sessionId, authz.row.port, url, subprotocols);
    });
    return true;
  };
}

/**
 * Bridge a connected client WS to the guest port: stand up a one-shot loopback
 * listener that raw-pipes a real local socket into the `PortRelay` tunnel, then
 * open a native `WebSocket` to that local port. The guest dev server handshakes
 * + frames over the tunnel; we forward messages + close in both directions.
 *
 * Exported so ADR 0081's IDE proxy (`routes/ide.ts`) reuses these mechanics
 * verbatim for code-server's own-handshake WS traffic, keyed by session path
 * instead of a preview slug/token — `url` there already carries the
 * route-prefix-stripped path code-server expects.
 */
export function bridgeClientToGuest(
  clientWs: WsConn,
  relay: PortRelayClient,
  sessionId: string,
  port: number,
  url: URL,
  subprotocols: string[],
): void {
  const abort = new AbortController();
  const pending: Array<string | BufferSource> = [];
  let guestWs: WebSocket | null = null;
  let guestOpen = false;

  // Attach the client handlers IMMEDIATELY (not inside the async listen
  // callback): `ws` drops messages that arrive before a 'message' listener
  // exists, and the client can send the instant it opens — before the guest-side
  // socket (set up async below) is ready. Buffer until the guest is OPEN.
  clientWs.on("message", (data: RawData, isBinary: boolean) => {
    const msg = isBinary ? rawDataToBufferSource(data) : data.toString();
    if (guestOpen && guestWs) guestWs.send(msg);
    else pending.push(msg);
  });
  const closeGuest = () => {
    abort.abort();
    try {
      guestWs?.close();
    } catch {
      /* noop */
    }
  };
  clientWs.on("close", closeGuest);
  clientWs.on("error", closeGuest);

  // Captured from the relay tunnel so the terminal close path can signal the
  // coordinator's `resource_exhausted` (preview-connection cap, ADR 0066) as a
  // retryable 1013 rather than a generic 1011.
  let tunnelError: unknown;
  const server = net.createServer((sock) => {
    server.close(); // one connection per upgrade
    const tunnel = tunnelSocket(relay, sessionId, port, abort.signal);
    sock.pipe(tunnel);
    tunnel.pipe(sock);
    const tearDown = () => {
      sock.destroy();
      tunnel.destroy();
    };
    sock.on("error", tearDown);
    tunnel.on("error", (err) => {
      tunnelError = err;
      tearDown();
    });
  });

  server.on("error", () => {
    try {
      clientWs.close(...previewWsCloseFor(tunnelError));
    } catch {
      /* already closing */
    }
  });

  server.listen(0, "127.0.0.1", () => {
    const addr = server.address();
    const localPort = addr && typeof addr === "object" ? addr.port : 0;
    // Native WebSocket (Bun) to the real local port → loopback → tunnel → guest.
    // Carry the original path/query + subprotocols so the guest routes/negotiates
    // exactly as the client asked (Vite HMR keys on both).
    const gw = new WebSocket(`ws://127.0.0.1:${localPort}${url.pathname}${url.search}`, subprotocols);
    gw.binaryType = "arraybuffer";
    guestWs = gw;

    gw.onopen = () => {
      guestOpen = true;
      for (const m of pending) gw.send(m);
      pending.length = 0;
    };
    // guest → client
    gw.onmessage = (ev: MessageEvent) => {
      try {
        if (typeof ev.data === "string") clientWs.send(ev.data);
        else clientWs.send(Buffer.from(ev.data as ArrayBuffer));
      } catch {
        /* client gone */
      }
    };
    gw.onclose = (ev: CloseEvent) => {
      try {
        // If the relay tunnel itself errored (e.g. the preview-connection cap),
        // the guest WS closes abnormally (1006) — surface the tunnel's mapped
        // code instead of the opaque 1006 so the client can distinguish it.
        if (tunnelError !== undefined) {
          clientWs.close(...previewWsCloseFor(tunnelError));
        } else {
          clientWs.close(safeCloseCode(ev.code), ev.reason);
        }
      } catch {
        /* noop */
      }
      try {
        server.close();
      } catch {
        /* noop */
      }
    };
    gw.onerror = () => {
      try {
        clientWs.close(...previewWsCloseFor(tunnelError));
      } catch {
        /* noop */
      }
    };
  });
}
