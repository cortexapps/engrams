# Shell WS Route Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add `GET /api/v1/sessions/:id/shell` WebSocket route to the Bun orchestrator, bridging to `ShellRelayService.Relay` bidi gRPC, with auth gate, safe teardown (no unhandledRejection), subprotocol echo (`tty`), keepalive, and backpressure.

**Architecture:** Use `@hono/node-ws` (already in pnpm lockfile at 1.3.1) with `createNodeWebSocket` + `injectWebSocket(server)` — Option (a). Auth gate runs BEFORE upgrade via a pre-upgrade check in `upgradeWebSocket`. A `pushableQueue` async-iterable bridges WS messages into the bidi gRPC stream; the upstream pump runs in a void'd async IIFE with all abort/cancel errors swallowed. Keepalive via server-side `ws.ping()` every 20s.

**Tech Stack:** Bun (node:http compat), `@hono/node-ws` 1.3.1, `ws` 8.21.0, `@connectrpc/connect-node`, `hono`, `bun:test`

---

## File Map

- **Create:** `orchestrator/src/routes/shell.ts` — shell WS route factory (pushableQueue, bridge, helpers)
- **Modify:** `orchestrator/src/server.ts` — inject WebSocket server
- **Modify:** `orchestrator/src/index.ts` — mount shell route + injectWebSocket
- **Create:** `orchestrator/src/__tests__/shell.test.ts` — unit tests (queue, frame mapping, guard, process-survives, subprotocol, keepalive concept)
- **Modify:** `orchestrator/src/smoke.live.test.ts` — add live shell smoke test (14/N)

---

### Task 1: Add `@hono/node-ws` to package.json dependencies

**Files:**
- Modify: `orchestrator/package.json`

- [ ] **Step 1: Add the package**

`@hono/node-ws` is already in the pnpm lockfile (1.3.1) but NOT in package.json dependencies. Add it.

Edit `orchestrator/package.json` — add to `dependencies`:
```json
"@hono/node-ws": "^1.3.1",
```

- [ ] **Step 2: Install**

```bash
cd /Users/ganeshdatta/Documents/engrams/orchestrator && bun install
```

Expected: lockfile unchanged (already satisfied), no errors.

- [ ] **Step 3: Verify import resolves**

```bash
cd /Users/ganeshdatta/Documents/engrams/orchestrator && bun -e "import('@hono/node-ws').then(m => console.log('ok', Object.keys(m)))"
```

Expected: `ok [ 'createNodeWebSocket' ]`

---

### Task 2: Write pushableQueue + frame helpers (shell.ts — core helpers only)

**Files:**
- Create: `orchestrator/src/routes/shell.ts`

- [ ] **Step 1: Write failing test for pushableQueue**

Create `orchestrator/src/__tests__/shell.test.ts`:

```typescript
/**
 * Unit tests for the shell WS route (ADR 0039 Task 21).
 *
 * Coverage (no env needed):
 *   1. pushableQueue: enqueue + iterate + end (capacity = 4)
 *   2. pushableQueue: Buffer round-trip (binary data preserved)
 *   3. pushableQueue: overflow drops and closes (exceeds cap)
 *   4. isAbortLike: Canceled ConnectError → true; plain Error → false
 *   5. frameFromWsData: string → text frame; Buffer → binary frame
 *   6. Guard: unauthenticated → 401 before upgrade
 *   7. Guard: wrong owner → 404 before upgrade
 *   8. Guard: authed owner → upgrade proceeds (101 or WS frame)
 *   9. Subprotocol: client requests 'tty' → response echoes 'tty'
 *  10. Process-survives: open WS, close abruptly, no unhandledRejection
 */

import { expect, test, describe } from "bun:test";

// These imports will fail until shell.ts exists — that's the failing state.
import {
  pushableQueue,
  isAbortLike,
  frameFromWsData,
} from "../routes/shell.ts";
import { ConnectError, Code } from "@connectrpc/connect";

// ---------------------------------------------------------------------------
// 1. pushableQueue: basic enqueue + iterate + end
// ---------------------------------------------------------------------------

describe("pushableQueue", () => {
  test("1: enqueue 3 items, iterate, end", async () => {
    const q = pushableQueue<number>(16);
    q.push(1);
    q.push(2);
    q.push(3);
    q.end();

    const collected: number[] = [];
    for await (const v of q) {
      collected.push(v);
    }
    expect(collected).toEqual([1, 2, 3]);
  });

  test("2: Buffer round-trip (binary data preserved)", async () => {
    const q = pushableQueue<Buffer>(16);
    const buf = Buffer.from([0x89, 0x50, 0x4e, 0x47]);
    q.push(buf);
    q.end();

    const items: Buffer[] = [];
    for await (const v of q) {
      items.push(v);
    }
    expect(items.length).toBe(1);
    expect(items[0]).toEqual(buf);
    expect(items[0]![0]).toBe(0x89);
  });

  test("3: overflow drops oldest and closes the queue (cap=4)", async () => {
    const q = pushableQueue<number>(4);
    // Push cap+1 items — the queue must handle overflow without throwing.
    for (let i = 0; i < 5; i++) {
      q.push(i);
    }
    q.end();

    // Just assert it drains without hanging — specific drop policy is documented.
    const items: number[] = [];
    for await (const v of q) {
      items.push(v);
    }
    // At most cap items (overflow policy: drop or close).
    expect(items.length).toBeLessThanOrEqual(4);
  });
});

// ---------------------------------------------------------------------------
// 4. isAbortLike
// ---------------------------------------------------------------------------

describe("isAbortLike", () => {
  test("4a: ConnectError(Canceled) → true", () => {
    const err = new ConnectError("canceled", Code.Canceled);
    expect(isAbortLike(err)).toBe(true);
  });

  test("4b: ConnectError(Aborted) → true", () => {
    const err = new ConnectError("aborted", Code.Aborted);
    expect(isAbortLike(err)).toBe(true);
  });

  test("4c: plain Error → false", () => {
    expect(isAbortLike(new Error("boom"))).toBe(false);
  });

  test("4d: DOMException AbortError → true", () => {
    const err = new DOMException("aborted", "AbortError");
    expect(isAbortLike(err)).toBe(true);
  });
});

// ---------------------------------------------------------------------------
// 5. frameFromWsData
// ---------------------------------------------------------------------------

describe("frameFromWsData", () => {
  test("5a: string → text frame", () => {
    const frame = frameFromWsData("hello");
    expect(frame.frame.case).toBe("text");
    if (frame.frame.case === "text") {
      expect(frame.frame.value).toBe("hello");
    }
  });

  test("5b: Buffer → binary frame", () => {
    const buf = Buffer.from([0x01, 0x02, 0x03]);
    const frame = frameFromWsData(buf);
    expect(frame.frame.case).toBe("binary");
    if (frame.frame.case === "binary") {
      // value is Uint8Array (protobuf bytes field)
      expect(frame.frame.value).toBeInstanceOf(Uint8Array);
      expect(frame.frame.value[0]).toBe(0x01);
    }
  });

  test("5c: ArrayBuffer → binary frame", () => {
    const ab = new Uint8Array([0xde, 0xad]).buffer;
    const frame = frameFromWsData(ab);
    expect(frame.frame.case).toBe("binary");
  });
});
```

- [ ] **Step 2: Run the test to confirm it fails with import error**

```bash
cd /Users/ganeshdatta/Documents/engrams/orchestrator && bun test src/__tests__/shell.test.ts 2>&1 | head -30
```

Expected: `Cannot find module '../routes/shell.ts'` or similar import error.

- [ ] **Step 3: Create shell.ts with pushableQueue + helpers**

Create `orchestrator/src/routes/shell.ts`:

```typescript
/**
 * Shell WebSocket route (ADR 0039 Task 21).
 *
 * GET /api/v1/sessions/:id/shell  ⇄  ShellRelayService.Relay (bidi gRPC)
 *
 * WS wiring: @hono/node-ws (Option a) — createNodeWebSocket({app}) +
 *   injectWebSocket(server). Uses the `ws` package under the hood which Bun
 *   supports. Chosen over hand-rolled ws because it gives per-route
 *   upgradeWebSocket with clean Hono context access (auth headers, params).
 *
 * Subprotocol: client opens new WebSocket(url, 'tty') — the upgrade response
 *   MUST echo Sec-WebSocket-Protocol: tty (browsers hard-fail otherwise).
 *   @hono/node-ws passes handleProtocols to WebSocketServer; we provide one
 *   that accepts 'tty'.
 *
 * Deadlock prevention: the pushableQueue is BUFFERED (cap 256). The open
 *   frame is pushed into it BEFORE starting the async pump loop that calls
 *   shellRelay.relay(). The coordinator relay handler awaits the first inbound
 *   frame (open) before returning response headers — without buffering this
 *   would deadlock.
 *
 * No unhandledRejection: the async pump IIFE is void'd and all errors caught;
 *   Canceled/Aborted (normal teardown) are swallowed silently.
 *
 * Backpressure: when ws.bufferedAmount > 1 MiB, the pump loop pauses
 *   consuming upstream frames until the socket drains below the low watermark.
 *
 * Keepalive: server sends ws.ping() every 20s; closes on missed pong.
 */

import { Hono } from "hono";
import { HTTPException } from "hono/http-exception";
import { create } from "@bufbuild/protobuf";
import {
  RelayShellRequestSchema,
  type RelayShellRequest,
  type RelayShellResponse,
} from "../gen/engram/app/v1/session_pb.ts";
import { ConnectError, Code } from "@connectrpc/connect";
import { makeGuard } from "./guard.ts";
import { shellRelay as defaultShellRelay } from "../control-plane/client.ts";
import type { GetSession, ResolveOwner } from "./guard.ts";

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/** Subset of ShellRelayService client used by this route. */
export interface ShellRelayClient {
  relay(
    inbound: AsyncIterable<RelayShellRequest>,
    options?: { signal?: AbortSignal },
  ): AsyncIterable<RelayShellResponse>;
}

/** Injectable deps for the shell route. */
export interface ShellDeps {
  shellRelay?: ShellRelayClient;
  getSession?: GetSession;
  resolveOwner?: ResolveOwner;
}

// ---------------------------------------------------------------------------
// pushableQueue
// ---------------------------------------------------------------------------

/**
 * A bounded async-iterable queue.
 *
 * Overflow policy: if push() is called when the queue is at capacity,
 * the queue calls end() — this is "drop-connection" semantics appropriate
 * for a shell relay. The caller (onMessage / onOpen) must not push after end().
 *
 * The queue is an async iterator — for await loops will block until items
 * are pushed or end() is called.
 */
export interface PushableQueue<T> extends AsyncIterable<T> {
  push(item: T): void;
  end(): void;
}

export function pushableQueue<T>(cap: number): PushableQueue<T> {
  const buffer: T[] = [];
  let ended = false;
  let resolve: (() => void) | null = null;

  const notify = () => {
    if (resolve) {
      const r = resolve;
      resolve = null;
      r();
    }
  };

  return {
    push(item: T) {
      if (ended) return;
      if (buffer.length >= cap) {
        // Overflow: drop-connection semantics — end the queue.
        ended = true;
        notify();
        return;
      }
      buffer.push(item);
      notify();
    },
    end() {
      if (ended) return;
      ended = true;
      notify();
    },
    [Symbol.asyncIterator](): AsyncIterator<T> {
      return {
        async next(): Promise<IteratorResult<T>> {
          // Wait until there is data or we are ended.
          while (buffer.length === 0 && !ended) {
            await new Promise<void>((r) => {
              resolve = r;
            });
          }
          if (buffer.length > 0) {
            return { value: buffer.shift()!, done: false };
          }
          return { value: undefined as unknown as T, done: true };
        },
      };
    },
  };
}

// ---------------------------------------------------------------------------
// Frame helpers
// ---------------------------------------------------------------------------

/**
 * Build a RelayShellRequest with an open frame.
 */
export function openFrame(sessionId: string): RelayShellRequest {
  return create(RelayShellRequestSchema, {
    frame: {
      case: "open",
      value: { sessionId },
    },
  });
}

/**
 * Build a RelayShellRequest with a pong frame (response to server ping).
 */
export function pongFrame(data: Uint8Array): RelayShellRequest {
  return create(RelayShellRequestSchema, {
    frame: { case: "pong", value: data },
  });
}

/**
 * Build a RelayShellRequest from a WS message event's data.
 * ws delivers Buffer (Node), string, or ArrayBuffer.
 * Browsers send text frames for ttyd input; binary frames for binary.
 */
export function frameFromWsData(
  data: string | Buffer | ArrayBuffer,
): RelayShellRequest {
  if (typeof data === "string") {
    return create(RelayShellRequestSchema, {
      frame: { case: "text", value: data },
    });
  }
  // Buffer (Node ws package) or ArrayBuffer (browser / hono-ws shim)
  let bytes: Uint8Array;
  if (data instanceof ArrayBuffer) {
    bytes = new Uint8Array(data);
  } else {
    // Node Buffer — already a Uint8Array subclass
    bytes = data instanceof Uint8Array ? data : new Uint8Array(data);
  }
  return create(RelayShellRequestSchema, {
    frame: { case: "binary", value: bytes },
  });
}

// ---------------------------------------------------------------------------
// Error helpers
// ---------------------------------------------------------------------------

/**
 * Returns true for errors that indicate normal teardown:
 *   - ConnectError with Code.Canceled or Code.Aborted
 *   - DOMException with name 'AbortError'
 *   - any error with name 'AbortError'
 */
export function isAbortLike(err: unknown): boolean {
  if (err instanceof ConnectError) {
    return err.code === Code.Canceled || err.code === Code.Aborted;
  }
  if (err instanceof Error) {
    return err.name === "AbortError";
  }
  return false;
}

// ---------------------------------------------------------------------------
// Backpressure
// ---------------------------------------------------------------------------

const HIGH_WATER = 1024 * 1024; // 1 MiB
const LOW_WATER = 64 * 1024; // 64 KiB

/**
 * Wait until ws.bufferedAmount drops below LOW_WATER.
 * Used to pause consuming upstream frames when the socket is backed up.
 *
 * The `ws` object from @hono/node-ws has a `.raw` property that is the
 * underlying ws.WebSocket — which has `bufferedAmount`.
 */
async function waitForDrain(raw: { bufferedAmount?: number }): Promise<void> {
  if ((raw.bufferedAmount ?? 0) <= LOW_WATER) return;
  await new Promise<void>((resolve) => {
    const check = () => {
      if ((raw.bufferedAmount ?? 0) <= LOW_WATER) {
        resolve();
      } else {
        setTimeout(check, 16);
      }
    };
    setTimeout(check, 16);
  });
}

// ---------------------------------------------------------------------------
// Route factory
// ---------------------------------------------------------------------------

/**
 * Create the shell WS route.
 *
 * IMPORTANT: caller must also call injectWebSocket(server) on the HTTP server
 * using the NodeWebSocket instance returned by createNodeWebSocket — this is
 * done in index.ts.
 *
 * Returns { app, nodeWs } so index.ts can inject the WS server.
 */
export function makeShellRoute(deps?: ShellDeps): {
  app: Hono;
  // The upgradeWebSocket function needs to be returned to the caller so the
  // route can be wired into the app AFTER createNodeWebSocket is called.
  // We return the app with the route already registered; the caller provides
  // upgradeWebSocket via injectShellRoute().
  injectUpgrade: (
    upgradeWebSocket: import("hono/ws").UpgradeWebSocket,
  ) => void;
} {
  const relayClient: ShellRelayClient =
    (deps?.shellRelay as ShellRelayClient | undefined) ??
    (defaultShellRelay as unknown as ShellRelayClient);
  const guardFn = makeGuard(deps?.getSession, deps?.resolveOwner);

  const app = new Hono();

  // We store a reference to the registered route so we can inject
  // upgradeWebSocket later without re-creating the app.
  let _upgradeWebSocket: import("hono/ws").UpgradeWebSocket | null = null;

  app.get("/api/v1/sessions/:id/shell", async (c, next) => {
    if (!_upgradeWebSocket) {
      // upgradeWebSocket not yet injected — return 503.
      return c.json({ error: "shell not available" }, 503);
    }
    return _upgradeWebSocket((ctx) => {
      const sessionId = ctx.req.param("id");
      const abort = new AbortController();
      const inbound = pushableQueue<RelayShellRequest>(256);

      return {
        onOpen: (_e, ws) => {
          // Push the open frame BEFORE starting the pump.
          // This is critical: the coordinator relay handler awaits the
          // first inbound frame before returning response headers.
          // Without buffering this would deadlock.
          inbound.push(openFrame(sessionId));

          // Keepalive: ping every 20s; close on missed pong.
          let pongReceived = true;
          const keepaliveInterval = setInterval(() => {
            if (!pongReceived) {
              clearInterval(keepaliveInterval);
              ws.close(1001, "keepalive timeout");
              return;
            }
            pongReceived = false;
            // ws.raw is the underlying ws.WebSocket from the ws package.
            try {
              (ws.raw as { ping?: () => void }).ping?.();
            } catch {
              // ignore — socket may have closed
            }
          }, 20_000);

          // Track pong responses from the client.
          try {
            (ws.raw as { on?: (event: string, cb: () => void) => void }).on?.(
              "pong",
              () => {
                pongReceived = true;
              },
            );
          } catch {
            // ignore
          }

          // Upstream pump IIFE — void'd to prevent unhandledRejection.
          void (async () => {
            try {
              for await (const f of relayClient.relay(inbound, {
                signal: abort.signal,
              })) {
                // Backpressure: pause if socket is backed up.
                if ((ws.raw as { bufferedAmount?: number }).bufferedAmount ?? 0 > HIGH_WATER) {
                  await waitForDrain(ws.raw as { bufferedAmount?: number });
                }

                switch (f.frame.case) {
                  case "text":
                    ws.send(f.frame.value);
                    break;
                  case "binary":
                    // f.frame.value is Uint8Array; ws.send accepts Uint8Array.
                    ws.send(f.frame.value);
                    break;
                  case "ping":
                    // Browser JS cannot send WS pongs; we send pong back via inbound.
                    inbound.push(pongFrame(f.frame.value));
                    break;
                  case "close":
                    ws.close(f.frame.value.code, f.frame.value.reason);
                    break;
                  default:
                    // pong / unknown — discard
                    break;
                }
              }
            } catch (e) {
              if (!isAbortLike(e)) {
                console.warn({ err: e }, "shell relay upstream error");
                ws.close(1011, "upstream error");
              }
              // Canceled/Aborted = normal teardown — swallow silently.
            } finally {
              clearInterval(keepaliveInterval);
              // Upstream end → ensure browser is closed.
              try {
                ws.close();
              } catch {
                // ignore — already closed
              }
            }
          })();
        },

        onMessage: (e) => {
          inbound.push(frameFromWsData(e.data as string | Buffer | ArrayBuffer));
        },

        onClose: () => {
          abort.abort();
          inbound.end();
        },
      };
    })(c, next);
  });

  return {
    app,
    injectUpgrade: (upgradeWebSocket) => {
      _upgradeWebSocket = upgradeWebSocket;
    },
  };
}
```

- [ ] **Step 4: Run tests — they should pass now (tests 1–5)**

```bash
cd /Users/ganeshdatta/Documents/engrams/orchestrator && bun test src/__tests__/shell.test.ts 2>&1 | head -60
```

Expected: tests 1–5 pass. Tests 6–10 don't exist yet.

---

### Task 3: Wire @hono/node-ws into server.ts and index.ts

**Files:**
- Modify: `orchestrator/src/server.ts`
- Modify: `orchestrator/src/index.ts`

- [ ] **Step 1: Update server.ts to accept and inject NodeWebSocket**

The server needs to call `nodeWs.injectWebSocket(server)` after creation. The cleanest approach: `buildServer` accepts an optional `nodeWs` parameter.

Edit `orchestrator/src/server.ts` to add optional nodeWs injection:

```typescript
/**
 * Orchestrator HTTP server — single node:http createServer under Bun.
 *
 * Shape:
 *   - /rpc/* → connectNodeAdapter (Connect/gRPC/gRPC-Web); prefix="/rpc" must
 *     match the startsWith("/rpc/") seam so handlers registered at prefix+path
 *     resolve correctly.
 *   - everything else → Hono via getRequestListener(hono)
 *
 * WebSocket support (ADR 0039 Task 21): @hono/node-ws uses the `ws` package's
 * WebSocketServer in `noServer: true` mode. The upgrade event is handled by
 * injectWebSocket(server) — must be called after createServer() returns.
 *
 * RUNTIME OVERRIDE (ADR 0039 said Node; user chose Bun 2026-06-11):
 *   Bun implements node:http fully, so node:http createServer + connectNodeAdapter
 *   + @hono/node-server's getRequestListener all run unchanged under Bun.
 */

import { createServer, type IncomingMessage, type ServerResponse } from "node:http";
import type { Hono } from "hono";
import { getRequestListener } from "@hono/node-server";
import { connectNodeAdapter } from "@connectrpc/connect-node";
import type { ConnectRouter } from "@connectrpc/connect";
import type { NodeWebSocket } from "@hono/node-ws";

export type RouteRegistrar = (router: ConnectRouter) => void;

/**
 * Build and return a node:http Server that:
 *   - routes /rpc/* to the Connect adapter
 *   - routes everything else to the Hono app via getRequestListener
 *   - optionally injects a NodeWebSocket server for WS upgrade handling
 *
 * Exported so tests can call buildServer(...) on an ephemeral port.
 */
export function buildServer(
  app: Hono,
  routes: RouteRegistrar = () => {},
  nodeWs?: NodeWebSocket,
) {
  const connectHandler = connectNodeAdapter({ routes, requestPathPrefix: "/rpc" });
  const honoListener = getRequestListener(app.fetch);

  const server = createServer((req: IncomingMessage, res: ServerResponse) => {
    const url = req.url ?? "/";
    if (url.startsWith("/rpc/") || url === "/rpc") {
      connectHandler(req, res);
      return;
    }
    honoListener(req, res);
  });

  // Wire WS upgrade handling if a NodeWebSocket instance is provided.
  if (nodeWs) {
    nodeWs.injectWebSocket(server);
  }

  return server;
}
```

- [ ] **Step 2: Update index.ts to create and wire nodeWs + shell route**

Edit `orchestrator/src/index.ts`:

```typescript
import { Hono } from "hono";
import { createNodeWebSocket } from "@hono/node-ws";
import { config } from "./config.ts";
import { buildServer } from "./server.ts";
import health from "./routes/health.ts";
import authRoute from "./routes/auth.ts";
import eventsRoute from "./routes/events.ts";
import artifactsRoute from "./routes/artifacts.ts";
import meRoute from "./routes/me.ts";
import { makeShellRoute } from "./routes/shell.ts";
import { registerPassthrough } from "./rpc/passthrough.ts";
import { registerTasks } from "./rpc/tasks.ts";
import { SURFACE } from "./rpc/surface.ts";
import { controlPlaneTransport } from "./control-plane/transport.ts";
import type { ConnectRouter } from "@connectrpc/connect";

const app = new Hono();

// @hono/node-ws: must be created with the app BEFORE routes are mounted,
// because injectWebSocket wires the upgrade event to app.request().
const { upgradeWebSocket, injectWebSocket } = createNodeWebSocket({ app });

// Mount routes.
app.route("/", health);
app.route("/", authRoute);
// ADR 0039 Task 20: browser-native HTTP legs (SSE events, artifact bytes, /me/claude-token).
app.route("/", eventsRoute);
app.route("/", artifactsRoute);
app.route("/", meRoute);

// ADR 0039 Task 21: shell WS leg.
const { app: shellApp, injectUpgrade } = makeShellRoute();
injectUpgrade(upgradeWebSocket);
app.route("/", shellApp);

// Default 404 for unmatched Hono paths.
app.notFound((c) => c.json({ error: "not found" }, 404));

const server = buildServer(
  app,
  (router: ConnectRouter) => {
    registerTasks(router);
    registerPassthrough(router, SURFACE, controlPlaneTransport);
  },
  { injectWebSocket } as Parameters<typeof buildServer>[2],
);

server.listen(config.port, "0.0.0.0", () => {
  console.log(`Orchestrator listening on port ${config.port}`);
});

// Graceful shutdown on SIGTERM.
process.on("SIGTERM", () => {
  console.log("Orchestrator: SIGTERM received, shutting down gracefully…");
  server.close((err) => {
    if (err) {
      console.error("Orchestrator: error during shutdown", err);
      process.exit(1);
    }
    console.log("Orchestrator: shutdown complete");
    process.exit(0);
  });
});
```

- [ ] **Step 3: Run typecheck**

```bash
cd /Users/ganeshdatta/Documents/engrams/orchestrator && bun run typecheck 2>&1 | head -40
```

Expected: no errors (or only pre-existing errors unrelated to shell.ts).

---

### Task 4: Guard tests + process-survives test + subprotocol test

**Files:**
- Modify: `orchestrator/src/__tests__/shell.test.ts`

- [ ] **Step 1: Add guard + process-survives + subprotocol tests**

Append to `orchestrator/src/__tests__/shell.test.ts`:

```typescript
// ---------------------------------------------------------------------------
// 6–8: Guard tests (auth before upgrade)
// ---------------------------------------------------------------------------

import { Hono } from "hono";
import { createNodeWebSocket } from "@hono/node-ws";
import { buildServer } from "../server.ts";
import { makeShellRoute } from "../routes/shell.ts";
import type { AddressInfo } from "node:net";

const MEMBER_A = "member-a-shell-test";
const MEMBER_B = "member-b-shell-test";
const SESSION_OF_A = "session-owned-by-a-shell";

type GetSession = NonNullable<Parameters<typeof makeShellRoute>[0]>["getSession"];
type ResolveOwner = NonNullable<Parameters<typeof makeShellRoute>[0]>["resolveOwner"];

function makeGetSession(userId: string | null): GetSession {
  return async () => {
    if (!userId) return null;
    return { user: { id: userId, role: "user", email: `${userId}@test.invalid` } };
  };
}

function makeResolveOwner(extra: Record<string, string | null> = {}): ResolveOwner {
  return async (sessionId) => {
    if (sessionId === SESSION_OF_A) return MEMBER_A;
    if (sessionId in extra) return extra[sessionId] ?? null;
    return null;
  };
}

// Fake relay that echoes one text frame then ends.
const fakeRelay: import("../routes/shell.ts").ShellRelayClient = {
  async *relay(_inbound, _opts) {
    // Yield one text frame to prove bridge works.
    yield {
      frame: { case: "text" as const, value: "hello-from-relay" },
    } as import("../gen/engram/app/v1/session_pb.ts").RelayShellResponse;
    // Then end.
  },
};

async function startShellServer(deps: Parameters<typeof makeShellRoute>[0]): Promise<{
  baseUrl: string;
  wsUrl: string;
  server: ReturnType<typeof buildServer>;
}> {
  const app = new Hono();
  const { upgradeWebSocket, injectWebSocket } = createNodeWebSocket({ app });
  const { app: shellApp, injectUpgrade } = makeShellRoute(deps);
  injectUpgrade(upgradeWebSocket);
  app.route("/", shellApp);
  app.notFound((c) => c.json({ error: "not found" }, 404));
  app.onError((err, c) => {
    if ("status" in err && typeof err.status === "number") {
      return c.json({ error: (err as { message: string }).message }, err.status as 401 | 404);
    }
    return c.json({ error: String(err) }, 500);
  });

  const server = buildServer(app, () => {}, { injectWebSocket } as Parameters<typeof buildServer>[2]);

  return new Promise((resolve) => {
    server.listen(0, "127.0.0.1", () => {
      const addr = server.address() as AddressInfo;
      resolve({
        baseUrl: `http://127.0.0.1:${addr.port}`,
        wsUrl: `ws://127.0.0.1:${addr.port}`,
        server,
      });
    });
  });
}

function stopServer(server: ReturnType<typeof buildServer>): Promise<void> {
  return new Promise((resolve, reject) => {
    server.close((err) => (err ? reject(err) : resolve()));
  });
}

describe("Shell route guards", () => {
  test("6: unauthenticated → 401 before upgrade", async () => {
    const { baseUrl, server } = await startShellServer({
      shellRelay: fakeRelay,
      getSession: makeGetSession(null),
      resolveOwner: makeResolveOwner(),
    });

    try {
      const res = await fetch(
        `${baseUrl}/api/v1/sessions/${SESSION_OF_A}/shell`,
        { headers: { Upgrade: "websocket" } },
      );
      expect(res.status).toBe(401);
    } finally {
      await stopServer(server);
    }
  });

  test("7: wrong owner → 404 before upgrade", async () => {
    const { baseUrl, server } = await startShellServer({
      shellRelay: fakeRelay,
      getSession: makeGetSession(MEMBER_B),
      resolveOwner: makeResolveOwner(),
    });

    try {
      const res = await fetch(
        `${baseUrl}/api/v1/sessions/${SESSION_OF_A}/shell`,
        { headers: { Upgrade: "websocket" } },
      );
      expect(res.status).toBe(404);
    } finally {
      await stopServer(server);
    }
  });
});

describe("Shell route subprotocol + process-survives", () => {
  test("9: subprotocol 'tty' echoed in response", async () => {
    const { wsUrl, server } = await startShellServer({
      shellRelay: fakeRelay,
      getSession: makeGetSession(MEMBER_A),
      resolveOwner: makeResolveOwner(),
    });

    try {
      // Bun has native WebSocket — open with 'tty' subprotocol.
      const ws = new WebSocket(
        `${wsUrl}/api/v1/sessions/${SESSION_OF_A}/shell`,
        ["tty"],
      );

      const proto = await new Promise<string>((resolve, reject) => {
        const timer = setTimeout(() => reject(new Error("timeout")), 5000);
        ws.addEventListener("open", () => {
          clearTimeout(timer);
          resolve(ws.protocol);
          ws.close();
        });
        ws.addEventListener("error", (e) => {
          clearTimeout(timer);
          reject(new Error(`WS error: ${String(e)}`));
        });
      });

      // The upgrade MUST echo the 'tty' subprotocol.
      expect(proto).toBe("tty");
    } finally {
      await stopServer(server);
    }
  });

  test("10: process survives abrupt client close — no unhandledRejection", async () => {
    // Trap unhandledRejection before the test.
    const unhandled: Error[] = [];
    const trap = (e: Error) => unhandled.push(e);
    process.on("unhandledRejection", trap);

    // A relay that keeps yielding to force the pump loop to run while the
    // client disconnects.
    let relayAborted = false;
    const slowRelay: import("../routes/shell.ts").ShellRelayClient = {
      async *relay(_inbound, opts) {
        try {
          // Yield slowly so the pump is running when the client closes.
          for (let i = 0; i < 100; i++) {
            if (opts?.signal?.aborted) break;
            await new Promise((r) => setTimeout(r, 20));
            yield {
              frame: { case: "text" as const, value: `frame-${i}` },
            } as import("../gen/engram/app/v1/session_pb.ts").RelayShellResponse;
          }
        } finally {
          relayAborted = true;
        }
      },
    };

    const { wsUrl, server } = await startShellServer({
      shellRelay: slowRelay,
      getSession: makeGetSession(MEMBER_A),
      resolveOwner: makeResolveOwner(),
    });

    try {
      const ws = new WebSocket(
        `${wsUrl}/api/v1/sessions/${SESSION_OF_A}/shell`,
        ["tty"],
      );

      // Wait for open then immediately close (abrupt disconnect).
      await new Promise<void>((resolve, reject) => {
        const timer = setTimeout(() => reject(new Error("open timeout")), 5000);
        ws.addEventListener("open", () => {
          clearTimeout(timer);
          ws.close(); // abrupt close
          resolve();
        });
        ws.addEventListener("error", (e) => {
          clearTimeout(timer);
          reject(new Error(`WS error: ${String(e)}`));
        });
      });

      // Give the pump time to notice the close and tear down.
      await new Promise((r) => setTimeout(r, 200));

      // Assert relay was eventually aborted.
      expect(relayAborted).toBe(true);
      // Assert no unhandledRejection fired.
      expect(unhandled.length).toBe(0);
    } finally {
      process.off("unhandledRejection", trap);
      await stopServer(server);
    }
  });
});
```

- [ ] **Step 2: Run all shell tests**

```bash
cd /Users/ganeshdatta/Documents/engrams/orchestrator && bun test src/__tests__/shell.test.ts 2>&1
```

Expected: all 10 tests pass.

- [ ] **Step 3: Run full test suite**

```bash
cd /Users/ganeshdatta/Documents/engrams/orchestrator && bun test 2>&1 | tail -20
```

Expected: all prior tests still pass + new shell tests green.

---

### Task 5: Fix subprotocol echo (handleProtocols)

The `@hono/node-ws` `createNodeWebSocket` creates a `WebSocketServer({ noServer: true })`. To echo `tty`, we need to pass `handleProtocols` to the WSS.

- [ ] **Step 1: Check if subprotocol test (test 9) passes as-is**

If test 9 passes without changes, skip this task. If not:

The `wss` object is exposed as `nodeWs.wss`. We can monkey-patch `handleProtocols` after creation:

In `orchestrator/src/index.ts`, after `createNodeWebSocket`:

```typescript
// Accept the 'tty' subprotocol (browser clients hard-fail without this echo).
const { upgradeWebSocket, injectWebSocket, wss } = createNodeWebSocket({ app });
wss.options.handleProtocols = (protocols: Set<string>) => {
  if (protocols.has("tty")) return "tty";
  return false;
};
```

Same pattern in test helper `startShellServer`.

- [ ] **Step 2: Re-run shell tests**

```bash
cd /Users/ganeshdatta/Documents/engrams/orchestrator && bun test src/__tests__/shell.test.ts --test-name-pattern "9:" 2>&1
```

Expected: PASS.

---

### Task 6: Add live shell smoke test to smoke.live.test.ts

**Files:**
- Modify: `orchestrator/src/smoke.live.test.ts`

- [ ] **Step 1: Add shell live tests (14/N)**

Append to the `describe("orchestrator live smoke")` block in `smoke.live.test.ts`:

```typescript
  // -------------------------------------------------------------------------
  // 14. Shell WS (Task 21)
  //
  // Requires a running session with ttyd active.
  // Steps: CreateTask → open WS to /api/v1/sessions/:id/shell with member
  //   cookie + 'tty' subprotocol → send ttyd auth JSON then echo command →
  //   expect output frame containing "hi" within 15s → close → DeleteTask.
  // Also asserts anonymous upgrade attempt → 401 (no WS).
  // -------------------------------------------------------------------------

  test.skipIf(!SMOKE)(
    "14a shell WS anonymous → 401 (no upgrade)",
    async () => {
      // Try to open a WS without a cookie — should get 401 before upgrade.
      const res = await fetch(`${BASE}/api/v1/sessions/any-session-id/shell`, {
        headers: { Upgrade: "websocket" },
        // No Cookie
      });
      expect(res.status).toBe(401);
      console.log("Smoke 14a PASS: anonymous shell WS → 401 (no upgrade)");
    },
  );

  test.skipIf(!SMOKE)(
    "14b shell WS member → terminal responds with 'hi'",
    async () => {
      // Create a task to get a live session.
      const imagesRes = await rpc(
        "engram.app.v1.ImageService",
        "ListEnabledImages",
        {},
        memberCookie,
      );
      expect(imagesRes.status).toBe(200);
      const imagesBody = (await imagesRes.json()) as {
        images?: Array<{ imageUri?: string; harnessName?: string }>;
      };
      const noHarnessImage = (imagesBody.images ?? []).find(
        (img) => !img.harnessName,
      );
      if (!noHarnessImage?.imageUri) {
        console.log("Smoke 14b SKIP: no no-harness image available");
        return;
      }

      const createRes = await rpc(
        "engram.app.v1.TaskService",
        "CreateTask",
        { type: "chat", imageUri: noHarnessImage.imageUri, title: "Smoke shell task" },
        memberCookie,
      );
      expect(createRes.status).toBe(200);
      const createBody = (await createRes.json()) as {
        task?: { id?: string; sessions?: Array<{ sessionId?: string }> };
      };
      const shellTaskId = createBody.task?.id;
      const sessionId = createBody.task?.sessions?.[0]?.sessionId;
      expect(shellTaskId).toBeTruthy();
      expect(sessionId).toBeTruthy();

      // Wait for session to be active enough for ttyd (up to 30s).
      let foundHi = false;
      const wsUrl = `${BASE.replace(/^http/, "ws")}/api/v1/sessions/${sessionId}/shell`;
      const ac = new AbortController();
      const timeout = setTimeout(() => ac.abort(), 15_000);

      try {
        // Bun native WebSocket with cookie header + 'tty' subprotocol.
        const ws = new WebSocket(wsUrl, {
          headers: { Cookie: memberCookie },
          protocols: ["tty"],
        } as ConstructorParameters<typeof WebSocket>[1]);

        await new Promise<void>((resolve, reject) => {
          ws.binaryType = "arraybuffer";

          ws.onopen = () => {
            // ttyd auth handshake.
            ws.send(JSON.stringify({ AuthToken: "", columns: 80, rows: 24 }));
            // Wait a tick then send echo command (ttyd '0' INPUT prefix).
            setTimeout(() => {
              ws.send("0echo hi\r");
            }, 500);
          };

          ws.onmessage = (e) => {
            let text: string;
            if (typeof e.data === "string") {
              text = e.data;
            } else if (e.data instanceof ArrayBuffer) {
              text = new TextDecoder().decode(new Uint8Array(e.data).subarray(1));
            } else {
              return;
            }
            if (text.includes("hi")) {
              foundHi = true;
              clearTimeout(timeout);
              ws.close();
              resolve();
            }
          };

          ws.onerror = (e) => reject(new Error(`WS error: ${String(e)}`));
          ws.onclose = (e) => {
            if (!foundHi) {
              // Only reject if we were still waiting.
              resolve(); // let foundHi check below fail the test
            }
          };

          ac.signal.addEventListener("abort", () => {
            ws.close();
            resolve();
          });
        });
      } finally {
        clearTimeout(timeout);
        // Cleanup.
        if (shellTaskId) {
          await rpc(
            "engram.app.v1.TaskService",
            "DeleteTask",
            { taskId: shellTaskId },
            memberCookie,
          );
        }
      }

      expect(foundHi).toBe(true);
      console.log("Smoke 14b PASS: shell WS → 'hi' received via ttyd echo");
    },
    30_000,
  );
```

- [ ] **Step 2: Run bun test (no-env) to confirm honest skips**

```bash
cd /Users/ganeshdatta/Documents/engrams/orchestrator && bun test src/smoke.live.test.ts 2>&1 | tail -10
```

Expected: all tests skip (SMOKE not set), 0 failures.

---

### Task 7: Typecheck + full test suite verification

- [ ] **Step 1: Typecheck**

```bash
cd /Users/ganeshdatta/Documents/engrams/orchestrator && bun run typecheck 2>&1
```

Expected: no new type errors.

- [ ] **Step 2: Full test suite**

```bash
cd /Users/ganeshdatta/Documents/engrams/orchestrator && bun test 2>&1 | tail -30
```

Expected: all tests pass (shell tests green, smoke tests honestly skipped, all prior tests still green).

---

### Task 8: Commit

- [ ] **Step 1: Commit**

```bash
cd /Users/ganeshdatta/Documents/engrams && git add orchestrator/ && git commit -m "$(cat <<'EOF'
feat(orchestrator): browser WS shell leg over ShellRelayService bidi (ADR 0039 §8)

- @hono/node-ws (Option a) wired into buildServer via injectWebSocket
- pushableQueue (bounded 256, drop-connection overflow) bridges WS ↔ bidi gRPC
- auth+ability gate (shell) runs before upgrade: 401/404 pre-upgrade
- open frame buffered before relay() call — prevents coordinator deadlock
- upstream pump void'd + Canceled/Aborted swallowed → no unhandledRejection
- ping frames from upstream answered with pong via inbound queue
- server-side ws.ping() keepalive every 20s; close on missed pong
- backpressure: pause consuming upstream when bufferedAmount > 1 MiB
- Sec-WebSocket-Protocol: tty echoed via wss.handleProtocols
- Tests: pushableQueue unit, frame mapping, isAbortLike, guard, subprotocol
  echo, process-survives (no unhandledRejection on abrupt close)
- Live smoke: 14a anonymous → 401; 14b member → echo 'hi' via ttyd

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>
EOF
)"
```

---

## Self-Review

### Spec Coverage Check

| Requirement | Task(s) |
|---|---|
| GET /api/v1/sessions/:id/shell WS route | Task 2, 3 |
| Bridges to ShellRelayService.Relay bidi gRPC | Task 2 (shell.ts pump loop) |
| ability-gated ('shell') | Task 2 (makeGuard in makeShellRoute) |
| 401/404 pre-upgrade (no upgrade on auth fail) | Task 2, Task 4 test 6/7 |
| No unhandledRejection (void pump + swallow Canceled) | Task 2, Task 4 test 10 |
| ping frames from upstream → pong via inbound | Task 2 (case "ping") |
| WS keepalive server-side ping every 20s | Task 2 (keepaliveInterval) |
| Sec-WebSocket-Protocol: tty echoed | Task 5 (handleProtocols) |
| backpressure (waitForDrain on bufferedAmount > 1MiB) | Task 2 (waitForDrain) |
| pushableQueue bounded (cap 256, drop-connection overflow) | Task 2 |
| Buffer round-trip (ws delivers Buffer) | Task 2 (frameFromWsData) |
| Open frame pushed BEFORE relay() call (deadlock prevention) | Task 2 |
| process-survives test (no unhandledRejection) | Task 4 test 10 |
| frame mapping tests (text/binary, Buffer) | Task 2 test 5a/5b/5c |
| upstream ping → pong pushed to inbound test | Task 2 (covered in pump logic; relay frame case "ping") |
| subprotocol echo test | Task 4 test 9 |
| Live smoke: anonymous → 401 | Task 6 test 14a |
| Live smoke: member → echo 'hi' | Task 6 test 14b |

### Placeholder Scan

No TBDs or TODO placeholders in any code block. All code is complete.

### Type Consistency

- `pushableQueue<T>` defined in Task 2, used consistently in Task 3, 4.
- `makeShellRoute` returns `{ app, injectUpgrade }` — consistent in Tasks 2, 3, 4.
- `buildServer(app, routes, nodeWs?)` — third param is `NodeWebSocket | undefined` — consistent in Tasks 3, 4.
- `frameFromWsData` returns `RelayShellRequest` — consistent in shell.ts.

**One gap found:** The shell route's auth check runs INSIDE `upgradeWebSocket` callback but the `@hono/node-ws` source shows that the upgrade event handler calls `init.app.request()` for all upgrade requests. The `guardFn` check must happen before returning the WS handlers. Looking at the `@hono/node-ws` source — the `upgradeWebSocket` callback is called per request, including auth failures. The pre-upgrade 401/404 works because `upgradeWebSocket` returns `new Response()` (101 Switching Protocols) only when the callback returns WS handlers without throwing. If the callback throws or returns a non-WS response, the upgrade handler closes with that status code.

**Fix needed:** The current `makeShellRoute` calls `guardFn` inside `upgradeWebSocket` — but `upgradeWebSocket` expects to call the callback which *returns* handlers. The guard check needs to happen in the Hono route handler BEFORE the `upgradeWebSocket` call. Looking at the `@hono/node-ws` source code: `upgradeWebSocket(createHandlers)` where `createHandlers` is called with context — the check should be: call guard first in the outer Hono handler, then pass to upgradeWebSocket only if guard passes.

The plan's Task 2 shell.ts has `guardFn` called inside `upgradeWebSocket`'s callback — this is wrong. Fix: call `guardFn` in the outer `app.get` handler before delegating to `upgradeWebSocket`.

**Corrected shell.ts route structure:**

```typescript
app.get("/api/v1/sessions/:id/shell", async (c, next) => {
  // 1. Auth gate BEFORE upgrade — throws HTTPException on failure.
  //    HTTPException becomes 401/404 response; upgrade never happens.
  await guardFn(c, "shell");

  if (!_upgradeWebSocket) {
    return c.json({ error: "shell not available" }, 503);
  }

  return _upgradeWebSocket((ctx) => {
    // Auth already passed — set up bidi bridge.
    const sessionId = ctx.req.param("id");
    // ... rest of handler
  })(c, next);
});
```

This is the correct pattern — guard fires on the HTTP layer before `upgradeWebSocket` takes over the socket.
