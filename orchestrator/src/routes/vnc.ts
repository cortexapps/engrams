/**
 * VNC WebSocket route (ADR 0065 / ADR 0066).
 *
 * GET /api/v1/sessions/:id/vnc
 *
 * - ACCEPT-FIRST like /shell (ws-util.ts): handshake completes, then the
 *   same auth + ownership gate as /shell; refusal = close 4401/4404.
 * - Ensures the in-guest browser stack is up (SessionService.EnsureBrowser →
 *   x11vnc on the guest's loopback :5900), then bridges the browser's noVNC
 *   client to that RFB port over the ADR-0066 vsock port relay: agentd dials
 *   127.0.0.1:5900 in-guest and splices raw RFB bytes.
 *
 * This is a plain websockify — x11vnc speaks raw RFB over TCP, so we terminate
 * the client WebSocket here and pump its binary frames to/from the `tunnelSocket`
 * Duplex (the same raw-byte PortRelay tunnel the live-host previews use). Unlike
 * preview-ws.ts (which bridges a guest that itself speaks WebSocket, e.g. Vite
 * HMR), there is no guest-side WS handshake. RFB is binary → no `tty`/subprotocol.
 */

import type { IncomingMessage } from "node:http";
import type { Socket } from "node:net";
import type { Duplex } from "node:stream";
import { WebSocketServer, type WebSocket as NodeWebSocket } from "ws";

import { sessions as defaultSessions, portRelay as defaultPortRelay } from "../control-plane/client.ts";
import { tunnelSocket, type PortRelayClient } from "./preview-proxy.ts";
import { makeHeaderGuard } from "./guard.ts";
import type { GetSession, ResolveOwner } from "./guard.ts";
import { isAbortLike } from "./shell.ts";
import { acceptUpgrade, requestHeaders } from "./ws-util.ts";
import { log as rootLog } from "../log.ts";

const log = rootLog.child({ component: "vnc-ws" });

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

export interface VncDeps {
  /** Control-plane SessionService client (for EnsureBrowser). */
  sessions?: Pick<typeof defaultSessions, "ensureBrowser">;
  /** PortRelay client for the raw RFB tunnel to the guest. */
  portRelay?: PortRelayClient;
  getSession?: GetSession;
  resolveOwner?: ResolveOwner;
}

/** Poll raw.bufferedAmount every 16 ms until it drops below 1 MiB, so we can
 * pause the guest→client pump when the client write buffer is full. */
function waitForDrain(raw: { bufferedAmount?: number }): Promise<void> {
  return new Promise<void>((resolve) => {
    const check = () => {
      if ((raw.bufferedAmount ?? 0) < 1024 * 1024) resolve();
      else setTimeout(check, 16);
    };
    check();
  });
}

function toBuffer(data: string | Buffer | ArrayBuffer | Uint8Array): Buffer {
  if (typeof data === "string") return Buffer.from(data);
  if (Buffer.isBuffer(data)) return data;
  return Buffer.from(data as ArrayBuffer);
}

// ---------------------------------------------------------------------------
// Route factory
// ---------------------------------------------------------------------------

export function makeVncUpgradeHandler(
  deps?: VncDeps,
): (req: IncomingMessage, socket: Socket, head: Buffer) => Promise<boolean> {
  const sessions = deps?.sessions ?? defaultSessions;
  const portRelay: PortRelayClient =
    (deps?.portRelay as PortRelayClient | undefined) ??
    (defaultPortRelay as unknown as PortRelayClient);
  const guard = makeHeaderGuard(deps?.getSession, deps?.resolveOwner);
  const wss = new WebSocketServer({
    noServer: true,
    // Older noVNC offers 'binary'; echo it when offered, else select none
    // (false → no subprotocol, the upgrade still proceeds per RFC 6455).
    handleProtocols: (protocols) => (protocols.has("binary") ? "binary" : false),
  });

  const VNC_PATH = /^\/api\/v1\/sessions\/([^/]+)\/vnc$/;

  return async function tryVncUpgrade(req, socket, head) {
    let sessionId: string | null = null;
    try {
      const url = new URL(req.url ?? "/", "http://localhost");
      const m = VNC_PATH.exec(url.pathname);
      sessionId = m ? decodeURIComponent(m[1]) : null;
    } catch {
      return false;
    }
    if (!sessionId) return false;
    const headers = requestHeaders(req);

    // Accept FIRST (ws-util.ts invariant) — authorize after.
    const ws = acceptUpgrade(wss, req, socket, head) as
      | (NodeWebSocket & { bufferedAmount?: number })
      | null;
    if (!ws) {
      log.warn({ url: req.url }, "vnc handshake produced no socket");
      return true;
    }

    let closedEarly = false;
    ws.once("close", () => {
      closedEarly = true;
    });
    const gone = () => closedEarly || ws.readyState !== ws.OPEN;

    void (async () => {
      try {
        // VNC is a session-scoped interactive relay with the same bar as
        // the shell tab.
        const authz = await guard(headers, sessionId, "shell");
        if (gone()) return;
        if (!authz.ok) {
          ws.close(Math.min(4000 + authz.status, 4999), `vnc ${authz.status}`);
          return;
        }
        attachVnc(ws, sessionId, sessions, portRelay);
      } catch (error: unknown) {
        log.warn({ err: error, url: req.url }, "vnc upgrade failed");
        try {
          ws.close(4500, "vnc 500");
        } catch {
          /* already gone */
        }
      }
    })();
    return true;
  };
}

/** Bridge one accepted, authorized socket to the guest's RFB port. */
function attachVnc(
  ws: NodeWebSocket & { bufferedAmount?: number },
  sessionId: string,
  sessions: Pick<typeof defaultSessions, "ensureBrowser">,
  portRelay: PortRelayClient,
): void {
  const abort = new AbortController();
  // The raw RFB tunnel to the guest — opened async (after EnsureBrowser), so
  // buffer any client bytes that arrive before it's up. RFB is server-first
  // (x11vnc sends its banner before the client speaks), so this is belt-only.
  let sock: Duplex | null = null;
  const pending: Buffer[] = [];

  // Server-side keepalive: ping every 20s, close if pong not received.
  let pongReceived = true;
  ws.on("pong", () => {
    pongReceived = true;
  });
  const keepalive = setInterval(() => {
    if (!pongReceived) {
      clearInterval(keepalive);
      try {
        ws.close(1001, "keepalive timeout");
      } catch {
        /* ignore */
      }
      abort.abort();
      return;
    }
    pongReceived = false;
    try {
      ws.ping();
    } catch {
      /* ws may already be closed */
    }
  }, 20_000);

  void (async () => {
    try {
      // Ensure the ephemeral browser stack is up; get the RFB port.
      const { port } = await sessions.ensureBrowser({ sessionId }, { signal: abort.signal });

      // Open the raw RFB tunnel to the guest's loopback port over the
      // PortRelay (which pins the session against idle eviction while
      // open). Flush anything the client sent before we were ready.
      const s = tunnelSocket(portRelay, sessionId, port, abort.signal);
      sock = s;
      for (const b of pending) s.write(b);
      pending.length = 0;

      // Guest RFB bytes → client WS binary frames, with backpressure.
      s.on("data", async (chunk: Buffer) => {
        if ((ws.bufferedAmount ?? 0) > 1024 * 1024) {
          s.pause();
          await waitForDrain(ws);
          s.resume();
        }
        try {
          ws.send(chunk);
        } catch {
          /* client gone */
        }
      });
      s.on("close", () => {
        clearInterval(keepalive);
        try {
          ws.close();
        } catch {
          /* ignore */
        }
      });
      s.on("error", (err: unknown) => {
        if (!isAbortLike(err)) log.warn({ err }, "vnc tunnel error");
        try {
          ws.close(1011, "upstream error");
        } catch {
          /* ignore */
        }
      });
    } catch (e) {
      clearInterval(keepalive);
      if (!isAbortLike(e)) {
        log.warn({ err: e }, "vnc ensureBrowser/tunnel error");
        try {
          ws.close(1011, "browser start failed");
        } catch {
          /* ignore */
        }
      }
    }
  })();

  // Client RFB bytes → guest x11vnc (via the tunnel).
  ws.on("message", (data: Buffer | ArrayBuffer | Buffer[]) => {
    const buf = Array.isArray(data) ? Buffer.concat(data) : toBuffer(data as Buffer | ArrayBuffer);
    if (sock) sock.write(buf);
    else pending.push(buf);
  });

  ws.on("close", () => {
    abort.abort();
    sock?.destroy();
  });
}
