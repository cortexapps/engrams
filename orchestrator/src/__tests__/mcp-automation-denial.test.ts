import { expect, spyOn, test } from "bun:test";
import { eq } from "drizzle-orm";
import {
  automation,
  automationRun,
  automationSession,
  mcpAdmission,
  mcpApproval,
  mcpExecution,
  task,
  taskSession,
} from "../db/schema.ts";
import { makeMcpRecovery } from "../mcp/recovery.ts";
import { verifyMcpTestDatabases } from "./helpers/mcp-test-isolation.ts";
import { effectFixture } from "./mcp-effect-fixture.ts";

await verifyMcpTestDatabases();

async function linkAutomation(f: Awaited<ReturnType<typeof effectFixture>>) {
  const automationId = crypto.randomUUID(),
    runId = crypto.randomUUID();
  await f.db
    .insert(automation)
    .values({ id: automationId, name: "Fixture automation", createdByUserId: f.actorId });
  await f.db.insert(automationRun).values({
    id: runId,
    automationId,
    taskId: f.taskId,
    sessionId: f.sessionId,
    status: "running",
    trigger: { source: "manual", receivedAt: new Date().toISOString() },
  });
  await f.db.insert(automationSession).values({ sessionId: f.sessionId, runId, blockId: "start" });
  await f.db
    .update(taskSession)
    .set({ integrationPrincipalId: `automation:${automationId}` })
    .where(eq(taskSession.sessionId, f.sessionId));
  await f.db.update(task).set({ createdByUserId: null }).where(eq(task.id, f.taskId));
  return automationId;
}

function recovery(
  f: Awaited<ReturnType<typeof effectFixture>>,
  completeToolCall: (input: { resultJson: string }) => Promise<void> = async () => {},
) {
  return makeMcpRecovery({
    db: f.db,
    driver: f.driver,
    deploymentId: f.deploymentId,
    now: () => new Date(),
    completer: { completeToolCall },
    async readRequestEvent(_sessionId, idx) {
      return {
        idx,
        kind: "tool_call_requested",
        payloadJson: JSON.stringify({
          tool_call_id: f.request.toolCallId,
          run_id: f.request.runId,
          name: f.request.modelToolName,
          args_json: f.request.originalArguments,
        }),
      };
    },
  });
}

function requestMarker(f: Awaited<ReturnType<typeof effectFixture>>) {
  return {
    sessionId: f.sessionId,
    runId: f.request.runId,
    toolCallId: f.request.toolCallId,
    requestEventIdx: "1",
    deadlineMs: Date.now() + 930000,
  };
}

test("write-eligible automation without an explicit write grant completes a policy denial without effects", async () => {
  const f = await effectFixture({ classification: "write", automationWrite: false });
  let automationId: string | undefined;
  try {
    automationId = await linkAutomation(f);
    const delivered: string[] = [];
    let deliveryUnavailable = true;
    const runtime = recovery(f, async ({ resultJson }) => {
      if (deliveryUnavailable) throw Error("private-transport-error");
      delivered.push(resultJson);
    });
    const ref = requestMarker(f);
    expect(f.snapshot.immutableBindings[0]!.tools[0]!.grant.automationWrite).toBe(false);
    f.failAuthorityReads(1);
    await expect(runtime.decideRequest(ref)).rejects.toThrow("MCP request acceptance unavailable");
    const decision = await runtime.decideRequest(ref);
    expect(decision).toEqual({ kind: "denied", code: "write_not_allowed" });
    if (decision.kind !== "denied") throw Error("expected policy denial");
    await expect(runtime.deliverDenial(ref, decision.code)).rejects.toThrow(
      "MCP denial completion unavailable",
    );
    deliveryUnavailable = false;
    await runtime.deliverDenial(ref, decision.code);
    expect(delivered).toHaveLength(1);
    expect(JSON.parse(delivered[0]!)).toMatchObject({
      source: "runtime",
      denial: "write_not_allowed",
      delivery: "available",
      output: { isError: true },
    });
    expect(
      await f.db.select().from(mcpExecution).where(eq(mcpExecution.sessionId, f.sessionId)),
    ).toHaveLength(0);
    for (const table of [mcpAdmission, mcpApproval])
      expect(
        await f.db
          .select()
          .from(table)
          .innerJoin(mcpExecution, eq(table.executionId, mcpExecution.id))
          .where(eq(mcpExecution.sessionId, f.sessionId)),
      ).toHaveLength(0);
    expect(f.events).not.toContain("lease");
    expect(f.leases).toBe(0);
    expect(await f.command("stats")).toMatchObject({ calls: 0, credentialRequests: 0 });
  } finally {
    if (automationId) await f.db.delete(automation).where(eq(automation.id, automationId));
    await f.cleanup();
  }
}, 20000);

test("explicit constrained automation grant preserves unavailable errors, original acceptance, and argument limits", async () => {
  const f = await effectFixture({
    classification: "write",
    constraints: [{ properties: { x: { maximum: 1 } } }],
  });
  let automationId: string | undefined;
  try {
    automationId = await linkAutomation(f);
    const runtime = recovery(f),
      ref = requestMarker(f);
    const transaction = spyOn(f.db, "transaction").mockRejectedValueOnce(
      Error("private-database-error"),
    );
    try {
      await expect(runtime.decideRequest(ref)).rejects.toThrow(
        "MCP request acceptance unavailable",
      );
    } finally {
      transaction.mockRestore();
    }
    const decision = await runtime.decideRequest(ref);
    expect(decision.kind).toBe("accepted");
    if (decision.kind !== "accepted") throw Error("expected automation acceptance");
    const call = (await f.journal.get(decision.execution.executionId))!;
    expect(call.authorizationKind).toBe("automation");
    expect((await f.driver.driveOnce(call.id)).state).toBe("succeeded");
    expect(
      await f.db.select().from(mcpApproval).where(eq(mcpApproval.executionId, call.id)),
    ).toHaveLength(0);
    f.request.toolCallId = "restricted-call";
    f.request.originalArguments = '{"x":2}';
    f.requests.set(f.request.toolCallId, { ...f.request });
    const restricted = await runtime.decideRequest(requestMarker(f));
    if (restricted.kind !== "accepted") throw Error("expected schema validation path");
    const failed = await f.driver.driveOnce(restricted.execution.executionId);
    expect(failed.state).toBe("failed");
    expect(failed.intentId).toBeNull();
    expect(failed.result).toMatchObject({ source: "local_validation", output: { isError: true } });
    expect(await f.journal.getAdmission(failed.id)).toBeNull();
    await f.db.update(automation).set({ enabled: false }).where(eq(automation.id, automationId));
    expect(
      await runtime.decideRequest({
        ...ref,
        deadlineMs: 0,
        requestEventIdx: "2",
        runId: "changed-run",
      }),
    ).toEqual(decision);
    expect((await f.journal.get(call.id))!.originalArguments).toBe('{ "x": 1 }');
    expect(f.leases).toBe(1);
    expect((await f.command("stats")).calls).toBe(1);
  } finally {
    if (automationId) await f.db.delete(automation).where(eq(automation.id, automationId));
    await f.cleanup();
  }
}, 20000);
