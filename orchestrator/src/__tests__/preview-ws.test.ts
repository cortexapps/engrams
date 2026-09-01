/**
 * Preview WebSocket passthrough (ADR 0064 P2b-ws) — full bridge on the Bun
 * runtime: a real ws client → buildServer's upgrade hook → makePreviewUpgradeHandler
 * → loopback → a fake PortRelay that pipes raw bytes to a real ws "guest" echo
 * server → frames back. Exercises the whole Bun-safe WS path in-process; only
 * the real coordinator PortRelay is faked (replaced by a TCP bridge to the guest).
 */

import { afterEach, describe, expect, test } from "bun:test";
import net from "node:net";
import { Hono } from "hono";
import { WebSocketServer, WebSocket as WsClient } from "ws";
import { create } from "@bufbuild/protobuf";

import { buildServer } from "../server.ts";
import { makePreviewUpgradeHandler } from "../routes/preview-ws.ts";
import { pushableQueue } from "../routes/shell.ts";
import {
  RelayPortResponseSchema,
  type RelayPortRequest,
} from "../gen/engram/app/v1/session_pb.ts";
import type { SessionAppRow, SessionAppStore } from "../db/session-apps.ts";
import type { PortRelayClient } from "../routes/preview-proxy.ts";

const cleanups: Array<() => void> = [];
afterEach(() => {
  for (const c of cleanups.splice(0)) c();
});

function fakeStore(row: SessionAppRow): SessionAppStore {
  return {
    async createMany() {
      throw new Error("unused");
    },
    async createOne() {
      throw new Error("unused");
    },
    async listBySession() {
      return [];
    },
    async getByHostLabel(label) {
      return label === row.hostLabel ? row : null;
    },
    async deleteByHostLabel() {
      return false;
    },
    async deleteBySession() {
      return 0;
    },
  };
}

/** A PortRelay that bridges the tunnel's raw bytes to a real TCP port — i.e.
 * behaves like the host-agent dialing the guest. Points at the guest ws echo
 * server, so the native WebSocket client's handshake + frames reach a real ws
 * server and echo back. */
function fakeRelayToPort(targetPort: number): PortRelayClient {
  return {
    relay(inbound: AsyncIterable<RelayPortRequest>) {
      return (async function* () {
        const sock = net.connect(targetPort, "127.0.0.1");
        const out = pushableQueue<ReturnType<typeof dataFrame>>(4096);
        function dataFrame(b: Buffer) {
          return create(RelayPortResponseSchema, {
            frame: { case: "data", value: new Uint8Array(b) },
          });
        }
        sock.on("data", (b: Buffer) => out.push(dataFrame(b)));
        sock.on("close", () => out.end());
        sock.on("error", () => out.end());
        void (async () => {
          for await (const m of inbound) {
            if (m.frame.case === "data") sock.write(Buffer.from(m.frame.value));
            else if (m.frame.case === "close") sock.end();
          }
          sock.end();
        })();
        yield* out;
      })();
    },
  };
}

const authedOwner = async () => ({ user: { id: "owner" } });

const sessionApp: SessionAppRow = {
  hostLabel: "web-jumping-fat-kittens",
  sessionId: "sess_1",
  name: "web",
  port: 3000, // ignored by the fake relay (which targets the guest echo port)
  ownerUserId: "owner",
  visibility: "org",
  createdAt: new Date(0),
};

describe("preview WS passthrough", () => {
  test("bridges a client WS through the tunnel to the guest and echoes both ways", async () => {
    // 1. Guest: a real ws echo server (the "dev server" inside the session).
    const guest = new WebSocketServer({ port: 0 });
    guest.on("connection", (ws) => ws.on("message", (m, isBin) => ws.send(m, { binary: isBin })));
    await new Promise<void>((r) => guest.on("listening", () => r()));
    const guestPort = (guest.address() as net.AddressInfo).port;
    cleanups.push(() => guest.close());

    // 2. Orchestrator: buildServer with the preview upgrade hook + a fake relay
    //    that pipes to the guest.
    const app = new Hono();
    const server = buildServer(
      app,
      () => {},
      [
        makePreviewUpgradeHandler({
          store: fakeStore(sessionApp),
          portRelay: fakeRelayToPort(guestPort),
          getSession: authedOwner,
          previewBaseDomain: "lvh.me:8787",
        }),
      ],
    );
    await new Promise<void>((r) => server.listen(0, "127.0.0.1", () => r()));
    const serverPort = (server.address() as net.AddressInfo).port;
    cleanups.push(() => server.close());

    // 3. Client: a real ws client with the preview Host header.
    const client = new WsClient(`ws://127.0.0.1:${serverPort}/socket`, {
      headers: { host: "web-jumping-fat-kittens.lvh.me:8787" },
    });
    cleanups.push(() => client.close());

    const echoed = await new Promise<string>((resolve, reject) => {
      const timer = setTimeout(() => reject(new Error("no echo within 5s")), 5000);
      client.on("open", () => client.send("hello-preview"));
      client.on("message", (data) => {
        clearTimeout(timer);
        resolve(data.toString());
      });
      client.on("error", (e) => {
        clearTimeout(timer);
        reject(e);
      });
    });
    expect(echoed).toBe("hello-preview");
  });

  test("rejects an unauthenticated preview WS (close code 4401)", async () => {
    const app = new Hono();
    const server = buildServer(
      app,
      () => {},
      [
        makePreviewUpgradeHandler({
          store: fakeStore(sessionApp),
          portRelay: fakeRelayToPort(0), // never dialed — auth fails first
          getSession: async () => null,
          previewBaseDomain: "lvh.me:8787",
        }),
      ],
    );
    await new Promise<void>((r) => server.listen(0, "127.0.0.1", () => r()));
    const serverPort = (server.address() as net.AddressInfo).port;
    cleanups.push(() => server.close());

    const client = new WsClient(`ws://127.0.0.1:${serverPort}/`, {
      headers: { host: "web-jumping-fat-kittens.lvh.me:8787" },
    });
    cleanups.push(() => client.close());

    const code = await new Promise<number>((resolve, reject) => {
      const timer = setTimeout(() => reject(new Error("no close within 5s")), 5000);
      client.on("close", (c) => {
        clearTimeout(timer);
        resolve(c);
      });
      client.on("error", () => {
        /* a 4xx upgrade can surface as error first; wait for close */
      });
    });
    expect(code).toBe(4401); // 4000 + 401 (unauthenticated)
  });
});

// ---------------------------------------------------------------------------
// Prod incident, 2026-08-20: both orchestrator pods crash-looped 11 times.
//
// A preview WebSocket upgrade reached `ws`'s abort path — abortHandshake with a
// code `http.STATUS_CODES` has no entry for — and the TypeError escaped the
// ASYNC `upgrade` listener as an unhandled rejection, which under Bun exits the
// process. One client's failed handshake took the whole orchestrator down.
//
// Two independent defences. server.ts now contains any upgrade error to the one
// socket; this asserts the other half — that a socket which has already gone
// away never reaches ws.handleUpgrade at all. A browser abandoning an HMR
// reconnect makes that routine rather than rare.
// ---------------------------------------------------------------------------

describe("preview WS upgrade — a socket that already hung up", () => {
  /** A client that vanished while we awaited the store lookup. */
  const goneSocket = () =>
    ({ destroyed: true, writable: false, destroy() {}, on() {}, end() {} }) as unknown as net.Socket;

  const anyRow: SessionAppRow = {
    hostLabel: "web-gone",
    sessionId: "sess_gone",
    name: "web",
    port: 3000,
    ownerUserId: "owner",
    visibility: "org",
    createdAt: new Date(0),
  };

  test("still claims the upgrade, but does not throw handing it to ws", async () => {
    const handler = makePreviewUpgradeHandler({
      previewBaseDomain: "preview.example.com",
      store: fakeStore(anyRow),
      getSession: async () => null,
    });

    // Host names no app → the reject(404) path, which is the one an abandoned
    // HMR reconnect hits over and over.
    const claimed = await handler(
      { headers: { host: "web-nosuchapp.preview.example.com" }, url: "/" } as never,
      goneSocket(),
      Buffer.alloc(0),
    );

    // Claimed: ADR 0118's termination invariant still holds for a preview host.
    expect(claimed).toBe(true);
  });

  test("an authorized upgrade on a gone socket is also a no-op, not a throw", async () => {
    const handler = makePreviewUpgradeHandler({
      previewBaseDomain: "preview.example.com",
      store: fakeStore(anyRow),
      // Authorized, so this reaches the SUCCESS path's handleUpgrade guard.
      getSession: async () => ({ user: { id: "owner", role: "user" } }),
    });

    const claimed = await handler(
      { headers: { host: "web-gone.preview.example.com" }, url: "/" } as never,
      goneSocket(),
      Buffer.alloc(0),
    );
    expect(claimed).toBe(true);
  });
});

// ---------------------------------------------------------------------------
// The upgrade that took prod down on 2026-08-20.
//
// A handshake with no `Sec-WebSocket-Key` drives Bun's BUILTIN `ws` into its
// abort path (the crash frames read `ws:671` with no file — the npm package in
// node_modules is not what runs), and that path mishandles its own arguments:
// on Bun 1.3.14 it answers `HTTP/1.1 400 [object Object]`, on the 1.4.0 the pods
// run it throws `TypeError: undefined is not an object (evaluating 'message')`.
// The throw escaped the async upgrade listener and exited the process.
//
// server.ts now refuses such a handshake before `ws` sees it. Dropping the
// socket is the only available answer: `socket.end()` in an upgrade listener is
// a no-op under Bun, and completing the handshake to send a close frame needs
// the key that is missing.
// ---------------------------------------------------------------------------

describe("malformed websocket upgrade", () => {
  /** Raw TCP so we can send a handshake a real WS client would never produce. */
  async function rawUpgrade(port: number, lines: string[]): Promise<string> {
    return await new Promise<string>((resolve) => {
      const c = net.connect(port, "127.0.0.1", () => {
        c.write(lines.join("\r\n") + "\r\n\r\n");
      });
      let got = "";
      c.on("data", (d) => (got += d.toString()));
      const done = () => {
        c.destroy();
        resolve(got);
      };
      c.on("close", done);
      c.on("error", done);
      setTimeout(done, 600);
    });
  }

  async function serverWithWs(): Promise<number> {
    const app = new Hono();
    app.get("/healthz", (c) => c.json({ ok: true }));
    const server = buildServer(app, () => {}, []);
    await new Promise<void>((r) => server.listen(0, "127.0.0.1", () => r()));
    cleanups.push(() => server.close());
    return (server.address() as net.AddressInfo).port;
  }

  const BASE = ["GET / HTTP/1.1", "Host: x", "Upgrade: websocket", "Connection: Upgrade"];

  test("no Sec-WebSocket-Key: socket dropped, process survives", async () => {
    const port = await serverWithWs();
    const reply = await rawUpgrade(port, [...BASE, "Sec-WebSocket-Version: 13"]);

    // Dropped, not answered — and crucially NOT `400 [object Object]`, which is
    // what reaching ws's abort path looks like.
    expect(reply).toBe("");
    expect(reply).not.toContain("[object Object]");

    // The server is still serving, which is the whole point: one malformed
    // handshake used to exit the process.
    const res = await fetch(`http://127.0.0.1:${port}/healthz`);
    expect(res.status).toBe(200);
  });

  test("wrong Sec-WebSocket-Version is refused too", async () => {
    const port = await serverWithWs();
    const key = "dGhlIHNhbXBsZSBub25jZQ==";
    // Bun's ws shim answers 101 to version 8, which is its own bug; we refuse
    // before it can.
    const reply = await rawUpgrade(port, [
      ...BASE,
      `Sec-WebSocket-Key: ${key}`,
      "Sec-WebSocket-Version: 8",
    ]);
    expect(reply).not.toContain("101 Switching Protocols");

    const res = await fetch(`http://127.0.0.1:${port}/healthz`);
    expect(res.status).toBe(200);
  });
});

