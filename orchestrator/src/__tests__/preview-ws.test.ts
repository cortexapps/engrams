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
import { createNodeWebSocket } from "@hono/node-ws";
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
    const nodeWs = createNodeWebSocket({ app });
    const server = buildServer(
      app,
      () => {},
      nodeWs,
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
    const nodeWs = createNodeWebSocket({ app });
    const server = buildServer(
      app,
      () => {},
      nodeWs,
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
