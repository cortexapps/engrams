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
// Server-level tests (guard + subprotocol + process-survives)
// ---------------------------------------------------------------------------

import { Hono } from "hono";
import { createNodeWebSocket } from "@hono/node-ws";
import { buildServer } from "../server.ts";
import { makeShellRoute } from "../routes/shell.ts";
import type { RelayShellResponse } from "../gen/engram/app/v1/session_pb.ts";
import type { AddressInfo } from "node:net";

const MEMBER_A_WS = "member-a-ws-test";
const MEMBER_B_WS = "member-b-ws-test";
const SESSION_OF_A_WS = "session-owned-by-a-ws";

type ShellDeps = NonNullable<Parameters<typeof makeShellRoute>[0]>;

function makeGetSessionWs(userId: string | null): ShellDeps["getSession"] {
  return async () =>
    userId ? { user: { id: userId, role: "user", email: `${userId}@test` } } : null;
}

function makeResolveOwnerWs(): ShellDeps["resolveOwner"] {
  return async (sid) => (sid === SESSION_OF_A_WS ? MEMBER_A_WS : null);
}

// Fake relay: keeps yielding text frames until aborted.
const slowFakeRelay: ShellDeps["shellRelay"] = {
  async *relay(_inbound, opts) {
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
  const { upgradeWebSocket, injectWebSocket, wss } = createNodeWebSocket({ app });
  // Echo 'tty' subprotocol.
  (wss as { options: { handleProtocols?: (p: Set<string>) => string | false } }).options.handleProtocols =
    (p: Set<string>) => (p.has("tty") ? "tty" : false);
  const { app: shellApp, injectUpgrade } = makeShellRoute(deps);
  injectUpgrade(upgradeWebSocket);
  app.route("/", shellApp);
  app.notFound((c) => c.json({ error: "not found" }, 404));
  app.onError((err, c) => {
    if ("status" in err && typeof (err as { status?: number }).status === "number") {
      const e = err as { status: number; message: string };
      return c.json({ error: e.message }, e.status as 401 | 404 | 500);
    }
    return c.json({ error: String(err) }, 500);
  });
  // Pass the full NodeWebSocket handle so buildServer can install the
  // Bun-compatible upgrade handler (wss.handleUpgrade instead of socket.end).
  const server = buildServer(app, () => {}, { upgradeWebSocket, injectWebSocket, wss });
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
  return new Promise((r, j) => server.close((e) => (e ? j(e) : r())));
}

describe("Shell WS route — guard", () => {
  test("6: unauthenticated → 401 before upgrade", async () => {
    const { baseUrl, server } = await startWsServer({
      shellRelay: fastFakeRelay,
      getSession: makeGetSessionWs(null),
      resolveOwner: makeResolveOwnerWs(),
    });
    try {
      // Plain HTTP GET (no Upgrade header) — guard fires in the Hono handler
      // and returns a proper 401 HTTP response before any WS upgrade.
      const res = await fetch(`${baseUrl}/api/v1/sessions/${SESSION_OF_A_WS}/shell`);
      expect(res.status).toBe(401);
    } finally {
      await stopWsServer(server);
    }
  });

  test("7: wrong owner → 404 before upgrade", async () => {
    const { baseUrl, server } = await startWsServer({
      shellRelay: fastFakeRelay,
      getSession: makeGetSessionWs(MEMBER_B_WS),
      resolveOwner: makeResolveOwnerWs(),
    });
    try {
      // Plain HTTP GET — guard fires before WS upgrade; returns 404.
      const res = await fetch(`${baseUrl}/api/v1/sessions/${SESSION_OF_A_WS}/shell`);
      expect(res.status).toBe(404);
    } finally {
      await stopWsServer(server);
    }
  });

  test("8: unauthenticated WS upgrade → close code 4401", async () => {
    // Use the `ws` npm client (not Bun's native WebSocket) because Bun's native
    // WS strips custom request headers, breaking any auth that relies on them.
    // The ws client sends a real HTTP Upgrade request with full headers.
    const { wsUrl, server } = await startWsServer({
      shellRelay: fastFakeRelay,
      getSession: makeGetSessionWs(null), // null → no session → 401
      resolveOwner: makeResolveOwnerWs(),
    });
    try {
      const closeCode = await new Promise<number>((resolve, reject) => {
        const ws = new WebSocketClient(
          `${wsUrl}/api/v1/sessions/${SESSION_OF_A_WS}/shell`,
          ["tty"],
        );
        const t = setTimeout(() => {
          ws.terminate();
          reject(new Error("timeout waiting for close"));
        }, 4000);
        ws.on("close", (code: number) => {
          clearTimeout(t);
          // setImmediate defers the resolve so Bun's event loop can flush the
          // ws-client microtasks before the Promise continuation runs.
          setImmediate(() => resolve(code));
        });
        ws.on("error", (_err: Error) => {
          // ws may emit an error before close on non-101 responses; ignore and
          // wait for the close event which carries the code.
        });
      });
      expect(closeCode).toBe(4401);
    } finally {
      // closeAllConnections() forces immediate teardown of any lingering sockets
      // (e.g. the handleUpgrade connection); without it server.close() blocks
      // waiting for the WS connection to drain.  After closing connections we
      // call server.close() directly (stopWsServer would throw ERR_SERVER_NOT_RUNNING
      // if closeAllConnections already stopped it in Bun 1.3).
      (server as ReturnType<typeof buildServer> & { closeAllConnections(): void }).closeAllConnections();
      await new Promise<void>((r) => server.close(() => r()));
    }
  });
});

describe("Shell WS route — subprotocol + process-survives", () => {
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
        ws.addEventListener("error", (e) => { clearTimeout(t); reject(new Error(String(e))); });
        ws.addEventListener("close", (e) => {
          // If close fires without open, reject with the close code.
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
    const trap = (e: Error) => { unhandled.push(e); };
    process.on("unhandledRejection", trap);

    let relayFinalized = false;
    const trackingRelay: ShellDeps["shellRelay"] = {
      async *relay(_inbound, opts) {
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
        ws.addEventListener("open", () => { clearTimeout(t); ws.close(); resolve(); });
        ws.addEventListener("error", (e) => { clearTimeout(t); reject(new Error(String(e))); });
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
