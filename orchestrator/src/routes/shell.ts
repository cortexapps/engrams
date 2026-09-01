/**
 * Shell WebSocket upgrade handler (ADR 0051 Task 21).
 *
 * GET /api/v1/sessions/:id/shell (raw `upgrade` event, not a Hono route)
 *
 * - ACCEPT-FIRST (ws-util.ts): the handshake completes before the auth
 *   round-trip; an unauthorized caller gets close 4401/4404. The old
 *   Hono-middleware shape (auth await → upgradeWebSocket) broke every
 *   accepted upgrade under Bun 1.4.0 — see ws-util.ts for the invariant
 *   and spec-sync.ts (#1333) for the original outage.
 * - Bridges to ShellRelayService.Relay bidi gRPC stream.
 * - pushableQueue (bounded 256 slots) bridges client frames → relay input.
 * - Open frame pushed into queue BEFORE pump starts (avoids coordinator deadlock).
 * - Pump IIFE is void'd; Canceled/Aborted errors are swallowed → no unhandledRejection.
 * - Server-side keepalive: ping every 20 s, close on missed pong.
 * - Backpressure: pause consuming upstream when bufferedAmount > 1 MiB.
 * - Sec-WebSocket-Protocol: tty echoed (this handler's own wss).
 */

import type { IncomingMessage } from "node:http";
import type { Socket } from "node:net";
import { WebSocketServer, type WebSocket as NodeWebSocket } from "ws";
import { ConnectError, Code } from "@connectrpc/connect";
import { create } from "@bufbuild/protobuf";
import {
  RelayShellRequestSchema,
  type RelayShellRequest,
  type RelayShellResponse,
} from "../gen/engram/app/v1/session_pb.ts";
import { shellRelay as defaultShellRelay } from "../control-plane/client.ts";
import { makeHeaderGuard } from "./guard.ts";
import type { GetSession, ResolveOwner } from "./guard.ts";
import { acceptUpgrade, requestHeaders } from "./ws-util.ts";
import { log as rootLog } from "../log.ts";

const log = rootLog.child({ component: "shell-ws" });

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

export function makeShellUpgradeHandler(
  deps?: ShellDeps,
): (req: IncomingMessage, socket: Socket, head: Buffer) => Promise<boolean> {
  const relayClient: ShellRelayClient =
    (deps?.shellRelay as ShellRelayClient | undefined) ??
    (defaultShellRelay as unknown as ShellRelayClient);
  const guard = makeHeaderGuard(deps?.getSession, deps?.resolveOwner);
  const wss = new WebSocketServer({
    noServer: true,
    // xterm.js hard-fails unless the offered 'tty' subprotocol is echoed.
    handleProtocols: (protocols) => (protocols.has("tty") ? "tty" : false),
  });

  const SHELL_PATH = /^\/api\/v1\/sessions\/([^/]+)\/shell$/;

  return async function tryShellUpgrade(req, socket, head) {
    let sessionId: string | null = null;
    try {
      const url = new URL(req.url ?? "/", "http://localhost");
      const m = SHELL_PATH.exec(url.pathname);
      sessionId = m ? decodeURIComponent(m[1]) : null;
    } catch {
      return false;
    }
    if (!sessionId) return false;
    const headers = requestHeaders(req);

    // Accept FIRST (ws-util.ts invariant) — authorize after.
    const ws = acceptUpgrade(wss, req, socket, head);
    if (!ws) {
      log.warn({ url: req.url }, "shell handshake produced no socket");
      return true;
    }

    // A client that hangs up during the auth round-trip must not be
    // bridged to the relay dead (same leak class spec-sync documents).
    let closedEarly = false;
    ws.once("close", () => {
      closedEarly = true;
    });
    const gone = () => closedEarly || ws.readyState !== ws.OPEN;

    void (async () => {
      try {
        const authz = await guard(headers, sessionId, "shell");
        if (gone()) return;
        if (!authz.ok) {
          ws.close(Math.min(4000 + authz.status, 4999), `shell ${authz.status}`);
          return;
        }
        attachShell(ws, sessionId, relayClient);
      } catch (error: unknown) {
        log.warn({ err: error, url: req.url }, "shell upgrade failed");
        try {
          ws.close(4500, "shell 500");
        } catch {
          /* already gone */
        }
      }
    })();
    return true;
  };
}

/** Bridge one accepted, authorized socket to the shell relay. */
function attachShell(
  ws: NodeWebSocket & { bufferedAmount?: number },
  sessionId: string,
  relayClient: ShellRelayClient,
): void {
  const inbound = pushableQueue<RelayShellRequest>(256);
  const abort = new AbortController();

  // IMPORTANT: push open frame BEFORE starting pump.
  // The relay() call blocks until the coordinator receives the open frame —
  // buffering it first prevents a deadlock.
  inbound.push(openFrame(sessionId));

  // Server-side keepalive: ping every 20s, close if pong not received.
  let pongReceived = true;
  ws.on("pong", () => {
    pongReceived = true;
  });
  const keepaliveInterval = setInterval(() => {
    if (!pongReceived) {
      clearInterval(keepaliveInterval);
      try {
        ws.close(1001, "keepalive timeout");
      } catch {
        /* ignore */
      }
      abort.abort();
      inbound.end();
      return;
    }
    pongReceived = false;
    try {
      ws.ping();
    } catch {
      /* ws may already be closed */
    }
  }, 20_000);

  // Upstream pump — void'd to prevent unhandledRejection.
  void (async () => {
    try {
      for await (const f of relayClient.relay(inbound, { signal: abort.signal })) {
        // Backpressure: pause consuming upstream when write buffer is large.
        if ((ws.bufferedAmount ?? 0) > 1024 * 1024) {
          await waitForDrain(ws);
        }
        switch (f.frame.case) {
          case "text":
            ws.send(f.frame.value);
            break;
          case "binary":
            ws.send(f.frame.value);
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
        log.warn({ err: e }, "shell relay upstream error");
        try {
          ws.close(1011, "upstream error");
        } catch {
          /* ignore */
        }
      }
    } finally {
      clearInterval(keepaliveInterval);
      try {
        ws.close();
      } catch {
        /* ignore */
      }
    }
  })();

  // Client frames → relay input stream.
  ws.on("message", (data: Buffer | ArrayBuffer | Buffer[], isBinary: boolean) => {
    const buf = Array.isArray(data) ? Buffer.concat(data) : data;
    inbound.push(frameFromWsData(isBinary ? (buf as Buffer) : buf.toString()));
  });

  ws.on("close", () => {
    abort.abort();
    inbound.end();
  });
}
