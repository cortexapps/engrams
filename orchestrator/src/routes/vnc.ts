/**
 * VNC WebSocket route (ADR 0065 / ADR 0066).
 *
 * GET /api/v1/sessions/:id/vnc
 *
 * - Same auth + ownership gate as /shell, BEFORE upgrade (plain 401/404).
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

import { Hono } from "hono";
import type { Context } from "hono";
import type { UpgradeWebSocket, WSContext, WSEvents } from "hono/ws";
import type { WebSocket as NodeWebSocket } from "ws";
import type { Duplex } from "node:stream";

import { sessions as defaultSessions, portRelay as defaultPortRelay } from "../control-plane/client.ts";
import { tunnelSocket, type PortRelayClient } from "./preview-proxy.ts";
import { makeGuard } from "./guard.ts";
import type { GetSession, ResolveOwner } from "./guard.ts";
import { isAbortLike } from "./shell.ts";

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

export function makeVncRoute(deps?: VncDeps): {
  app: Hono;
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  injectUpgrade: (upgradeWebSocket: UpgradeWebSocket<any>) => void;
} {
  const app = new Hono();
  const sessions = deps?.sessions ?? defaultSessions;
  const portRelay: PortRelayClient =
    (deps?.portRelay as PortRelayClient | undefined) ??
    (defaultPortRelay as unknown as PortRelayClient);
  const guardFn = makeGuard(deps?.getSession, deps?.resolveOwner);

  // Mutable slot for the UpgradeWebSocket helper injected after
  // createNodeWebSocket (see index.ts). Typed `any` to dodge the generic
  // variance friction documented in shell.ts.
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  let _upgradeWebSocket: UpgradeWebSocket<any> | null = null;
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  function injectUpgrade(upgradeWebSocket: UpgradeWebSocket<any>): void {
    _upgradeWebSocket = upgradeWebSocket;
  }

  app.get("/api/v1/sessions/:id/vnc", async (c, next) => {
    // 1. Auth + ownership — throws HTTPException(401/404) BEFORE upgrade. VNC is
    //    a session-scoped interactive relay with the same bar as the shell tab.
    await guardFn(c, "shell");

    if (!_upgradeWebSocket) {
      return c.json({ error: "WebSocket not configured" }, 500);
    }

    const sessionId = c.req.param("id");

    const createHandlers = (_c: Context): WSEvents<NodeWebSocket> => {
      const abort = new AbortController();
      // The raw RFB tunnel to the guest — opened async (after EnsureBrowser), so
      // buffer any client bytes that arrive before it's up. RFB is server-first
      // (x11vnc sends its banner before the client speaks), so this is belt-only.
      let sock: Duplex | null = null;
      const pending: Buffer[] = [];

      return {
        onOpen(_e: Event, ws: WSContext<NodeWebSocket>) {
          const nodeWs = ws.raw as (NodeWebSocket & { bufferedAmount?: number }) | undefined;

          // Server-side keepalive: ping every 20s, close if pong not received.
          let pongReceived = true;
          nodeWs?.on("pong", () => {
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
              nodeWs?.ping();
            } catch {
              /* ws may already be closed */
            }
          }, 20_000);

          void (async () => {
            try {
              // 2. Ensure the ephemeral browser stack is up; get the RFB port.
              const { port } = await sessions.ensureBrowser({ sessionId }, { signal: abort.signal });

              // 3. Open the raw RFB tunnel to the guest's loopback port over the
              //    PortRelay (which pins the session against idle eviction while
              //    open). Flush anything the client sent before we were ready.
              const s = tunnelSocket(portRelay, sessionId, port, abort.signal);
              sock = s;
              for (const b of pending) s.write(b);
              pending.length = 0;

              // 4. Guest RFB bytes → client WS binary frames, with backpressure.
              s.on("data", async (chunk: Buffer) => {
                if ((nodeWs?.bufferedAmount ?? 0) > 1024 * 1024) {
                  s.pause();
                  await waitForDrain(nodeWs ?? {});
                  s.resume();
                }
                try {
                  ws.send(chunk as unknown as Uint8Array<ArrayBuffer>);
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
                if (!isAbortLike(err)) console.warn({ err }, "vnc tunnel error");
                try {
                  ws.close(1011, "upstream error");
                } catch {
                  /* ignore */
                }
              });
            } catch (e) {
              clearInterval(keepalive);
              if (!isAbortLike(e)) {
                console.warn({ err: e }, "vnc ensureBrowser/tunnel error");
                try {
                  ws.close(1011, "browser start failed");
                } catch {
                  /* ignore */
                }
              }
            }
          })();
        },

        // Client RFB bytes → guest x11vnc (via the tunnel).
        onMessage(e: MessageEvent, _ws: WSContext<NodeWebSocket>) {
          const buf = toBuffer(e.data as string | Buffer | ArrayBuffer);
          if (sock) sock.write(buf);
          else pending.push(buf);
        },

        onClose(_e: CloseEvent, _ws: WSContext<NodeWebSocket>) {
          abort.abort();
          sock?.destroy();
        },
      };
    };

    return _upgradeWebSocket(createHandlers)(c, next);
  });

  return { app, injectUpgrade };
}
