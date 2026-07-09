/**
 * In-guest IDE proxy (ADR 0081 P4) — HTTP + WS on the Bun runtime.
 *
 * Mirrors preview-{proxy,ws}.test.ts: only the coordinator PortRelay is faked
 * (a TCP bridge to a real in-process "guest" server standing in for
 * code-server). Asserts the P4-specific behavior — prefix strip preserving
 * path+query, the bare-/ide redirect, guard rejection (HTTP 401 and WS 4401),
 * EnsureIde invocation, and the WS bridge with the stripped path.
 */

import { afterEach, describe, expect, test } from "bun:test";
import net from "node:net";
import http from "node:http";
import { Hono } from "hono";
import { createNodeWebSocket } from "@hono/node-ws";
import { WebSocketServer, WebSocket as WsClient } from "ws";
import { create } from "@bufbuild/protobuf";

import { buildServer } from "../server.ts";
import { makeIdeRoute, makeIdeUpgradeHandler, parseIdePath } from "../routes/ide.ts";
import { pushableQueue } from "../routes/shell.ts";
import {
  RelayPortResponseSchema,
  type RelayPortRequest,
} from "../gen/engram/app/v1/session_pb.ts";
import type { PortRelayClient } from "../routes/preview-proxy.ts";

const cleanups: Array<() => void> = [];
afterEach(() => {
  for (const c of cleanups.splice(0)) c();
});

/** A PortRelay that bridges the tunnel's raw bytes to a real TCP port — i.e.
 * behaves like the host-agent dialing the guest (same shape as the preview
 * tests' fake). */
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
const ownedBy = (owner: string) => async () => owner;

describe("parseIdePath", () => {
  test("strips the route prefix and preserves path + query exactly", () => {
    expect(parseIdePath("/api/v1/sessions/s1/ide/static/out/vs/code.js", "?v=1&tkn=a%2Fb")).toEqual({
      sessionId: "s1",
      guestPath: "/static/out/vs/code.js?v=1&tkn=a%2Fb",
    });
  });

  test("bare /ide maps to /, non-IDE paths return null", () => {
    expect(parseIdePath("/api/v1/sessions/s1/ide", "")).toEqual({ sessionId: "s1", guestPath: "/" });
    expect(parseIdePath("/api/v1/sessions/s1/shell", "")).toBeNull();
    expect(parseIdePath("/api/v1/sessions/s1/ideX", "")).toBeNull();
  });
});

describe("IDE HTTP proxy", () => {
  test("guards, calls EnsureIde, and proxies with the prefix stripped", async () => {
    // Guest: a real HTTP server standing in for code-server.
    const guest = http.createServer((req, res) => {
      res.setHeader("content-type", "text/plain");
      res.end(`guest saw ${req.url}`);
    });
    await new Promise<void>((r) => guest.listen(0, "127.0.0.1", () => r()));
    const guestPort = (guest.address() as net.AddressInfo).port;
    cleanups.push(() => guest.close());

    const ensured: string[] = [];
    const { app: ideApp } = makeIdeRoute({
      sessions: {
        ensureIde: async (req: { sessionId: string }) => {
          ensured.push(req.sessionId);
          return { port: guestPort };
        },
      },
      portRelay: fakeRelayToPort(guestPort),
      getSession: authedOwner,
      resolveOwner: ownedBy("owner"),
    });
    const app = new Hono();
    app.route("/", ideApp);
    const server = buildServer(app);
    await new Promise<void>((r) => server.listen(0, "127.0.0.1", () => r()));
    const port = (server.address() as net.AddressInfo).port;
    cleanups.push(() => server.close());

    const resp = await fetch(`http://127.0.0.1:${port}/api/v1/sessions/s1/ide/healthz?probe=1`);
    expect(resp.status).toBe(200);
    expect(await resp.text()).toBe("guest saw /healthz?probe=1");
    expect(ensured).toEqual(["s1"]);
  });

  test("redirects bare /ide to /ide/ and rejects unauthenticated with 401", async () => {
    const { app: ideApp } = makeIdeRoute({
      sessions: { ensureIde: async () => ({ port: 1 }) },
      portRelay: fakeRelayToPort(0),
      getSession: async () => null, // unauthenticated
      resolveOwner: ownedBy("owner"),
    });
    const app = new Hono();
    app.route("/", ideApp);
    const server = buildServer(app);
    await new Promise<void>((r) => server.listen(0, "127.0.0.1", () => r()));
    const port = (server.address() as net.AddressInfo).port;
    cleanups.push(() => server.close());

    const redir = await fetch(`http://127.0.0.1:${port}/api/v1/sessions/s1/ide?a=1`, {
      redirect: "manual",
    });
    expect(redir.status).toBe(307);
    expect(redir.headers.get("location")).toBe("/api/v1/sessions/s1/ide/?a=1");

    const unauth = await fetch(`http://127.0.0.1:${port}/api/v1/sessions/s1/ide/`);
    expect(unauth.status).toBe(401);
  });
});

describe("IDE WS bridge", () => {
  test("bridges a client WS to the guest with the prefix-stripped path", async () => {
    // Guest: a real ws server standing in for code-server, echoing the
    // handshake path in its first message so we can assert the strip.
    const guest = new WebSocketServer({ port: 0 });
    guest.on("connection", (ws, req) => {
      ws.send(`path=${req.url}`);
      ws.on("message", (m, isBin) => ws.send(m, { binary: isBin }));
    });
    await new Promise<void>((r) => guest.on("listening", () => r()));
    const guestPort = (guest.address() as net.AddressInfo).port;
    cleanups.push(() => guest.close());

    const app = new Hono();
    const nodeWs = createNodeWebSocket({ app });
    const server = buildServer(app, () => {}, nodeWs, [
      makeIdeUpgradeHandler({
        sessions: { ensureIde: async () => ({ port: guestPort }) },
        portRelay: fakeRelayToPort(guestPort),
        getSession: authedOwner,
        resolveOwner: ownedBy("owner"),
      }),
    ]);
    await new Promise<void>((r) => server.listen(0, "127.0.0.1", () => r()));
    const serverPort = (server.address() as net.AddressInfo).port;
    cleanups.push(() => server.close());

    const client = new WsClient(`ws://127.0.0.1:${serverPort}/api/v1/sessions/s1/ide/ws?rc=1`);
    cleanups.push(() => client.close());

    const messages = await new Promise<string[]>((resolve, reject) => {
      const timer = setTimeout(() => reject(new Error("no echo within 5s")), 5000);
      const seen: string[] = [];
      client.on("open", () => client.send("hello-ide"));
      client.on("message", (data) => {
        seen.push(data.toString());
        if (seen.length === 2) {
          clearTimeout(timer);
          resolve(seen);
        }
      });
      client.on("error", (e) => {
        clearTimeout(timer);
        reject(e);
      });
    });
    expect(messages).toEqual(["path=/ws?rc=1", "hello-ide"]);
  });

  test("rejects an unauthenticated IDE WS with close code 4401", async () => {
    const app = new Hono();
    const nodeWs = createNodeWebSocket({ app });
    const server = buildServer(app, () => {}, nodeWs, [
      makeIdeUpgradeHandler({
        sessions: { ensureIde: async () => ({ port: 1 }) },
        portRelay: fakeRelayToPort(0), // never dialed — auth fails first
        getSession: async () => null,
        resolveOwner: ownedBy("owner"),
      }),
    ]);
    await new Promise<void>((r) => server.listen(0, "127.0.0.1", () => r()));
    const serverPort = (server.address() as net.AddressInfo).port;
    cleanups.push(() => server.close());

    const client = new WsClient(`ws://127.0.0.1:${serverPort}/api/v1/sessions/s1/ide/ws`);
    cleanups.push(() => client.close());

    const code = await new Promise<number>((resolve, reject) => {
      const timer = setTimeout(() => reject(new Error("no close within 5s")), 5000);
      client.on("close", (c) => {
        clearTimeout(timer);
        resolve(c);
      });
      client.on("error", () => {
        /* close follows */
      });
    });
    expect(code).toBe(4401);
  });
});
