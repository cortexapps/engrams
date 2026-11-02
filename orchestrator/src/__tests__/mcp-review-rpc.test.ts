import { expect, test } from "bun:test";
import { createClient, createRouterTransport, Code, ConnectError } from "@connectrpc/connect";
import { createConnectTransport } from "@connectrpc/connect-node";
import { create, toBinary, toJson } from "@bufbuild/protobuf";
import type { AddressInfo } from "node:net";
import { Hono } from "hono";
import { eq } from "drizzle-orm";
import { checkDb } from "../db/client.ts";
import { task, user, pendingToolCall, mcpApproval } from "../db/schema.ts";
import { schemaVerifiedFixture as reviewFixture } from "./mcp-review-fixture.ts";
import { buildServer } from "../server.ts";
import {
  IntegrationService,
  McpCompletionSettlement,
  GetMcpExecutionResultResponseSchema,
  GetSessionMcpStateResponseSchema,
} from "../gen/engram/app/v1/integration_pb.ts";
import { SessionService } from "../gen/engram/app/v1/session_pb.ts";
import { mcpHandlers, type McpServices } from "../mcp/contracts.ts";
import { makePendingToolCallStore } from "../tools/pending-tool-calls.ts";
import { makeExternalToolCompletionGuard } from "../rpc/tool-completion-guard.ts";
import { registerPassthrough } from "../rpc/passthrough.ts";

const reachable = process.env.ORCHESTRATOR_DATABASE_URL ? await checkDb() : false;
async function wire(f: Awaited<ReturnType<typeof reviewFixture>>, producers = true) {
  const { taskSession, profile } = await import("../db/schema.ts");
  const { canonicalDigest } = await import("../mcp/hash.ts");
  const publication = { revision: 1, actorId: f.actorId, grantsHash: f.snapshot.profileGrantsHash };
  const { bindingHash: _oldHash, ...body } = f.snapshot;
  f.snapshot.profilePublication = publication;
  f.snapshot.bindingHash = canonicalDigest({ ...body, profilePublication: publication });
  f.input.bindingHash = f.snapshot.bindingHash;
  f.input.resolvedTool.bindingHash = f.snapshot.bindingHash;
  f.input.resolvedTool.profilePublication = publication;
  const [sessionRow] = await f.db
    .select()
    .from(taskSession)
    .where(eq(taskSession.sessionId, f.sessionId));
  await f.db
    .update(taskSession)
    .set({
      mcpBindingEnvelope: {
        ...sessionRow!.mcpBindingEnvelope!,
        profilePublication: publication,
        bindingHash: f.snapshot.bindingHash,
      },
    })
    .where(eq(taskSession.sessionId, f.sessionId));
  await f.db
    .update(profile)
    .set({ mcpGrantPublication: publication })
    .where(eq(profile.id, f.profileId));
  const service: McpServices = { enabled: true, repository: f.repository, journal: f.journal };
  // Injected policy and resolver are fixtures, not a live coordinator/host ACK.
  if (producers)
    service.reviewProducers = {
      bindings: { readCurrent: async () => structuredClone(f.snapshot) },
      policy: {
        readWritePolicy: async () => ({
          state: {
            policy: {
              ref: f.input.requestPolicyContext.acceptedPolicyRef,
              writeEligibility: "write_eligible",
              current: true,
              initialized: true,
            },
            planReview: {
              policyRef: f.input.requestPolicyContext.acceptedPolicyRef,
              toolCallId: "plan-a",
              runId: "run-1",
              eventIdx: 1n,
              requestDigest: "plan-digest",
              argsJson: '{"plan":"original"}',
            },
          },
        }),
      },
    };
  let forwarded = 0;
  const upstream = createRouterTransport((router) =>
    router.service(SessionService, {
      completeToolCall: async (req) => {
        forwarded++;
        return { sessionId: req.sessionId };
      },
    }),
  );
  const server = buildServer(new Hono(), (router) => {
    const getSession = async () => ({
      user: { id: f.actorId, role: "admin", email: `${f.actorId}@example.test` },
    });
    router.service(
      IntegrationService,
      mcpHandlers({
        services: service,
        connections: f.connections,
        getSession,
        now: () => f.now,
        connectionToProto: (row) => ({ id: row.id }),
      }),
    );
    registerPassthrough(
      router,
      [{ service: SessionService, methods: ["CompleteToolCall"] }],
      upstream,
      getSession,
      async () => f.actorId,
      {
        "SessionService.CompleteToolCall": makeExternalToolCompletionGuard({
          pendingCalls: makePendingToolCallStore(f.db),
        }),
      },
    );
  });
  const url = await new Promise<string>((resolve) =>
    server.listen(0, "127.0.0.1", () =>
      resolve(`http://127.0.0.1:${(server.address() as AddressInfo).port}/rpc`),
    ),
  );
  const transport = createConnectTransport({ baseUrl: url, httpVersion: "1.1" });
  return {
    client: createClient(IntegrationService, transport),
    session: createClient(SessionService, transport),
    service,
    forwarded: () => forwarded,
    close: () =>
      new Promise<void>((resolve, reject) =>
        server.close((error) => (error ? reject(error) : resolve())),
      ),
  };
}
async function code(promise: Promise<unknown>, expected: Code) {
  try {
    await promise;
    throw new Error("expected denial");
  } catch (error) {
    expect(error).toBeInstanceOf(ConnectError);
    expect((error as ConnectError).code).toBe(expected);
  }
}
test.skipIf(!reachable)(
  "registered review handlers return original arguments and bind decisions to all context",
  async () => {
    const f = await reviewFixture(),
      s = await wire(f);
    try {
      const call = await f.journal.create(f.input, { kind: "approval" }, f.now);
      await f.journal.requestApproval(call.id, f.deploymentId, f.now);
      const state = await s.client.getSessionMcpState({
        sessionId: f.sessionId,
        planToolCallId: "plan-a",
      });
      expect(state.availability).toBe("available");
      expect(state.writePolicy?.planReview?.argsJson).toBe('{"plan":"original"}');
      expect(state.approvals[0]!.originalArguments).toBe(f.input.originalArguments);
      expect(state.executions[0]!.allowedActions).toContain("approve");
      expect(JSON.stringify(toJson(GetSessionMcpStateResponseSchema, state))).not.toContain(
        "mcp.test.key",
      );
      const expected = state.approvals[0]!.expectedReviewContext!;
      await code(
        s.client.decideMcpApproval({ executionId: call.id, approve: true }),
        Code.InvalidArgument,
      );
      await code(
        s.client.decideMcpApproval({
          executionId: call.id,
          approve: true,
          expectedReviewContext: { ...expected, argumentHash: "changed" },
        }),
        Code.Aborted,
      );
      const result = await s.client.decideMcpApproval({
        executionId: call.id,
        approve: true,
        expectedReviewContext: expected,
      });
      expect(result.state).toBe("authorized");
      expect(result.receipt?.reviewedContext).toEqual(expected);
      expect(result.receipt?.actorId).toBe(f.actorId);
      expect((await f.journal.get(call.id))!.bootId).toBeNull();
    } finally {
      await s.close();
      await f.cleanup();
    }
  },
);
test.skipIf(!reachable)(
  "fresh authorization denies cached admin on empty reads and changed permission decisions",
  async () => {
    const f = await reviewFixture(),
      s = await wire(f);
    try {
      await f.db.update(task).set({ createdByUserId: "other-owner" }).where(eq(task.id, f.taskId));
      await code(s.client.getSessionMcpState({ sessionId: f.sessionId }), Code.PermissionDenied);
      await code(s.client.listMcpApprovals({ sessionId: f.sessionId }), Code.PermissionDenied);
      await f.db.update(task).set({ createdByUserId: f.actorId }).where(eq(task.id, f.taskId));
      const call = await f.journal.create(f.input, { kind: "approval" }, f.now);
      await f.journal.requestApproval(call.id, f.deploymentId, f.now);
      const read = await s.client.listMcpApprovals({ sessionId: f.sessionId });
      await f.db.update(user).set({ banned: true }).where(eq(user.id, f.actorId));
      await code(
        s.client.decideMcpApproval({
          executionId: call.id,
          approve: true,
          expectedReviewContext: read.approvals[0]!.expectedReviewContext,
        }),
        Code.PermissionDenied,
      );
      expect((await f.journal.get(call.id))!.state).toBe("waiting_approval");
    } finally {
      await s.close();
      await f.cleanup();
    }
  },
);
test.skipIf(!reachable)(
  "generic completion and forged guest projection cannot change MCP authority",
  async () => {
    const f = await reviewFixture(),
      s = await wire(f);
    try {
      const call = await f.journal.create(f.input, { kind: "approval" }, f.now);
      await f.journal.requestApproval(call.id, f.deploymentId, f.now);
      const projection = makePendingToolCallStore(f.db);
      await projection.recordRequested({
        sessionId: f.sessionId,
        toolCallId: f.input.toolCallId,
        toolName: f.input.resolvedTool.modelToolName,
        handling: "handled",
        requestedAt: f.now,
      });
      await code(
        s.session.completeToolCall({
          sessionId: f.sessionId,
          toolCallId: f.input.toolCallId,
          resultJson: '{"decision":"approve","ok":true}',
        }),
        Code.PermissionDenied,
      );
      await projection.markSubmitted(f.sessionId, f.input.toolCallId, f.now);
      await projection.markCompleted(f.sessionId, f.input.toolCallId, f.now);
      expect((await f.journal.get(call.id))!.state).toBe("waiting_approval");
      const [approval] = await f.db
        .select()
        .from(mcpApproval)
        .where(eq(mcpApproval.executionId, call.id));
      expect(approval!.decision).toBe("pending");
      expect(approval!.consumedAt).toBeNull();
      expect(s.forwarded()).toBe(0);
    } finally {
      await f.db.delete(pendingToolCall).where(eq(pendingToolCall.sessionId, f.sessionId));
      await s.close();
      await f.cleanup();
    }
  },
);
test.skipIf(!reachable)(
  "state pages are bounded and missing producers never offer approval",
  async () => {
    const f = await reviewFixture(),
      s = await wire(f, false);
    try {
      for (let i = 0; i < 18; i++)
        await f.journal.create(
          {
            ...f.input,
            toolCallId: `page-${i}`,
            requestPolicyContext: { ...f.input.requestPolicyContext, toolCallId: `page-${i}` },
          },
          { kind: "approval" },
          f.now,
        );
      const first = await s.client.getSessionMcpState({ sessionId: f.sessionId });
      expect(first.executions).toHaveLength(16);
      expect(first.availability).toBe("unavailable");
      expect(first.approvals).toHaveLength(0);
      expect(first.executions.flatMap((c) => c.allowedActions)).not.toContain("approve");
      const next = await s.client.getSessionMcpState({
        sessionId: f.sessionId,
        afterExecutionId: first.nextExecutionId,
      });
      expect(next.executions).toHaveLength(2);
      expect(
        new Set([...first.executions, ...next.executions].map((c) => c.executionId)).size,
      ).toBe(18);
      expect(
        toBinary(GetSessionMcpStateResponseSchema, create(GetSessionMcpStateResponseSchema, first))
          .byteLength,
      ).toBeLessThanOrEqual(2 * 1024 * 1024);
    } finally {
      await s.close();
      await f.cleanup();
    }
  },
);

test.skipIf(!reachable)(
  "byte-limited pages preserve whole arguments, independent schema roots and int64 policy identity",
  async () => {
    const documents = [
      {
        $schema: "http://json-schema.org/draft-07/schema#",
        $id: "urn:test:constraint",
        definitions: { value: { type: "string" } },
        type: "object",
        properties: { value: { $ref: "#/definitions/value" } },
      },
      {
        $schema: "https://json-schema.org/draft/2020-12/schema",
        $id: "urn:test:constraint",
        $defs: { value: { maxLength: 250000 } },
        type: "object",
        properties: { value: { $ref: "#/$defs/value" } },
      },
    ];
    const f = await reviewFixture(documents),
      s = await wire(f);
    try {
      f.input.requestPolicyContext.acceptedPolicyRef.policyRevision = 9007199254740993n;
      const { canonicalDigest } = await import("../mcp/hash.ts");
      const args = { value: '"'.repeat(120000) },
        originalArguments = JSON.stringify(args);
      for (let i = 0; i < 10; i++) {
        const call = await f.journal.create(
          {
            ...f.input,
            originalArguments,
            argumentHash: canonicalDigest(args),
            toolCallId: `large-${i}`,
            requestPolicyContext: { ...f.input.requestPolicyContext, toolCallId: `large-${i}` },
          },
          { kind: "approval" },
          f.now,
        );
        await f.journal.requestApproval(call.id, f.deploymentId, f.now);
      }
      const first = await s.client.getSessionMcpState({ sessionId: f.sessionId });
      expect(first.executions.length).toBeLessThan(10);
      expect(first.executions.length).toBeGreaterThan(0);
      const pages = [first];
      while (pages.at(-1)!.hasMoreExecutions) {
        pages.push(
          await s.client.getSessionMcpState({
            sessionId: f.sessionId,
            afterExecutionId: pages.at(-1)!.nextExecutionId,
            omitBindings: true,
          }),
        );
        if (pages.length > 10) throw new Error("state cursor did not advance");
      }
      const approvals = pages.flatMap((page) => page.approvals);
      expect(approvals).toHaveLength(10);
      for (const item of approvals) {
        expect(item.originalArguments).toBe(originalArguments);
        expect(item.argumentConstraintDocumentsJson.map((value) => JSON.parse(value))).toEqual(
          documents,
        );
        expect(item.expectedReviewContext!.policyRevision).toBe("9007199254740993");
      }
      for (const page of pages) {
        expect(toBinary(GetSessionMcpStateResponseSchema, page).byteLength).toBeLessThanOrEqual(
          2 * 1024 * 1024,
        );
        expect(
          Buffer.byteLength(JSON.stringify(toJson(GetSessionMcpStateResponseSchema, page))),
        ).toBeLessThanOrEqual(2 * 1024 * 1024);
      }
    } finally {
      await s.close();
      await f.cleanup();
    }
  },
);

test.skipIf(!reachable)(
  "no-call sessions expose complete immutable binding tools with independent paging",
  async () => {
    const documents = [
      { $schema: "http://json-schema.org/draft-07/schema#", $id: "urn:shared", type: "object" },
      {
        $schema: "https://json-schema.org/draft/2020-12/schema",
        $id: "urn:shared",
        type: "object",
      },
    ];
    const f = await reviewFixture(documents, 18),
      s = await wire(f, false);
    try {
      const first = await s.client.getSessionMcpState({ sessionId: f.sessionId });
      expect(first.executions).toHaveLength(0);
      expect(first.bindingTools).toHaveLength(16);
      expect(first.bindingReceipt?.current).toBe(false);
      expect(first.hasMoreBindingTools).toBe(true);
      const second = await s.client.getSessionMcpState({
        sessionId: f.sessionId,
        afterBindingToolName: first.nextBindingToolName,
        omitExecutions: true,
      });
      expect(second.bindingTools).toHaveLength(2);
      expect(second.hasMoreBindingTools).toBe(false);
      const tools = [...first.bindingTools, ...second.bindingTools];
      expect(new Set(tools.map((tool) => tool.modelToolName)).size).toBe(18);
      for (const tool of tools) {
        expect(tool.argumentConstraintDocumentsJson.map((value) => JSON.parse(value))).toEqual(
          documents,
        );
        expect(tool.catalogId).toBe(f.catalog.id);
        expect(tool.unrestricted).toBe(false);
      }
      expect(JSON.stringify(toJson(GetSessionMcpStateResponseSchema, first))).not.toContain(
        "mcp.test.key",
      );
      expect(toBinary(GetSessionMcpStateResponseSchema, first).byteLength).toBeLessThanOrEqual(
        2 * 1024 * 1024,
      );
    } finally {
      await s.close();
      await f.cleanup();
    }
  },
);

test.skipIf(!reachable)("a fresh policy lineage change invalidates a saved review", async () => {
  const f = await reviewFixture(),
    s = await wire(f);
  try {
    const call = await f.journal.create(f.input, { kind: "approval" }, f.now);
    await f.journal.requestApproval(call.id, f.deploymentId, f.now);
    const state = await s.client.getSessionMcpState({ sessionId: f.sessionId });
    const expected = state.approvals[0]!.expectedReviewContext!;
    s.service.reviewProducers!.policy.readWritePolicy = async () => ({
      state: {
        policy: {
          ref: f.input.requestPolicyContext.acceptedPolicyRef,
          writeEligibility: "write_eligible",
          current: true,
          initialized: true,
          ancestorPolicyRefs: [
            { sessionId: "ancestor-changed", bindingEpoch: 0n, policyRevision: 2n },
          ],
        },
      },
    });
    await code(
      s.client.decideMcpApproval({
        executionId: call.id,
        approve: true,
        expectedReviewContext: expected,
      }),
      Code.Aborted,
    );
    const current = await s.client.getSessionMcpState({ sessionId: f.sessionId });
    expect(current.approvals).toHaveLength(0);
    expect(current.executions[0]!.allowedActions).not.toContain("approve");
    expect((await f.journal.get(call.id))!.state).toBe("waiting_approval");
  } finally {
    await s.close();
    await f.cleanup();
  }
});

test.skipIf(!reachable)(
  "typed state reports legacy leaf publication as unavailable",
  async () => {
    const { taskSession } = await import("../db/schema.ts");
    const { canonicalDigest } = await import("../mcp/hash.ts");
    const f = await reviewFixture();
    const s = await wire(f);
    try {
      const call = await f.journal.create(f.input, { kind: "approval" }, f.now);
      await f.journal.requestApproval(call.id, f.deploymentId, f.now);
      const published = await s.client.getSessionMcpState({ sessionId: f.sessionId });
      expect(published.availability).toBe("available");
      expect(published.executions[0]!.allowedActions).toContain("approve");
      const { bindingHash: _old, ...body } = f.snapshot;
      const legacy = { ...body, profilePublication: null };
      const hash = canonicalDigest(legacy);
      f.snapshot.profilePublication = null;
      f.snapshot.bindingHash = hash;
      const [stored] = await f.db
        .select()
        .from(taskSession)
        .where(eq(taskSession.sessionId, f.sessionId));
      await f.db
        .update(taskSession)
        .set({
          mcpBindingEnvelope: {
            ...stored!.mcpBindingEnvelope!,
            profilePublication: null,
            bindingHash: hash,
          },
        })
        .where(eq(taskSession.sessionId, f.sessionId));
      const state = await s.client.getSessionMcpState({ sessionId: f.sessionId });
      expect(state.availability).toBe("unavailable");
      expect(state.bindingReceipt?.staleReasons).toContain("publication_required");
      expect(state.executions[0]!.allowedActions).not.toContain("approve");
      expect(state.writePolicy?.allowedActions ?? []).not.toContain("approve_current_plan");
    } finally {
      await s.close();
      await f.cleanup();
    }
  },
  20000,
);

test.skipIf(!reachable)(
  "registered result read enforces session association and current human ownership with dispatch disabled",
  async () => {
    const f = await reviewFixture(),
      s = await wire(f);
    const { mcpExecution } = await import("../db/schema.ts");
    const other = await reviewFixture();
    s.service.enabled = false;
    try {
      await f.db.update(task).set({ createdByUserId: f.actorId }).where(eq(task.id, other.taskId));
      const call = await f.journal.create(f.input, { kind: "approval" }, f.now);
      const result = {
        outcome: "failed",
        delivery: "available",
        output: {
          content: [{ type: "text", text: "<img src=https://example.test>" }],
          structuredContent: { x: 1 },
          isError: true,
        },
      };
      await f.db
        .update(mcpExecution)
        .set({ state: "failed", result, version: 2 })
        .where(eq(mcpExecution.id, call.id));
      await f.db
        .update(mcpExecution)
        .set({ completionEnqueuedVersion: 2 })
        .where(eq(mcpExecution.id, call.id));
      const response = await s.client.getMcpExecutionResult({
        sessionId: f.sessionId,
        executionId: call.id,
      });
      expect(JSON.parse(response.resultJson!)).toEqual(result);
      expect(response.state).toBe("failed");
      expect(response.settlement).toBe(McpCompletionSettlement.SETTLED_OR_TARGET_GONE);
      await code(
        s.client.getMcpExecutionResult({ sessionId: f.sessionId, executionId: "missing" }),
        Code.FailedPrecondition,
      );
      await code(
        s.client.getMcpExecutionResult({ sessionId: "other", executionId: call.id }),
        Code.PermissionDenied,
      );
      await code(
        s.client.getMcpExecutionResult({ sessionId: other.sessionId, executionId: call.id }),
        Code.FailedPrecondition,
      );
      await f.db.update(task).set({ createdByUserId: "other-owner" }).where(eq(task.id, f.taskId));
      await code(
        s.client.getMcpExecutionResult({ sessionId: f.sessionId, executionId: call.id }),
        Code.PermissionDenied,
      );
    } finally {
      await s.close();
      await other.cleanup();
      await f.cleanup();
    }
  },
);

test.skipIf(!reachable)(
  "result reader bounds data and preserves absent and pending settlement",
  async () => {
    const f = await reviewFixture(),
      s = await wire(f);
    const { mcpExecution } = await import("../db/schema.ts");
    const { canonicalJson } = await import("../mcp/hash.ts");
    async function make(name: string, result: Record<string, unknown>) {
      const call = await f.journal.create(
        {
          ...f.input,
          toolCallId: name,
          requestPolicyContext: { ...f.input.requestPolicyContext, toolCallId: name },
        },
        { kind: "approval" },
        f.now,
      );
      await f.db
        .update(mcpExecution)
        .set({ state: "failed", result, version: 2 })
        .where(eq(mcpExecution.id, call.id));
      return call;
    }
    try {
      const active = await f.journal.create(f.input, { kind: "approval" }, f.now);
      const absent = await s.client.getMcpExecutionResult({
        sessionId: f.sessionId,
        executionId: active.id,
      });
      expect(absent.resultJson).toBeUndefined();
      expect(absent.settlement).toBe(McpCompletionSettlement.NOT_APPLICABLE);
      const envelope = (text: string) => ({
        outcome: "failed",
        delivery: "available",
        output: { content: [{ type: "text", text }], isError: true },
      });
      const overhead = Buffer.byteLength(canonicalJson(envelope("")));
      const max = await make("max", envelope("x".repeat(1024 * 1024 - overhead)));
      const response = await s.client.getMcpExecutionResult({
        sessionId: f.sessionId,
        executionId: max.id,
      });
      expect(Buffer.byteLength(response.resultJson!)).toBe(1024 * 1024);
      expect(response.settlement).toBe(McpCompletionSettlement.PENDING);
      const oversized = await make("oversized", envelope("x".repeat(1024 * 1024 - overhead + 1)));
      await code(
        s.client.getMcpExecutionResult({ sessionId: f.sessionId, executionId: oversized.id }),
        Code.FailedPrecondition,
      );
      const escaped = await make(
        "escaped",
        envelope("\\".repeat(Math.floor((1024 * 1024 - overhead) / 2))),
      );
      const escapedResult = await s.client.getMcpExecutionResult({
        sessionId: f.sessionId,
        executionId: escaped.id,
      });
      expect(
        Buffer.byteLength(
          JSON.stringify(toJson(GetMcpExecutionResultResponseSchema, escapedResult)),
        ),
      ).toBeLessThanOrEqual(2 * 1024 * 1024);
      const unavailable = await f.journal.create(
        {
          ...f.input,
          toolCallId: "unavailable",
          requestPolicyContext: { ...f.input.requestPolicyContext, toolCallId: "unavailable" },
        },
        { kind: "approval" },
        f.now,
      );
      await f.db
        .update(mcpExecution)
        .set({ state: "authorized", version: 2 })
        .where(eq(mcpExecution.id, unavailable.id));
      await f.db
        .update(mcpExecution)
        .set({
          state: "dispatching",
          version: 3,
          bootId: "boot",
          intentId: "intent-unavailable",
          intentExpiresAt: f.now,
        })
        .where(eq(mcpExecution.id, unavailable.id));
      await f.db
        .update(mcpExecution)
        .set({ state: "succeeded", version: 4, resultUnavailable: true })
        .where(eq(mcpExecution.id, unavailable.id));
      const noResult = await s.client.getMcpExecutionResult({
        sessionId: f.sessionId,
        executionId: unavailable.id,
      });
      expect(noResult.resultUnavailable).toBe(true);
      expect(noResult.resultJson).toBeUndefined();
      const malformed = await make("malformed", {
        outcome: "failed",
        delivery: "available",
        output: { content: [{ type: "image" }], isError: true },
      });
      await code(
        s.client.getMcpExecutionResult({ sessionId: f.sessionId, executionId: malformed.id }),
        Code.FailedPrecondition,
      );
    } finally {
      await s.close();
      await f.cleanup();
    }
  },
);

test.skipIf(!reachable)(
  "result read checks current admin, banned and service identities",
  async () => {
    const f = await reviewFixture(),
      s = await wire(f);
    try {
      const call = await f.journal.create(f.input, { kind: "approval" }, f.now);
      await f.db.update(task).set({ createdByUserId: "other-owner" }).where(eq(task.id, f.taskId));
      await code(
        s.client.getMcpExecutionResult({ sessionId: f.sessionId, executionId: call.id }),
        Code.PermissionDenied,
      );
      await f.db.update(user).set({ role: "admin" }).where(eq(user.id, f.actorId));
      expect(
        (await s.client.getMcpExecutionResult({ sessionId: f.sessionId, executionId: call.id }))
          .state,
      ).toBe("waiting_approval");
      await f.db.update(user).set({ banned: true }).where(eq(user.id, f.actorId));
      await code(
        s.client.getMcpExecutionResult({ sessionId: f.sessionId, executionId: call.id }),
        Code.PermissionDenied,
      );
      await f.db
        .update(user)
        .set({ banned: false, email: `apikey+${f.actorId}@service.local` })
        .where(eq(user.id, f.actorId));
      await code(
        s.client.getMcpExecutionResult({ sessionId: f.sessionId, executionId: call.id }),
        Code.PermissionDenied,
      );
    } finally {
      await s.close();
      await f.cleanup();
    }
  },
);

test.skipIf(!reachable)(
  "disabled dispatch keeps registered history, pending reviews and exact cancellation",
  async () => {
    const f = await reviewFixture(),
      s = await wire(f);
    try {
      const call = await f.journal.create(f.input, { kind: "approval" }, f.now);
      await f.journal.requestApproval(call.id, f.deploymentId, f.now);
      const enabled = await s.client.getSessionMcpState({ sessionId: f.sessionId });
      const expected = enabled.approvals[0]!.expectedReviewContext!;
      let cancels = 0;
      const runtime = {
        cancel: async () => {
          cancels++;
        },
        prepareRebind: async () => {
          throw Error("must not rebind");
        },
        rebind: async () => {
          throw Error("must not rebind");
        },
      };
      s.service.runtime = runtime;
      s.service.enabled = false;
      const state = await s.client.getSessionMcpState({ sessionId: f.sessionId });
      expect(state.availability).toBe("unavailable");
      expect(state.executions[0]!.review!.originalArguments).toBe(f.input.originalArguments);
      expect(state.executions[0]!.allowedActions).toEqual(["cancel"]);
      expect(state.bindingReceipt!.allowedActions).toEqual([]);
      expect(state.writePolicy?.allowedActions).toEqual(["approve_current_plan"]);
      expect((await s.client.listMcpApprovals({ sessionId: f.sessionId })).approvals).toHaveLength(
        1,
      );
      await code(
        s.client.decideMcpApproval({
          executionId: call.id,
          approve: true,
          expectedReviewContext: expected,
        }),
        Code.FailedPrecondition,
      );
      await code(
        s.client.prepareMcpRebind({
          sessionId: f.sessionId,
          expectedBindingHash: f.snapshot.bindingHash,
        }),
        Code.FailedPrecondition,
      );
      await code(s.client.discoverMcp({}), Code.FailedPrecondition);
      await code(s.client.publishMcp({}), Code.FailedPrecondition);
      await code(s.client.rebindMcp({}), Code.FailedPrecondition);
      await code(
        s.client.cancelMcpCall({
          executionId: call.id,
          expectedJournalRevision: call.version + 100,
        }),
        Code.Aborted,
      );
      const current = (await f.journal.get(call.id))!;
      const canceled = await s.client.cancelMcpCall({
        executionId: call.id,
        expectedJournalRevision: current.version,
      });
      expect(canceled.state).toBe("canceled");
      expect(cancels).toBe(1);
      await f.db.update(task).set({ createdByUserId: "other-owner" }).where(eq(task.id, f.taskId));
      await code(s.client.getSessionMcpState({ sessionId: f.sessionId }), Code.PermissionDenied);
    } finally {
      await s.close();
      await f.cleanup();
    }
  },
);

test.skipIf(!reachable)(
  "disabled dispatch preserves exact unknown resolution and journal-only cancellation authority",
  async () => {
    const f = await reviewFixture(),
      s = await wire(f);
    try {
      const { reviewContext } = await import("../mcp/journal-review.ts");
      const call = await f.journal.create(f.input, { kind: "approval" }, f.now);
      const approval = await f.journal.requestApproval(call.id, f.deploymentId, f.now);
      const approved = (
        await f.journal.decide(call.id, f.actorId, true, reviewContext(call, approval), f.now)
      ).call;
      await f.journal.admit(call.id, f.deploymentId, f.now);
      const lease = (await f.journal.getAdmission(call.id))!;
      const dispatched = await f.journal.dispatch(
        call.id,
        { bootId: "boot", intentId: crypto.randomUUID(), expiresAt: lease.expiresAt },
        f.now,
        {
          executionId: call.id,
          requestDigest: call.requestDigest,
          bindingHash: call.bindingHash,
          principalId: call.principalId,
          expectedJournalRevision: approved.version,
          reservationId: lease.reservationId,
          reservationExpiresAt: lease.expiresAt,
        },
      );
      s.service.enabled = false;
      expect(s.service.runtime).toBeUndefined();
      const canceled = await s.client.cancelMcpCall({
        executionId: call.id,
        expectedJournalRevision: dispatched.version,
      });
      expect(canceled.state).toBe("outcome_unknown");
      expect(canceled.receipt!.state).toBe("outcome_unknown");
      const state = await s.client.getSessionMcpState({ sessionId: f.sessionId });
      expect(state.unknownWriteBlocks).toContain(f.connection.id);
      const expected = state.executions[0]!.expectedUnknownContext!;
      const request = {
        executionId: call.id,
        outcome: "acknowledged",
        expectedUnknownContext: expected,
      };
      await code(
        s.client.resolveMcpOutcome({
          ...request,
          expectedUnknownContext: { ...expected, intentId: "other" },
        }),
        Code.Aborted,
      );
      await f.db
        .update(user)
        .set({ email: "apikey+fixture@service.local" })
        .where(eq(user.id, f.actorId));
      await code(s.client.getSessionMcpState({ sessionId: f.sessionId }), Code.PermissionDenied);
      await code(s.client.resolveMcpOutcome(request), Code.PermissionDenied);
      await f.db
        .update(user)
        .set({ email: `${f.actorId}@example.test` })
        .where(eq(user.id, f.actorId));
      await f.db.update(task).set({ createdByUserId: "other-owner" }).where(eq(task.id, f.taskId));
      await code(s.client.resolveMcpOutcome(request), Code.PermissionDenied);
      await f.db.update(user).set({ role: "admin" }).where(eq(user.id, f.actorId));
      const resolved = await s.client.resolveMcpOutcome(request);
      expect(resolved.receipt!.expectedContext).toEqual(expected);
      expect((await f.journal.get(call.id))!.state).toBe("outcome_unknown");
      expect((await f.journal.get(call.id))!.result).toBeNull();
      expect(
        (await s.client.getSessionMcpState({ sessionId: f.sessionId })).resolutionReceipts,
      ).toHaveLength(1);
    } finally {
      await s.close();
      await f.cleanup();
    }
  },
);
