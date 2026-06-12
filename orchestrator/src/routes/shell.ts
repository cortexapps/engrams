/**
 * Shell WebSocket route (ADR 0039 Task 21).
 *
 * GET /api/v1/sessions/:id/shell
 *
 * - Auth + ownership gate fires BEFORE upgrade (plain 401/404 on failure).
 * - Bridges to ShellRelayService.Relay bidi gRPC stream.
 * - Uses @hono/node-ws (Option a: node:http upgrade events).
 * - pushableQueue (bounded 256 slots) bridges onMessage → relay input stream.
 * - Open frame pushed into queue BEFORE pump starts (avoids coordinator deadlock).
 * - Pump IIFE is void'd; Canceled/Aborted errors are swallowed → no unhandledRejection.
 * - Server-side keepalive: ping every 20 s, close on missed pong.
 * - Backpressure: pause consuming upstream when bufferedAmount > 1 MiB.
 * - Sec-WebSocket-Protocol: tty echoed (set via wss.options.handleProtocols in index.ts).
 */

import { Hono } from "hono";
import type { Context } from "hono";
import type { UpgradeWebSocket, WSContext, WSEvents } from "hono/ws";
import type { WebSocket as NodeWebSocket } from "ws";
import { ConnectError, Code } from "@connectrpc/connect";
import { create } from "@bufbuild/protobuf";
import {
  RelayShellRequestSchema,
  type RelayShellRequest,
  type RelayShellResponse,
} from "../gen/engram/app/v1/session_pb.ts";
import { shellRelay as defaultShellRelay } from "../control-plane/client.ts";
import { makeGuard } from "./guard.ts";
import type { GetSession, ResolveOwner } from "./guard.ts";

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/** Minimal interface of the ShellRelayService client we depend on. */
export interface ShellRelayClient {
  relay(
    inbound: AsyncIterable<RelayShellRequest>,
    options?: { signal?: AbortSignal },
  ): AsyncIterable<RelayShellResponse>;
}

export interface ShellDeps {
  shellRelay?: ShellRelayClient;
  getSession?: GetSession;
  resolveOwner?: ResolveOwner;
}

// ---------------------------------------------------------------------------
// pushableQueue — bounded async-iterable queue
// ---------------------------------------------------------------------------

export interface PushableQueue<T> extends AsyncIterable<T> {
  push(item: T): void;
  end(): void;
}

/**
 * A bounded async-iterable queue. Consumers await items via `for await`.
 * Overflow policy: if push() is called when at capacity, end() is called
 * (drop-connection semantics — upstream is too fast for client).
 */
export function pushableQueue<T>(cap: number): PushableQueue<T> {
  const buf: T[] = [];
  let ended = false;
  // Resolve waiting consumer when an item is available.
  let waiting: ((done: boolean) => void) | null = null;

  function push(item: T): void {
    if (ended) return;
    if (buf.length >= cap) {
      // Overflow: terminate the queue.
      end();
      return;
    }
    buf.push(item);
    if (waiting) {
      const w = waiting;
      waiting = null;
      w(false);
    }
  }

  function end(): void {
    if (ended) return;
    ended = true;
    if (waiting) {
      const w = waiting;
      waiting = null;
      w(true);
    }
  }

  async function* iterate(): AsyncGenerator<T> {
    while (true) {
      if (buf.length > 0) {
        yield buf.shift()!;
        continue;
      }
      if (ended) return;
      // Wait for next item or end signal.
      await new Promise<boolean>((resolve) => {
        waiting = resolve;
      });
      // Loop again: either buf has items or ended is true.
    }
  }

  const iterable = iterate();

  return {
    push,
    end,
    [Symbol.asyncIterator]() {
      return iterable;
    },
  };
}

// ---------------------------------------------------------------------------
// isAbortLike
// ---------------------------------------------------------------------------

/**
 * True for ConnectError with Code.Canceled or Code.Aborted,
 * or any error with name === 'AbortError'.
 */
export function isAbortLike(err: unknown): boolean {
  if (err instanceof ConnectError) {
    return err.code === Code.Canceled || err.code === Code.Aborted;
  }
  if (err instanceof Error && err.name === "AbortError") return true;
  return false;
}

// ---------------------------------------------------------------------------
// Frame constructors
// ---------------------------------------------------------------------------

/**
 * Build a RelayShellRequest from raw WebSocket message data.
 * String → text frame; Buffer/ArrayBuffer → binary frame.
 */
export function frameFromWsData(
  data: string | Buffer | ArrayBuffer,
): RelayShellRequest {
  if (typeof data === "string") {
    return create(RelayShellRequestSchema, {
      frame: { case: "text", value: data },
    });
  }
  const bytes =
    data instanceof ArrayBuffer
      ? new Uint8Array(data)
      : new Uint8Array((data as Buffer).buffer, (data as Buffer).byteOffset, (data as Buffer).byteLength);
  return create(RelayShellRequestSchema, {
    frame: { case: "binary", value: bytes },
  });
}

/** Build the initial open frame that tells the coordinator which session. */
export function openFrame(sessionId: string): RelayShellRequest {
  return create(RelayShellRequestSchema, {
    frame: { case: "open", value: { sessionId } },
  });
}

/** Build a pong frame in response to an upstream ping. */
export function pongFrame(data: Uint8Array): RelayShellRequest {
  return create(RelayShellRequestSchema, {
    frame: { case: "pong", value: data },
  });
}

// ---------------------------------------------------------------------------
// Backpressure helper
// ---------------------------------------------------------------------------

/**
 * Poll raw.bufferedAmount every 16 ms until it drops below 64 KiB.
 * Used to pause consuming upstream when the client write buffer is full.
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

export function makeShellRoute(deps?: ShellDeps): {
  app: Hono;
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  injectUpgrade: (upgradeWebSocket: UpgradeWebSocket<any>) => void;
} {
  const app = new Hono();
  const relayClient: ShellRelayClient =
    (deps?.shellRelay as ShellRelayClient | undefined) ??
    (defaultShellRelay as unknown as ShellRelayClient);
  const guardFn = makeGuard(deps?.getSession, deps?.resolveOwner);

  // Mutable slot for the UpgradeWebSocket helper injected after createNodeWebSocket.
  // Typed as any to avoid complex variance issues with the generic type parameter.
  // The actual runtime type is UpgradeWebSocket<NodeWebSocket> from @hono/node-ws.
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  let _upgradeWebSocket: UpgradeWebSocket<any> | null = null;

  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  function injectUpgrade(upgradeWebSocket: UpgradeWebSocket<any>): void {
    _upgradeWebSocket = upgradeWebSocket;
  }

  app.get("/api/v1/sessions/:id/shell", async (c, next) => {
    // 1. Auth + ownership check — throws HTTPException(401/404) BEFORE upgrade.
    //    This runs in the standard Hono handler; if it throws, the WS upgrade
    //    never happens and the client gets a plain HTTP 4xx response.
    await guardFn(c, "shell");

    if (!_upgradeWebSocket) {
      return c.json({ error: "WebSocket not configured" }, 500);
    }

    const sessionId = c.req.param("id");

    // 2. Build WebSocket handler factory (called by @hono/node-ws after upgrade).
    //    The factory ignores the Context because sessionId was already captured
    //    from the outer Hono handler closure.
    const createHandlers = (_c: Context): WSEvents<NodeWebSocket> => {
      const inbound = pushableQueue<RelayShellRequest>(256);
      const abort = new AbortController();

      return {
        onOpen(_e: Event, ws: WSContext<NodeWebSocket>) {
          const nodeWs = ws.raw as (NodeWebSocket & { bufferedAmount?: number }) | undefined;

          // IMPORTANT: push open frame BEFORE starting pump.
          // The relay() call blocks until the coordinator receives the open frame —
          // buffering it first prevents a deadlock.
          inbound.push(openFrame(sessionId));

          // Server-side keepalive: ping every 20s, close if pong not received.
          let pongReceived = true;
          if (nodeWs) {
            nodeWs.on("pong", () => { pongReceived = true; });
          }

          const keepaliveInterval = setInterval(() => {
            if (!pongReceived) {
              // Missed pong — close the connection.
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
                  case "text":
                    ws.send(f.frame.value);
                    break;
                  case "binary":
                    // Uint8Array from protobuf-es is Uint8Array<ArrayBuffer> at runtime.
                    ws.send(f.frame.value as Uint8Array<ArrayBuffer>);
                    break;
                  case "ping":
                    // Answer upstream ping with a pong via inbound queue.
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
                console.warn({ err: e }, "shell relay upstream error");
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

