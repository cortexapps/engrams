/**
 * Unit tests for the VNC WS route (ADR 0064).
 *
 * Mirrors the shell route's guard harness (src/__tests__/shell.test.ts) and
 * asserts the open frame carries target = VNC. No SMOKE env needed.
 */
import { expect, test, describe } from "bun:test";
import WebSocketClient from "ws";
import { Hono } from "hono";
import { createNodeWebSocket } from "@hono/node-ws";
import { buildServer } from "../server.ts";
import { makeVncRoute, vncOpenFrame } from "./vnc.ts";
import {
  ShellTarget,
  type RelayShellResponse,
} from "../gen/engram/app/v1/session_pb.ts";
import type { AddressInfo } from "node:net";

const MEMBER_A = "member-a-vnc-test";
const MEMBER_B = "member-b-vnc-test";
const SESSION_OF_A = "session-owned-by-a-vnc";

type VncDeps = NonNullable<Parameters<typeof makeVncRoute>[0]>;

function makeGetSession(userId: string | null): VncDeps["getSession"] {
  return async () =>
    userId ? { user: { id: userId, role: "user", email: `${userId}@test` } } : null;
}

function makeResolveOwner(): VncDeps["resolveOwner"] {
  return async (sid) => (sid === SESSION_OF_A ? MEMBER_A : null);
}

// Fast relay: yields one binary frame then ends.
const fastFakeRelay: VncDeps["shellRelay"] = {
  async *relay() {
    yield {
      frame: { case: "binary" as const, value: new Uint8Array([0x52, 0x46, 0x42]) },
    } as RelayShellResponse;
  },
};

// Slow relay: keeps yielding until aborted (keeps the socket open for protocol checks).
const slowFakeRelay: VncDeps["shellRelay"] = {
  async *relay(_inbound, opts) {
    for (let i = 0; i < 200; i++) {
      if (opts?.signal?.aborted) break;
      await new Promise((r) => setTimeout(r, 10));
      yield {
        frame: { case: "binary" as const, value: new Uint8Array([i & 0xff]) },
      } as RelayShellResponse;
    }
  },
};

async function startWsServer(deps: VncDeps) {
  const app = new Hono();
  const { upgradeWebSocket, injectWebSocket, wss } = createNodeWebSocket({ app });
  // Widened selection matching index.ts: tty → tty, else binary → binary, else none.
  (wss as { options: { handleProtocols?: (p: Set<string>) => string | false } }).options.handleProtocols =
    (p: Set<string>) => (p.has("tty") ? "tty" : p.has("binary") ? "binary" : false);
  const { app: vncApp, injectUpgrade } = makeVncRoute(deps);
  injectUpgrade(upgradeWebSocket);
  app.route("/", vncApp);
  app.notFound((c) => c.json({ error: "not found" }, 404));
  app.onError((err, c) => {
    if ("status" in err && typeof (err as { status?: number }).status === "number") {
      const e = err as { status: number; message: string };
      return c.json({ error: e.message }, e.status as 401 | 404 | 500);
    }
    return c.json({ error: String(err) }, 500);
  });
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

describe("vncOpenFrame", () => {
  test("open frame carries the VNC target", () => {
    const f = vncOpenFrame("sess-123");
    expect(f.frame.case).toBe("open");
    if (f.frame.case === "open") {
      expect(f.frame.value.sessionId).toBe("sess-123");
      expect(f.frame.value.target).toBe(ShellTarget.VNC);
    }
  });
});

describe("VNC WS route — guard", () => {
  test("unauthenticated → 401 before upgrade", async () => {
    const { baseUrl, server } = await startWsServer({
      shellRelay: fastFakeRelay,
      getSession: makeGetSession(null),
      resolveOwner: makeResolveOwner(),
    });
    try {
      const res = await fetch(`${baseUrl}/api/v1/sessions/${SESSION_OF_A}/vnc`);
      expect(res.status).toBe(401);
    } finally {
      await stopWsServer(server);
    }
  });

  test("wrong owner → 404 before upgrade", async () => {
    const { baseUrl, server } = await startWsServer({
      shellRelay: fastFakeRelay,
      getSession: makeGetSession(MEMBER_B),
      resolveOwner: makeResolveOwner(),
    });
    try {
      const res = await fetch(`${baseUrl}/api/v1/sessions/${SESSION_OF_A}/vnc`);
      expect(res.status).toBe(404);
    } finally {
      await stopWsServer(server);
    }
  });

  test("unauthenticated WS upgrade → close code 4401", async () => {
    // Use the `ws` npm client (not Bun's native WebSocket) — Bun's native WS
    // strips custom request headers. noVNC connects with NO subprotocol, so we
    // open without one to exercise the widened handleProtocols path.
    const { wsUrl, server } = await startWsServer({
      shellRelay: fastFakeRelay,
      getSession: makeGetSession(null), // null → no session → 401
      resolveOwner: makeResolveOwner(),
    });
    try {
      const closeCode = await new Promise<number>((resolve, reject) => {
        const ws = new WebSocketClient(`${wsUrl}/api/v1/sessions/${SESSION_OF_A}/vnc`);
        const t = setTimeout(() => {
          ws.terminate();
          reject(new Error("timeout waiting for close"));
        }, 4000);
        ws.on("close", (code: number) => {
          clearTimeout(t);
          setImmediate(() => resolve(code));
        });
        ws.on("error", () => {
          // ws may emit an error before close on non-101 responses; ignore and
          // wait for the close event which carries the code.
        });
      });
      expect(closeCode).toBe(4401);
    } finally {
      (server as ReturnType<typeof buildServer> & { closeAllConnections(): void }).closeAllConnections();
      await new Promise<void>((r) => server.close(() => r()));
    }
  });
});

describe("VNC WS route — no subprotocol required", () => {
  test("noVNC client (no subprotocol) connects and opens", async () => {
    const { wsUrl, server } = await startWsServer({
      // slowFakeRelay keeps the connection open long enough to observe open.
      shellRelay: slowFakeRelay,
      getSession: makeGetSession(MEMBER_A),
      resolveOwner: makeResolveOwner(),
    });
    try {
      // No subprotocol offered — modern noVNC. handleProtocols returns false
      // (no subprotocol selected); the upgrade still proceeds per RFC 6455.
      const ws = new WebSocket(`${wsUrl}/api/v1/sessions/${SESSION_OF_A}/vnc`);
      const proto = await new Promise<string>((resolve, reject) => {
        const t = setTimeout(() => reject(new Error("timeout")), 5000);
        ws.addEventListener("open", () => {
          clearTimeout(t);
          resolve(ws.protocol);
          ws.close();
        });
        ws.addEventListener("error", (e) => { clearTimeout(t); reject(new Error(String(e))); });
        ws.addEventListener("close", (e) => {
          clearTimeout(t);
          reject(new Error(`WS closed before open: code=${e.code} reason=${e.reason}`));
        });
      });
      // No subprotocol negotiated for a noVNC client that offered none.
      expect(proto).toBe("");
    } finally {
      await stopWsServer(server);
    }
  });
});
