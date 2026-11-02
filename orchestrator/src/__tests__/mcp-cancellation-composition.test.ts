import { expect, test } from "bun:test";
import { createClient, createRouterTransport } from "@connectrpc/connect";
import { eq } from "drizzle-orm";
import { reviewFixture } from "./mcp-review-fixture.ts";
import { makeMcpRecovery } from "../mcp/recovery.ts";
import { SessionPolicyService } from "../gen/engram/mcp/v1/session_policy_pb.ts";
import { taskSession } from "../db/schema.ts";
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
