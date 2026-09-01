/**
 * Unit tests for the shell WS route (ADR 0051 Task 21).
 * No SMOKE env needed for these tests.
 */
import { expect, test, describe } from "bun:test";
import WebSocketClient from "ws";
import {
  pushableQueue,
  isAbortLike,
  frameFromWsData,
} from "../routes/shell.ts";
import { ConnectError, Code } from "@connectrpc/connect";

describe("pushableQueue", () => {
  test("1: enqueue 3 items, iterate, end", async () => {
    const q = pushableQueue<number>(16);
    q.push(1); q.push(2); q.push(3); q.end();
    const collected: number[] = [];
    for await (const v of q) collected.push(v);
    expect(collected).toEqual([1, 2, 3]);
  });

  test("2: Buffer round-trip (binary data preserved)", async () => {
    const q = pushableQueue<Buffer>(16);
    const buf = Buffer.from([0x89, 0x50, 0x4e, 0x47]);
    q.push(buf); q.end();
    const items: Buffer[] = [];
    for await (const v of q) items.push(v);
    expect(items.length).toBe(1);
    expect(items[0]![0]).toBe(0x89);
  });

  test("3: overflow (cap=4) does not hang", async () => {
    const q = pushableQueue<number>(4);
    for (let i = 0; i < 6; i++) q.push(i);
    q.end();
    const items: number[] = [];
    for await (const v of q) items.push(v);
    expect(items.length).toBeLessThanOrEqual(4);
  });
});

describe("isAbortLike", () => {
  test("4a: ConnectError(Canceled) → true", () => {
    expect(isAbortLike(new ConnectError("canceled", Code.Canceled))).toBe(true);
  });
  test("4b: ConnectError(Aborted) → true", () => {
    expect(isAbortLike(new ConnectError("aborted", Code.Aborted))).toBe(true);
  });
  test("4c: plain Error → false", () => {
    expect(isAbortLike(new Error("boom"))).toBe(false);
  });
  test("4d: DOMException AbortError → true", () => {
    expect(isAbortLike(new DOMException("aborted", "AbortError"))).toBe(true);
  });
});

describe("frameFromWsData", () => {
  test("5a: string → text frame", () => {
    const frame = frameFromWsData("hello");
    expect(frame.frame.case).toBe("text");
    if (frame.frame.case === "text") expect(frame.frame.value).toBe("hello");
  });
  test("5b: Buffer → binary frame with correct bytes", () => {
    const buf = Buffer.from([0x01, 0x02, 0x03]);
    const frame = frameFromWsData(buf);
    expect(frame.frame.case).toBe("binary");
    if (frame.frame.case === "binary") {
      expect(frame.frame.value).toBeInstanceOf(Uint8Array);
      expect(frame.frame.value[0]).toBe(0x01);
    }
  });
  test("5c: ArrayBuffer → binary frame", () => {
    const ab = new Uint8Array([0xde, 0xad]).buffer;
    expect(frameFromWsData(ab).frame.case).toBe("binary");
  });
});

// ---------------------------------------------------------------------------
// Server-level tests (guard + subprotocol + process-survives + THE regression)
// ---------------------------------------------------------------------------

import { Hono } from "hono";
import { buildServer } from "../server.ts";
import { makeShellUpgradeHandler } from "../routes/shell.ts";
import type { ShellDeps } from "../routes/shell.ts";
import type { RelayShellResponse } from "../gen/engram/app/v1/session_pb.ts";
import type { AddressInfo } from "node:net";

const MEMBER_A_WS = "member-a-ws-test";
const SESSION_OF_A_WS = "session-owned-by-a-ws";

function makeGetSessionWs(userId: string | null): ShellDeps["getSession"] {
  return async () =>
    userId ? { user: { id: userId, role: "user", email: `${userId}@test` } } : null;
}

function makeResolveOwnerWs(): ShellDeps["resolveOwner"] {
  return async (sid: string) => (sid === SESSION_OF_A_WS ? MEMBER_A_WS : null);
}

// Fake relay: keeps yielding text frames until aborted.
const slowFakeRelay: ShellDeps["shellRelay"] = {
  async *relay(_inbound: AsyncIterable<unknown>, opts?: { signal?: AbortSignal }) {
    for (let i = 0; i < 200; i++) {
      if (opts?.signal?.aborted) break;
      await new Promise((r) => setTimeout(r, 10));
      yield { frame: { case: "text" as const, value: `frame-${i}` } } as RelayShellResponse;
    }
  },
};

// Fast relay: yields 1 frame then ends.
const fastFakeRelay: ShellDeps["shellRelay"] = {
  async *relay() {
    yield { frame: { case: "text" as const, value: "hello-from-relay" } } as RelayShellResponse;
  },
};

async function startWsServer(deps: ShellDeps) {
  const app = new Hono();
  app.notFound((c) => c.json({ error: "not found" }, 404));
  // The shell is an accept-first upgrade HOOK now — no Hono route, no
  // @hono/node-ws. This wiring mirrors index.ts exactly.
  const server = buildServer(app, () => {}, [makeShellUpgradeHandler(deps)]);
  return new Promise<{ wsUrl: string; baseUrl: string; server: ReturnType<typeof buildServer> }>(
    (resolve) => {
      server.listen(0, "127.0.0.1", () => {
        const addr = server.address() as AddressInfo;
        resolve({
          baseUrl: `http://127.0.0.1:${addr.port}`,
          wsUrl: `ws://127.0.0.1:${addr.port}`,
          server,
        });
      });
    },
  );
}

function stopWsServer(server: ReturnType<typeof buildServer>): Promise<void> {
  (server as ReturnType<typeof buildServer> & { closeAllConnections(): void }).closeAllConnections();
  return new Promise((r) => server.close(() => r()));
}

/** Open a socket and await its close code (the guard speaks in 4000+status). */
function closeCodeOf(url: string, protocols?: string[]): Promise<number> {
  return new Promise<number>((resolve, reject) => {
    const ws = protocols ? new WebSocket(url, protocols) : new WebSocket(url);
    const t = setTimeout(() => reject(new Error("timeout waiting for close")), 5000);
    ws.addEventListener("close", (e) => {
      clearTimeout(t);
      resolve(e.code);
    });
  });
}

describe("Shell WS hook — guard", () => {
  test("6: plain HTTP GET is not a shell surface anymore → Hono 404", async () => {
    const { baseUrl, server } = await startWsServer({
      shellRelay: fastFakeRelay,
      getSession: makeGetSessionWs(null),
      resolveOwner: makeResolveOwnerWs(),
    });
    try {
      // The shell moved off Hono entirely; a non-upgrade GET falls through
      // to the app and 404s. Auth refusals are WS close codes (below).
      const res = await fetch(`${baseUrl}/api/v1/sessions/${SESSION_OF_A_WS}/shell`);
      expect(res.status).toBe(404);
    } finally {
      await stopWsServer(server);
    }
  });

  test("7: wrong owner → close code 4404 (accept-first, refuse-after)", async () => {
    const { wsUrl, server } = await startWsServer({
      shellRelay: fastFakeRelay,
      getSession: makeGetSessionWs("someone-else"),
      resolveOwner: makeResolveOwnerWs(),
    });
    try {
      const code = await closeCodeOf(`${wsUrl}/api/v1/sessions/${SESSION_OF_A_WS}/shell`, ["tty"]);
      expect(code).toBe(4404);
    } finally {
      await stopWsServer(server);
    }
  });

  test("8: unauthenticated WS upgrade → close code 4401", async () => {
    const { wsUrl, server } = await startWsServer({
      shellRelay: fastFakeRelay,
      getSession: makeGetSessionWs(null), // null → no session → 401
      resolveOwner: makeResolveOwnerWs(),
    });
    try {
      const code = await closeCodeOf(`${wsUrl}/api/v1/sessions/${SESSION_OF_A_WS}/shell`, ["tty"]);
      expect(code).toBe(4401);
    } finally {
      await stopWsServer(server);
    }
  });
});

describe("Shell WS hook — accept-first regression", () => {
  test("REGRESSION: accepted upgrade behind REAL async auth (a macrotask)", async () => {
    // THE #1333-class pin. Under Bun, the handshake must complete inside the
    // request's own event-loop turn: an auth guard that awaits real I/O (here
    // a setTimeout macrotask — same scheduling class as a Postgres read)
    // before the accept breaks native `server.upgrade()`, and the client sees
    // a dead socket. The old Hono-middleware shape failed EXACTLY this test;
    // immediate-resolve fakes (microtasks) cannot catch it, which is how the
    // shell shipped broken for 11 days. If this test hangs or closes without
    // "hello-from-relay", the accept-first invariant has regressed.
    const macrotaskGetSession: ShellDeps["getSession"] = async () => {
      await new Promise((r) => setTimeout(r, 25));
      return { user: { id: MEMBER_A_WS, role: "user", email: "a@test" } };
    };
    const { wsUrl, server } = await startWsServer({
      shellRelay: fastFakeRelay,
      getSession: macrotaskGetSession,
      resolveOwner: makeResolveOwnerWs(),
    });
    try {
      const first = await new Promise<string>((resolve, reject) => {
        const ws = new WebSocket(`${wsUrl}/api/v1/sessions/${SESSION_OF_A_WS}/shell`, ["tty"]);
        const t = setTimeout(() => reject(new Error("no frame — accept-first regressed")), 5000);
        ws.addEventListener("message", (e) => {
          clearTimeout(t);
          resolve(String(e.data));
          ws.close();
        });
        ws.addEventListener("close", (e) => {
          clearTimeout(t);
          reject(new Error(`closed before frame: code=${e.code} reason=${e.reason}`));
        });
      });
      expect(first).toBe("hello-from-relay");
    } finally {
      await stopWsServer(server);
    }
  });
});

describe("Shell WS hook — subprotocol + process-survives", () => {
  test("9: subprotocol 'tty' echoed", async () => {
    const { wsUrl, server } = await startWsServer({
      // Use slowFakeRelay so the connection stays open long enough to capture protocol.
      shellRelay: slowFakeRelay,
      getSession: makeGetSessionWs(MEMBER_A_WS),
      resolveOwner: makeResolveOwnerWs(),
    });
    try {
      const ws = new WebSocket(`${wsUrl}/api/v1/sessions/${SESSION_OF_A_WS}/shell`, ["tty"]);
      const proto = await new Promise<string>((resolve, reject) => {
        const t = setTimeout(() => reject(new Error("timeout")), 5000);
        ws.addEventListener("open", () => {
          clearTimeout(t);
          resolve(ws.protocol);
          ws.close();
        });
        ws.addEventListener("close", (e) => {
          clearTimeout(t);
          reject(new Error(`WS closed before open: code=${e.code} reason=${e.reason}`));
        });
      });
      expect(proto).toBe("tty");
    } finally {
      await stopWsServer(server);
    }
  });

  test("10: process survives abrupt client close — no unhandledRejection", async () => {
    const unhandled: Error[] = [];
    const trap = (e: Error) => {
      unhandled.push(e);
    };
    process.on("unhandledRejection", trap);

    let relayFinalized = false;
    const trackingRelay: ShellDeps["shellRelay"] = {
      async *relay(_inbound: AsyncIterable<unknown>, opts?: { signal?: AbortSignal }) {
        try {
          for (let i = 0; i < 100; i++) {
            if (opts?.signal?.aborted) break;
            await new Promise((r) => setTimeout(r, 20));
            yield { frame: { case: "text" as const, value: `f${i}` } } as RelayShellResponse;
          }
        } finally {
          relayFinalized = true;
        }
      },
    };

    const { wsUrl, server } = await startWsServer({
      shellRelay: trackingRelay,
      getSession: makeGetSessionWs(MEMBER_A_WS),
      resolveOwner: makeResolveOwnerWs(),
    });

    try {
      const ws = new WebSocket(`${wsUrl}/api/v1/sessions/${SESSION_OF_A_WS}/shell`, ["tty"]);
      await new Promise<void>((resolve, reject) => {
        const t = setTimeout(() => reject(new Error("open timeout")), 5000);
        ws.addEventListener("open", () => {
          clearTimeout(t);
          ws.close();
          resolve();
        });
        ws.addEventListener("error", (e) => {
          clearTimeout(t);
          reject(new Error(String(e)));
        });
      });
      // Give pump time to notice the close.
      await new Promise((r) => setTimeout(r, 300));
      expect(relayFinalized).toBe(true);
      expect(unhandled.length).toBe(0);
    } finally {
      process.off("unhandledRejection", trap);
      await stopWsServer(server);
    }
  });
});
