import { expect, test } from "bun:test";
import { createClient, createRouterTransport } from "@connectrpc/connect";
import { eq } from "drizzle-orm";
import { reviewFixture } from "./mcp-review-fixture.ts";
import {
  makeMcpRecovery,
  type McpRequestDecision,
  type McpRequestMarker,
} from "../mcp/recovery.ts";
import { SessionPolicyService } from "../gen/engram/mcp/v1/session_policy_pb.ts";
import { mcpExecution, taskSession } from "../db/schema.ts";
import { effectFixture } from "./mcp-effect-fixture.ts";
import { makeToolConsumer } from "../listeners/tool-consumer.ts";
import { makePendingToolCallStore } from "../tools/pending-tool-calls.ts";
import { createToolRegistry } from "../tools/registry.ts";
import { verifyMcpTestDatabases } from "./helpers/mcp-test-isolation.ts";
await verifyMcpTestDatabases();

test("authenticated cancellation checks exact current accepted identity before durable mutation", async () => {
  const f = await reviewFixture();
  try {
    const call = await f.journal.create(f.input, { kind: "approval" }, f.now);
    let status = "current",
      checks = 0,
      completions = 0;
    const policy = createClient(
      SessionPolicyService,
      createRouterTransport((router) =>
        router.service(SessionPolicyService, {
          validateExecutionIdentity: async (req) => {
            checks++;
            expect(req.originalArguments).toBe(f.input.originalArguments);
            expect(req.modelToolName).toBe(f.input.resolvedTool.modelToolName);
            expect(req.sessionId).toBe(f.sessionId);
            expect(req.runId).toBe(f.input.runId);
            return { protocol: 1, status };
          },
        }),
      ),
    );
    const runtime = makeMcpRecovery({
      db: f.db,
      deploymentId: f.deploymentId,
      now: () => f.now,
      cancellationPolicy: policy,
      completer: {
        completeToolCall: async () => {
          completions++;
        },
      },
    });
    const input = {
      sessionId: f.sessionId,
      runId: f.input.runId,
      toolCallId: f.input.toolCallId,
      modelToolName: f.input.resolvedTool.modelToolName,
    };
    await runtime.cancelRequest({ ...input, runId: "old-run" });
    await runtime.cancelRequest({ ...input, modelToolName: "mcp_other" });
    expect(checks).toBe(0);
    expect((await f.journal.get(call.id))!.state).toBe("waiting_approval");
    status = "stale";
    await runtime.cancelRequest(input);
    expect((await f.journal.get(call.id))!.state).toBe("waiting_approval");
    status = "unknown";
    await expect(runtime.cancelRequest(input)).rejects.toThrow("unavailable");
    status = "current";
    const envelope = f.snapshot;
    await f.db
      .update(taskSession)
      .set({ mcpBindingEnvelope: { ...envelope, bindingHash: `sha256:${"0".repeat(64)}` } })
      .where(eq(taskSession.sessionId, f.sessionId));
    await runtime.cancelRequest(input);
    expect((await f.journal.get(call.id))!.state).toBe("waiting_approval");
    const { immutableBindings: _bindings, ...original } = envelope;
    await f.db
      .update(taskSession)
      .set({ mcpBindingEnvelope: original })
      .where(eq(taskSession.sessionId, f.sessionId));
    await runtime.cancelRequest(input);
    const cancelled = (await f.journal.get(call.id))!;
    expect(cancelled.state).toBe("canceled");
    expect(cancelled.result).toBeNull();
    expect(cancelled.completionEnqueuedVersion).toBe(0);
    const version = cancelled.version;
    await runtime.cancelRequest(input);
    expect((await f.journal.get(call.id))!.version).toBe(version);
    expect(completions).toBe(0);
  } finally {
    await f.cleanup();
  }
});

for (const cancelBeforeDecision of [true, false])
  test(`cancellation ${cancelBeforeDecision ? "before acceptance" : "after denial"} without a journal row permits the next tool request`, async () => {
    const f = await effectFixture();
    try {
      const request = (toolCallId: string, idx: bigint) => ({
        idx,
        kind: "tool_call_requested",
        payloadJson: JSON.stringify({
          run_id: f.request.runId,
          tool_call_id: toolCallId,
          name: f.request.modelToolName,
          args_json: f.request.originalArguments,
        }),
      });
      const first = request(f.request.toolCallId, 1n);
      const next = request("next-call", 3n);
      const events = [first, next];
      const runtime = makeMcpRecovery({
        db: f.db,
        driver: f.driver,
        deploymentId: f.deploymentId,
        now: () => new Date(),
        readRequestEvent: async (_sessionId, idx) => events.find((event) => event.idx === idx),
      });
      const started: McpRequestMarker[] = [];
      const consumer = makeToolConsumer({
        registry: createToolRegistry(),
        pendingCalls: makePendingToolCallStore(f.db),
        now: () => new Date(),
        startWorkflow: async () => {
          throw Error("unexpected native tool dispatch");
        },
        startMcpRequest: async (ref) => {
          started.push(ref);
        },
        cancelMcpRequest: runtime.cancelRequest,
      });
      await consumer.handle(first, { sessionId: f.sessionId });
      // The persisted native cancellation makes the exact request fence stale.
      f.setFence(false);
      const decide = () => runtime.decideRequest(started[0]!);
      if (!cancelBeforeDecision)
        expect(await decide()).toEqual({ kind: "denied", code: "authority_changed" });
      await consumer.handle(
        {
          idx: 2n,
          kind: "tool_call_cancel_requested",
          payloadJson: JSON.stringify({
            run_id: f.request.runId,
            tool_call_id: f.request.toolCallId,
            name: f.request.modelToolName,
          }),
        },
        { sessionId: f.sessionId },
      );
      if (cancelBeforeDecision)
        expect(await decide()).toEqual({ kind: "denied", code: "authority_changed" });
      expect(
        await f.db.select().from(mcpExecution).where(eq(mcpExecution.sessionId, f.sessionId)),
      ).toEqual([]);
      await consumer.handle(next, { sessionId: f.sessionId });
      expect(started.map((ref) => ref.toolCallId)).toEqual([f.request.toolCallId, "next-call"]);
      f.requests.set("next-call", { ...f.request, toolCallId: "next-call" });
      f.setFence(true);
      expect((await runtime.decideRequest(started[1]!)).kind).toBe("accepted");
      expect((await f.command("stats")).calls).toBe(0);
      expect(f.leases).toBe(0);
    } finally {
      await f.cleanup();
    }
  }, 20_000);

test("cancellation during acceptance leaves the inserted call for stale-authority reconciliation", async () => {
  const f = await effectFixture();
  let release!: () => void;
  const held = new Promise<void>((resolve) => {
    release = resolve;
  });
  let inserted!: () => void;
  const reached = new Promise<void>((resolve) => {
    inserted = resolve;
  });
  let accepting: Promise<McpRequestDecision> | undefined;
  try {
    let fences = 0;
    f.beforeFence(async () => {
      if (++fences === 2) {
        inserted();
        await held;
      }
    });
    const runtime = makeMcpRecovery({
      db: f.db,
      driver: f.driver,
      deploymentId: f.deploymentId,
      now: () => new Date(),
      cancellationPolicy: createClient(
        SessionPolicyService,
        createRouterTransport((router) =>
          router.service(SessionPolicyService, {
            validateExecutionIdentity: async () => ({ protocol: 1, status: "stale" }),
          }),
        ),
      ),
      readRequestEvent: async (_sessionId, idx) => ({
        idx,
        kind: "tool_call_requested",
        payloadJson: JSON.stringify({
          run_id: f.request.runId,
          tool_call_id: f.request.toolCallId,
          name: f.request.modelToolName,
          args_json: f.request.originalArguments,
        }),
      }),
    });
    accepting = runtime.decideRequest({
      ...f.request,
      requestEventIdx: "1",
      deadlineMs: Date.now() + 60_000,
    });
    await Promise.race([
      reached,
      accepting.then(() => {
        throw Error("acceptance ended before the inserted-call fence");
      }),
    ]);
    f.setFence(false);
    await runtime.cancelRequest(f.request);
    release();
    const decision = await accepting;
    expect(decision.kind).toBe("accepted");
    if (decision.kind !== "accepted") throw Error("inserted request lost");
    await f.driver.reconcile(decision.execution.executionId);
    const call = (await f.journal.get(decision.execution.executionId))!;
    expect(call.state).toBe("canceled");
    expect(call.intentId).toBeNull();
    expect((await f.command("stats")).calls).toBe(0);
    expect(f.leases).toBe(0);
  } finally {
    release();
    try {
      await accepting;
    } finally {
      await f.cleanup();
    }
  }
}, 20_000);
