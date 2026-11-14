import { expect, test } from "bun:test";
import { Code, ConnectError, createClient, createRouterTransport } from "@connectrpc/connect";
import { createConnectTransport } from "@connectrpc/connect-node";
import type { AddressInfo } from "node:net";
import { Hono } from "hono";
import { buildServer } from "../server.ts";
import { SessionService } from "../gen/engram/app/v1/session_pb.ts";
import { registerPassthrough } from "../rpc/passthrough.ts";
import { makeMcpLifecycleInterceptor } from "../mcp/lifecycle-interceptor.ts";
import { makeMcpLifecycleInvalidator } from "../mcp/lifecycle-invalidation.ts";
import { effectFixture } from "./mcp-effect-fixture.ts";

test("public interrupt authorizes before durable cancellation and upstream forwarding", async () => {
  const f = await effectFixture({ classification: "read" });
  const call = await f.driver.acceptToolRequest(f.request);
  let owner = "other-owner";
  let forwarded = 0;
  const invalidate = makeMcpLifecycleInvalidator({
    db: f.db,
    abort: async (id) => f.driver.cancel(id),
  });
  const upstream = createRouterTransport(
    (router) =>
      router.service(SessionService, {
        async interrupt(req) {
          expect((await f.journal.get(call.id))!.state).toBe("canceled");
          forwarded++;
          return { sessionId: req.sessionId };
        },
      }),
    { transport: { interceptors: [makeMcpLifecycleInterceptor(invalidate)] } },
  );
  const server = buildServer(new Hono(), (router) =>
    registerPassthrough(
      router,
      [{ service: SessionService, methods: ["Interrupt"] }],
      upstream,
      async () => ({ user: { id: f.actorId, role: "user", email: "user@example.test" } }),
      async () => owner,
    ),
  );
  try {
    const url = await new Promise<string>((resolve) =>
      server.listen(0, "127.0.0.1", () =>
        resolve(`http://127.0.0.1:${(server.address() as AddressInfo).port}/rpc`),
      ),
    );
    const client = createClient(
      SessionService,
      createConnectTransport({ baseUrl: url, httpVersion: "1.1" }),
    );
    try {
      await client.interrupt({ sessionId: f.sessionId });
      throw Error("expected denial");
    } catch (error) {
      expect(error).toBeInstanceOf(ConnectError);
      expect((error as ConnectError).code).toBe(Code.NotFound);
    }
    expect((await f.journal.get(call.id))!.state).toBe("authorized");
    expect(forwarded).toBe(0);
    owner = f.actorId;
    await client.interrupt({ sessionId: f.sessionId });
    expect(forwarded).toBe(1);
    expect((await f.journal.cancellationReceipt(call.id))?.cause).toBe("interrupt");
  } finally {
    await new Promise<void>((resolve, reject) =>
      server.close((error) => (error ? reject(error) : resolve())),
    );
    await f.cleanup();
  }
}, 20000);
