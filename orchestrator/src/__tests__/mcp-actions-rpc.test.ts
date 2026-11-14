import { expect, test } from "bun:test";
import { Code, ConnectError, createClient } from "@connectrpc/connect";
import { createConnectTransport } from "@connectrpc/connect-node";
import type { AddressInfo } from "node:net";
import { Hono } from "hono";
import { buildServer } from "../server.ts";
import { IntegrationService } from "../gen/engram/app/v1/integration_pb.ts";
import { mcpHandlers } from "../mcp/contracts.ts";
import { reviewContext } from "../mcp/journal-review.ts";
import { effectFixture } from "./mcp-effect-fixture.ts";

test("typed cancellation and unknown resolution require the live owner and exact context", async () => {
  const f = await effectFixture({ classification: "write", response: "hold" });
  let actorId = "other";
  const server = buildServer(new Hono(), (router) =>
    router.service(
      IntegrationService,
      mcpHandlers({
        services: {
          enabled: true,
          journal: f.journal,
          repository: f.repository,
          runtime: {
            cancel: async (id) => f.driver.cancel(id),
            prepareRebind: async () => {
              throw Error("unavailable");
            },
            rebind: async () => {
              throw Error("unavailable");
            },
          },
        },
        connections: f.connections,
        getSession: async () => ({
          user: { id: actorId, role: "user", email: "user@example.test" },
        }),
        now: () => new Date(),
        connectionToProto: (row) => ({ id: row.id }),
      }),
    ),
  );
  try {
    const call = await f.driver.acceptToolRequest(f.request);
    await f.driver.driveOnce(call.id);
    const approval = await f.journal.requestApproval(call.id, f.deploymentId, new Date());
    await f.journal.decide(call.id, f.actorId, true, reviewContext(call, approval), new Date());
    const pending = f.driver.driveOnce(call.id);
    const deadline = Date.now() + 5000;
    while (!(await f.command("stats")).calls && Date.now() < deadline)
      await new Promise((resolve) => setTimeout(resolve, 10));
    const dispatched = (await f.journal.get(call.id))!;
    expect(dispatched.state).toBe("dispatching");
    const url = await new Promise<string>((resolve) =>
      server.listen(0, "127.0.0.1", () =>
        resolve(`http://127.0.0.1:${(server.address() as AddressInfo).port}/rpc`),
      ),
    );
    const client = createClient(
      IntegrationService,
      createConnectTransport({ baseUrl: url, httpVersion: "1.1" }),
    );
    try {
      await client.cancelMcpCall({
        executionId: call.id,
        expectedJournalRevision: dispatched.version,
      });
      throw Error("expected denial");
    } catch (error) {
      expect(error).toBeInstanceOf(ConnectError);
      expect((error as ConnectError).code).toBe(Code.PermissionDenied);
    }
    expect((await f.journal.get(call.id))!.state).toBe("dispatching");
    actorId = f.actorId;
    const canceled = await client.cancelMcpCall({
      executionId: call.id,
      expectedJournalRevision: dispatched.version,
    });
    expect(canceled.receipt?.state).toBe("outcome_unknown");
    expect((await pending).state).toBe("outcome_unknown");
    const unknown = (await f.journal.get(call.id))!;
    const expected = {
      executionId: call.id,
      journalRevision: unknown.version,
      requestDigest: unknown.requestDigest,
      bindingHash: unknown.bindingHash,
      intentId: unknown.intentId!,
    };
    await expect(
      client.resolveMcpOutcome({
        executionId: call.id,
        outcome: "acknowledged",
        expectedUnknownContext: { ...expected, intentId: "wrong" },
      }),
    ).rejects.toThrow();
    const receipt = await client.resolveMcpOutcome({
      executionId: call.id,
      outcome: "acknowledged",
      expectedUnknownContext: expected,
    });
    expect(receipt.receipt?.expectedContext?.intentId).toBe(expected.intentId);
    expect((await f.command("stats")).calls).toBe(1);
  } finally {
    await new Promise<void>((resolve) =>
      server.listening ? server.close(() => resolve()) : resolve(),
    );
    await f.cleanup();
  }
}, 20000);
