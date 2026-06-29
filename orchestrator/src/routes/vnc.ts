/**
 * VNC WebSocket route (ADR 0064).
 *
 * GET /api/v1/sessions/:id/vnc
 *
 * - Same auth + ownership gate as /shell, BEFORE upgrade (plain 401/404).
 * - Bridges to ShellRelayService.Relay with open.target = VNC; the coordinator
 *   routes target=VNC to host.proxy_vnc (P2.1).
 * - Carries raw RFB bytes (binary frames) for the browser's noVNC client.
 *
 * A near-twin of shell.ts: it reuses every relay/queue/frame helper from there
 * (pushableQueue, isAbortLike, frameFromWsData, pongFrame, ShellRelayClient).
 * The only things this module owns are vncOpenFrame (target=VNC), the
 * VncDeps/makeVncRoute wiring, and the route handler that bridges the helpers.
 * RFB is binary, so this route does NOT require the `tty` subprotocol.
 */

import { Hono } from "hono";
import type { Context } from "hono";
import type { UpgradeWebSocket, WSContext, WSEvents } from "hono/ws";
import type { WebSocket as NodeWebSocket } from "ws";
import { create } from "@bufbuild/protobuf";
import {
  RelayShellRequestSchema,
  ShellTarget,
  type RelayShellRequest,
} from "../gen/engram/app/v1/session_pb.ts";
import { shellRelay as defaultShellRelay } from "../control-plane/client.ts";
import { makeGuard } from "./guard.ts";
import type { GetSession, ResolveOwner } from "./guard.ts";
import {
  pushableQueue,
  isAbortLike,
  frameFromWsData,
  pongFrame,
  type ShellRelayClient,
} from "./shell.ts";

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

export interface VncDeps {
  shellRelay?: ShellRelayClient;
  getSession?: GetSession;
  resolveOwner?: ResolveOwner;
}

// ---------------------------------------------------------------------------
// Open frame — the one thing this module shapes differently from shell.ts
// ---------------------------------------------------------------------------

/** Open frame that selects this session's VNC stream (target = VNC). */
export function vncOpenFrame(sessionId: string): RelayShellRequest {
  return create(RelayShellRequestSchema, {
    frame: { case: "open", value: { sessionId, target: ShellTarget.VNC } },
  });
}

// ---------------------------------------------------------------------------
// Backpressure helper (local — mirrors shell.ts's private waitForDrain)
// ---------------------------------------------------------------------------

/**
 * Poll raw.bufferedAmount every 16 ms until it drops below 64 KiB. Used to
 * pause consuming upstream when the client write buffer is full. shell.ts keeps
 * its copy private, so this route carries its own small twin rather than
 * widening shell.ts's public surface for a 6-line poll.
 */
function waitForDrain(raw: { bufferedAmount?: number }): Promise<void> {
  return new Promise<void>((resolve) => {
    const check = () => {
      if ((raw.bufferedAmount ?? 0) < 64 * 1024) {
        resolve();
      } else {
        setTimeout(check, 16);
      }
    };
    check();
  });
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
  const relayClient: ShellRelayClient =
    (deps?.shellRelay as ShellRelayClient | undefined) ??
    (defaultShellRelay as unknown as ShellRelayClient);
  const guardFn = makeGuard(deps?.getSession, deps?.resolveOwner);

  // Mutable slot for the UpgradeWebSocket helper injected after
  // createNodeWebSocket. Typed as any to avoid the generic-variance friction
  // documented in shell.ts; the runtime type is UpgradeWebSocket<NodeWebSocket>.
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  let _upgradeWebSocket: UpgradeWebSocket<any> | null = null;

  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  function injectUpgrade(upgradeWebSocket: UpgradeWebSocket<any>): void {
    _upgradeWebSocket = upgradeWebSocket;
  }

  app.get("/api/v1/sessions/:id/vnc", async (c, next) => {
    // 1. Auth + ownership check — throws HTTPException(401/404) BEFORE upgrade.
    //    VNC is a session-scoped relay with the same ownership semantics as the
    //    shell, so it gates on the same "shell" action.
    await guardFn(c, "shell");

    if (!_upgradeWebSocket) {
      return c.json({ error: "WebSocket not configured" }, 500);
    }

    const sessionId = c.req.param("id");

    const createHandlers = (_c: Context): WSEvents<NodeWebSocket> => {
      const inbound = pushableQueue<RelayShellRequest>(256);
      const abort = new AbortController();

      return {
        onOpen(_e: Event, ws: WSContext<NodeWebSocket>) {
          const nodeWs = ws.raw as (NodeWebSocket & { bufferedAmount?: number }) | undefined;

          // Push the open frame BEFORE starting the pump: relay() blocks until
          // the coordinator receives it, so buffering it first avoids a deadlock.
          inbound.push(vncOpenFrame(sessionId));

          // Server-side keepalive: ping every 20s, close if pong not received.
          let pongReceived = true;
          if (nodeWs) {
            nodeWs.on("pong", () => { pongReceived = true; });
          }

          const keepaliveInterval = setInterval(() => {
            if (!pongReceived) {
              clearInterval(keepaliveInterval);
              try { ws.close(1001, "keepalive timeout"); } catch { /* ignore */ }
              abort.abort();
              inbound.end();
              return;
            }
            pongReceived = false;
            try { nodeWs?.ping(); } catch { /* ignore — ws may already be closed */ }
          }, 20_000);

          // Upstream pump — void'd to prevent unhandledRejection.
          void (async () => {
            try {
              for await (const f of relayClient.relay(inbound, { signal: abort.signal })) {
                // Backpressure: pause consuming upstream when write buffer is large.
                if ((nodeWs?.bufferedAmount ?? 0) > 1024 * 1024) {
                  await waitForDrain(nodeWs ?? {});
                }
                switch (f.frame.case) {
                  case "binary":
                    // RFB rides binary frames. Uint8Array from protobuf-es is
                    // Uint8Array<ArrayBuffer> at runtime.
                    ws.send(f.frame.value as Uint8Array<ArrayBuffer>);
                    break;
                  case "text":
                    // RFB is binary; text is unused but forwarded for parity.
                    ws.send(f.frame.value);
                    break;
                  case "ping":
                    inbound.push(pongFrame(f.frame.value));
                    break;
                  case "close":
                    ws.close(f.frame.value.code, f.frame.value.reason);
                    break;
                  // pong: no-op (informational)
                }
              }
            } catch (e) {
              if (!isAbortLike(e)) {
                console.warn({ err: e }, "vnc relay upstream error");
                try { ws.close(1011, "upstream error"); } catch { /* ignore */ }
              }
            } finally {
              clearInterval(keepaliveInterval);
              try { ws.close(); } catch { /* ignore */ }
            }
          })();
        },

        onMessage(e: MessageEvent, _ws: WSContext<NodeWebSocket>) {
          inbound.push(frameFromWsData(e.data as string | Buffer | ArrayBuffer));
        },

        onClose(_e: CloseEvent, _ws: WSContext<NodeWebSocket>) {
          abort.abort();
          inbound.end();
        },
      };
    };

    return _upgradeWebSocket(createHandlers)(c, next);
  });

  return { app, injectUpgrade };
}
