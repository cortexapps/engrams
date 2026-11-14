import { expect, test } from "bun:test";
import { create } from "@bufbuild/protobuf";
import { createClient, createRouterTransport } from "@connectrpc/connect";
import { eq, sql } from "drizzle-orm";
import { reviewFixture } from "./mcp-review-fixture.ts";
import { makeProfileStore } from "../db/profiles.ts";
import { task, taskSession, user, mcpExecution } from "../db/schema.ts";
import { compileSessionBindings, type McpSessionBindingSnapshot } from "../mcp/session-bindings.ts";
import {
  bindingSnapshotColumns,
  readLiveBindingAuthority,
  makeStoredSessionBindingResolver,
} from "../mcp/binding-store.ts";
import { compileChildSessionBindings } from "../mcp/session-child.ts";
import { resolveSnapshotTool } from "../mcp/session-resolver.ts";
import { reviewContext } from "../mcp/journal-review.ts";
import { unknownContext, type McpCallInput } from "../mcp/journal.ts";
import { makeMcpEffectDriver } from "../mcp/effect-driver.ts";
import { unwrapCredentialControlClient } from "../mcp/control-client.ts";
import { SessionPolicyService } from "../gen/engram/mcp/v1/session_policy_pb.ts";
import { CredentialControlService } from "../gen/engram/mcp/v1/control_pb.ts";
import { McpWritePolicyRefSchema } from "../gen/engram/app/v1/session_pb.ts";

// Explicit bound-child fixture. Native child activation and host enforcement remain closed.
// The journal, resolver, permission checks, approvals and admission use real Postgres.
async function family(withOtherConnection = false) {
  const f = await reviewFixture([], 2);
  let extra: Awaited<ReturnType<typeof reviewFixture>> | undefined;
  try {
    extra = withOtherConnection ? await reviewFixture() : undefined;
    await f.repository.publish(
      f.connection.id,
      f.draft.id,
      1,
      f.catalog.tools.map((tool, index) => ({
        ...tool,
        classification: index === 0 ? "write" : "read",
        enabled: true,
      })),
      f.publisherId,
      f.now,
    );
    await f.repository.setEnabled(f.connection.id, 1, true, f.now);
    const tasks: string[] = [];
    const snapshots = new Map<string, McpSessionBindingSnapshot>();
    let workerCalls = 0;
    let now = f.now;
    await f.db.update(user).set({ role: "admin" }).where(eq(user.id, f.actorId));
    const profiles = makeProfileStore(f.db);
    await profiles.update(
      f.profileId,
      {
        ...(await profiles.get(f.profileId))!,
        integrationGrants: [
          ...f.authority.profileGrants,
          ...(extra?.authority.profileGrants ?? []),
        ],
      },
      { actorId: f.actorId, expectedRevision: 0 },
    );
    await f.db.update(user).set({ role: "user" }).where(eq(user.id, f.actorId));
    const authority = await readLiveBindingAuthority(
      f.profileId,
      f.authority.operatorPolicies,
      f.db,
    );
    const nativeConnections = [f.connection, ...(extra ? [extra.connection] : [])].map((row) => ({
      id: row.id,
      alias: row.alias,
      provider: "mcp",
      displayName: row.displayName,
      config: row.config,
    }));
    const root = compileSessionBindings({
      launchPolicy: { profileId: f.profileId, integrationGrants: authority.profileGrants },
      liveAuthority: authority,
      bindingEpoch: 0,
    });
    await f.db
      .update(taskSession)
      .set(bindingSnapshotColumns(root, nativeConnections))
      .where(eq(taskSession.sessionId, f.sessionId));
    snapshots.set(f.sessionId, root);
    const policyRef = (sessionId: string) =>
      create(McpWritePolicyRefSchema, {
        sessionId,
        bindingEpoch: BigInt(snapshots.get(sessionId)!.bindingEpoch),
        policyRevision: 1n,
      });
    const policy = createClient(
      SessionPolicyService,
      createRouterTransport((router) =>
        router.service(SessionPolicyService, {
          readWritePolicy: async (req) => ({
            state: {
              policy: {
                current: true,
                initialized: true,
                writeEligibility: "write_eligible",
                ref: policyRef(req.sessionId),
                ancestorPolicyRefs: snapshots
                  .get(req.sessionId)!
                  .ancestorAuthorityRefs.map((ref) => policyRef(ref.sessionId)),
              },
            },
          }),
          validateDispatchFence: async () => ({ protocol: 1, status: "current" }),
          validateExecutionIdentity: async () => ({ protocol: 1, status: "current" }),
        }),
      ),
    );
    const resolver = makeStoredSessionBindingResolver(
      {
        control: policy,
        readOperatorPolicies: async () => f.authority.operatorPolicies,
        enforceManagedDeny: async (expected) => expected,
      },
      f.db,
    );
    const driver = makeMcpEffectDriver({
      db: f.db,
      deploymentId: f.deploymentId,
      policy,
      resolver,
      control: unwrapCredentialControlClient(
        createClient(
          CredentialControlService,
          createRouterTransport((router) => router.service(CredentialControlService, {})),
        ),
      ),
      enabled: () => true,
      now: () => now,
      workerTls: () => {
        workerCalls++;
        throw Error("fixture worker must not be invoked");
      },
    });
    function input(
      sessionId: string,
      taskId: string,
      callId = crypto.randomUUID(),
      connectionId = f.connection.id,
      toolIndex = 0,
    ): McpCallInput {
      const snapshot = snapshots.get(sessionId)!;
      const binding = snapshot.immutableBindings.find(
        (binding) => binding.connectionId === connectionId,
      )!;
      const resolved = resolveSnapshotTool(snapshot, binding.tools[toolIndex]!.name)!;
      return {
        ...f.input,
        sessionId,
        taskId,
        toolCallId: callId,
        bindingHash: snapshot.bindingHash,
        resolvedTool: resolved,
        connectionId,
        catalogId: binding.catalogId,
        classification: resolved.reviewedTool.classification,
        upstreamName: resolved.reviewedTool.upstreamName,
        configRevision: binding.configRevision,
        authGeneration: binding.authGeneration,
        requestPolicyContext: {
          ...f.input.requestPolicyContext,
          sessionId,
          toolCallId: callId,
          acceptedPolicyRef: policyRef(sessionId),
          ancestorPolicyRefs: snapshot.ancestorAuthorityRefs.map((ref) => policyRef(ref.sessionId)),
        },
        deadline: new Date(now.getTime() + 600_000),
      };
    }
    const request = (call: McpCallInput) => ({
      sessionId: call.sessionId,
      runId: call.runId,
      toolCallId: call.toolCallId,
      modelToolName: call.resolvedTool.modelToolName,
      originalArguments: call.originalArguments,
    });
    async function child(parentId: string | null = f.sessionId) {
      const taskId = crypto.randomUUID(),
        sessionId = crypto.randomUUID();
      const compilation = {
        launchPolicy: { profileId: f.profileId, integrationGrants: authority.profileGrants },
        liveAuthority: authority,
        bindingEpoch: 0,
        requestedGrants: authority.profileGrants,
      };
      const snapshot = parentId
        ? await compileChildSessionBindings({
            ...compilation,
            spawningSessionId: parentId,
            readParentCurrent: resolver.readCurrent,
          })
        : compileSessionBindings(compilation);
      await f.db.insert(task).values({
        id: taskId,
        type: "chat",
        createdByUserId: f.actorId,
        spawningSessionId: parentId,
      });
      tasks.push(taskId);
      await f.db.insert(taskSession).values({
        taskId,
        sessionId,
        profileId: f.profileId,
        integrationPrincipalId: f.actorId,
        ...bindingSnapshotColumns(snapshot, nativeConnections),
      });
      snapshots.set(sessionId, snapshot);
      return input(sessionId, taskId);
    }
    async function ready(input: McpCallInput, accepted?: typeof mcpExecution.$inferSelect) {
      const call =
        accepted ??
        (await f.journal.create(
          input,
          { kind: input.classification === "read" ? "read" : "approval" },
          now,
        ));
      await f.journal.admitSchema(call.id, f.deploymentId, now);
      const schema = (await f.journal.getAdmission(call.id))!;
      await f.journal.finishSchema(
        call.id,
        schema.reservationId,
        schema.expiresAt,
        call.requestDigest,
        true,
        now,
      );
      let authorized = call;
      if (call.authorizationKind === "approval") {
        const approval = await f.journal.requestApproval(call.id, f.deploymentId, now);
        authorized = (
          await f.journal.decide(call.id, f.actorId, true, reviewContext(call, approval), now)
        ).call;
      }
      await f.journal.admit(call.id, f.deploymentId, now);
      const lease = (await f.journal.getAdmission(call.id))!;
      const proof = {
        executionId: call.id,
        requestDigest: call.requestDigest,
        bindingHash: call.bindingHash,
        principalId: call.principalId,
        expectedJournalRevision: authorized.version,
        reservationId: lease.reservationId,
        reservationExpiresAt: lease.expiresAt,
      };
      const intent = {
        bootId: "fixture-boot",
        intentId: crypto.randomUUID(),
        expiresAt: lease.expiresAt,
      };
      return { call: authorized, proof, intent };
    }
    async function unknownParent() {
      const readyCall = await ready(input(f.sessionId, f.taskId));
      await f.journal.dispatch(readyCall.call.id, readyCall.intent, now, readyCall.proof);
      const call = await f.journal.cancelInternal(readyCall.call.id, "interrupt", now);
      expect(call.state).toBe("outcome_unknown");
      return call;
    }
    return {
      ...f,
      extraConnectionId: extra?.connection.id,
      driver,
      request,
      input,
      child,
      ready,
      unknownParent,
      snapshots,
      get workerCalls() {
        return workerCalls;
      },
      advance(value: Date) {
        now = value;
      },
      async cleanup() {
        for (const id of tasks.reverse()) await f.db.delete(task).where(eq(task.id, id));
        await f.cleanup();
        await extra?.cleanup();
      },
    };
  } catch (error) {
    await f.cleanup();
    await extra?.cleanup();
    throw error;
  }
}

test("parent unknown blocks fresh and already-bound child acceptance with new call IDs", async () => {
  const f = await family();
  try {
    const existing = await f.child();
    await f.unknownParent();
    const fresh = await f.child();
    for (const input of [existing, fresh]) {
      await expect(f.driver.acceptToolRequest(f.request(input))).rejects.toThrow("write authority");
      expect(
        await f.db.select().from(mcpExecution).where(eq(mcpExecution.sessionId, input.sessionId)),
      ).toHaveLength(0);
    }
    expect(f.workerCalls).toBe(0);
  } finally {
    await f.cleanup();
  }
});

test("accepted child cannot start worker or cross final dispatch after parent becomes unknown", async () => {
  const f = await family();
  try {
    const input = await f.child();
    const accepted = await f.driver.acceptToolRequest(f.request(input));
    const ready = await f.ready(input, accepted);
    await f.unknownParent();
    expect((await f.driver.acceptToolRequest(f.request(input))).id).toBe(accepted.id);
    await expect(f.driver.driveOnce(accepted.id)).rejects.toThrow("unknown write");
    await expect(
      f.journal.dispatch(ready.call.id, ready.intent, f.now, ready.proof),
    ).rejects.toThrow("unknown write");
    expect((await f.journal.get(accepted.id))!.state).toBe("authorized");
    expect(f.workerCalls).toBe(0);
  } finally {
    await f.cleanup();
  }
});

test("resolved parent remains blocking until intent expiry; unresolved expiry alone never releases", async () => {
  const f = await family();
  try {
    const parent = await f.unknownParent();
    const child = await f.child();
    f.advance(new Date(parent.intentExpiresAt!.getTime() + 1));
    await expect(f.driver.acceptToolRequest(f.request(child))).rejects.toThrow("write authority");
    f.advance(f.now);
    await f.journal.resolveUnknown(
      parent.id,
      f.actorId,
      "acknowledged",
      f.now,
      unknownContext(parent),
    );
    await expect(f.driver.acceptToolRequest(f.request(child))).rejects.toThrow("write authority");
    f.advance(new Date(parent.intentExpiresAt!.getTime() + 1));
    const accepted = await f.driver.acceptToolRequest(f.request(child));
    const ready = await f.ready(child, accepted);
    expect(
      (
        await f.journal.dispatch(
          ready.call.id,
          ready.intent,
          new Date(parent.intentExpiresAt!.getTime() + 1),
          ready.proof,
        )
      ).state,
    ).toBe("dispatching");
    expect(f.workerCalls).toBe(0);
  } finally {
    await f.cleanup();
  }
});

test("credential-broker current assertion checks ancestor outcomes after child barrier", async () => {
  const f = await family();
  try {
    const child = await f.ready(await f.child());
    await f.journal.dispatch(child.call.id, child.intent, f.now, child.proof);
    await f.unknownParent();
    await expect(
      f.journal.assertDispatchCurrent(child.call.id, child.proof, f.now),
    ).rejects.toThrow("unknown write");
  } finally {
    await f.cleanup();
  }
});

test("independent session sharing connection and parent of an unknown child remain allowed", async () => {
  const f = await family();
  try {
    const child = await f.ready(await f.child());
    await f.journal.dispatch(child.call.id, child.intent, f.now, child.proof);
    await f.journal.cancelInternal(child.call.id, "interrupt", f.now);
    const sibling = await f.child();
    for (const input of [f.input(f.sessionId, f.taskId), sibling, await f.child(null)])
      expect((await f.driver.acceptToolRequest(f.request(input))).state).toBe("waiting_approval");
    await f.unknownParent();
    expect((await f.driver.acceptToolRequest(f.request(await f.child(null)))).state).toBe(
      "waiting_approval",
    );
  } finally {
    await f.cleanup();
  }
});

for (const change of ["detached", "other-parent", "parent-epoch", "parent-terminal"] as const)
  test(`trusted ancestry rejects ${change} before new acceptance or worker`, async () => {
    const f = await family();
    try {
      const input = await f.child();
      if (change === "detached" || change === "other-parent") {
        const other = change === "other-parent" ? await f.child(null) : null;
        await f.db
          .update(task)
          .set({ spawningSessionId: other?.sessionId ?? null })
          .where(eq(task.id, input.taskId));
      } else if (change === "parent-terminal") {
        await f.db.update(task).set({ status: "failed" }).where(eq(task.id, f.taskId));
      } else {
        const envelope = (
          await f.db.select().from(taskSession).where(eq(taskSession.sessionId, f.sessionId))
        )[0]!.mcpBindingEnvelope!;
        await f.db
          .update(taskSession)
          .set({ mcpBindingEnvelope: { ...envelope, bindingEpoch: 1 } })
          .where(eq(taskSession.sessionId, f.sessionId));
      }
      await expect(f.driver.acceptToolRequest(f.request(input))).rejects.toThrow();
      await expect(f.journal.create(input, { kind: "approval" }, f.now)).rejects.toThrow();
      expect(f.workerCalls).toBe(0);
    } finally {
      await f.cleanup();
    }
  });

// Hold the parent execution row. Real cancellation takes the connection lock before it
// waits here. Child dispatch must then wait behind that cancellation on the connection.
test("parent unknown and child final dispatch serialize on the connection lock", async () => {
  const f = await family();
  try {
    const child = await f.ready(await f.child());
    const parent = await f.ready(f.input(f.sessionId, f.taskId));
    await f.journal.dispatch(parent.call.id, parent.intent, f.now, parent.proof);
    let release!: () => void;
    let locked!: (pid: number) => void;
    const gate = new Promise<void>((resolve) => {
      release = resolve;
    });
    const acquired = new Promise<number>((resolve) => {
      locked = resolve;
    });
    const holder = f.db.transaction(async (tx) => {
      await tx.select().from(mcpExecution).where(eq(mcpExecution.id, parent.call.id)).for("update");
      const pid = await tx.execute<{ pid: number }>(sql`select pg_backend_pid() as pid`);
      locked(pid.rows[0]!.pid);
      await gate;
    });
    async function waitingOn(pid: number) {
      for (let i = 0; i < 100; i++) {
        const rows = await f.db.execute<{ pid: number }>(
          sql`select pid from pg_stat_activity where ${pid} = any(pg_blocking_pids(pid))`,
        );
        if (rows.rows[0]) return rows.rows[0].pid;
        await Bun.sleep(5);
      }
      throw Error("journal operation did not reach the held row");
    }
    const pid = await acquired;
    const cancel = f.journal.cancelInternal(parent.call.id, "interrupt", f.now);
    let result: Promise<string> | undefined;
    try {
      const cancelPid = await waitingOn(pid);
      result = f.journal.dispatch(child.call.id, child.intent, f.now, child.proof).then(
        () => "sent",
        (error) => String(error),
      );
      expect(await waitingOn(cancelPid)).toBeNumber();
    } finally {
      release();
    }
    await holder;
    expect((await cancel).state).toBe("outcome_unknown");
    expect(await result).toContain("unknown write");
    expect((await f.journal.get(child.call.id))!.state).toBe("authorized");
  } finally {
    await f.cleanup();
  }
});

test("parent unknown permits child reads and writes on another connection", async () => {
  const f = await family(true);
  try {
    const child = await f.child();
    await f.unknownParent();
    const read = f.input(child.sessionId, child.taskId, crypto.randomUUID(), f.connection.id, 1);
    const other = f.input(child.sessionId, child.taskId, crypto.randomUUID(), f.extraConnectionId!);
    const acceptedRead = await f.driver.acceptToolRequest(f.request(read));
    expect(acceptedRead.state).toBe("authorized");
    const readyRead = await f.ready(read, acceptedRead);
    await f.journal.dispatch(readyRead.call.id, readyRead.intent, f.now, readyRead.proof);
    expect(
      (await f.journal.assertDispatchCurrent(readyRead.call.id, readyRead.proof, f.now)).state,
    ).toBe("dispatching");
    const accepted = await f.driver.acceptToolRequest(f.request(other));
    const ready = await f.ready(other, accepted);
    expect((await f.journal.dispatch(ready.call.id, ready.intent, f.now, ready.proof)).state).toBe(
      "dispatching",
    );
    expect((await f.journal.assertDispatchCurrent(ready.call.id, ready.proof, f.now)).state).toBe(
      "dispatching",
    );
  } finally {
    await f.cleanup();
  }
});

test("grandparent restriction survives a new child binding and changed arguments", async () => {
  const f = await family();
  try {
    const parent = await f.child();
    const grandchild = await f.child(parent.sessionId);
    await f.unknownParent();
    await expect(
      f.driver.acceptToolRequest({ ...f.request(grandchild), originalArguments: '{"x":2}' }),
    ).rejects.toThrow("write authority");
    const original = f.snapshots.get(grandchild.sessionId)!;
    const { bindingHash: _, ...body } = original;
    const replacement = { ...body, bindingEpoch: 1 };
    const { canonicalDigest } = await import("../mcp/hash.ts");
    const snapshot = { ...replacement, bindingHash: canonicalDigest(replacement) };
    f.snapshots.set(grandchild.sessionId, snapshot);
    const [stored] = await f.db
      .select()
      .from(taskSession)
      .where(eq(taskSession.sessionId, grandchild.sessionId));
    await f.db
      .update(taskSession)
      .set(bindingSnapshotColumns(snapshot, stored!.integrationConnections!))
      .where(eq(taskSession.sessionId, grandchild.sessionId));
    const changed = f.input(grandchild.sessionId, grandchild.taskId);
    await expect(f.driver.acceptToolRequest(f.request(changed))).rejects.toThrow("write authority");
    expect(f.workerCalls).toBe(0);
  } finally {
    await f.cleanup();
  }
});

test("actual acceptance still checks the current principal before any worker operation", async () => {
  const f = await family();
  try {
    const child = await f.child();
    await f.db.update(user).set({ banned: true }).where(eq(user.id, f.actorId));
    await expect(f.driver.acceptToolRequest(f.request(child))).rejects.toThrow(
      "owner or administrator",
    );
    expect(f.workerCalls).toBe(0);
  } finally {
    await f.cleanup();
  }
});

test("ancestor unknown preserves verified child result and exact duplicate identity", async () => {
  const f = await family();
  try {
    const input = await f.child();
    const child = await f.ready(input);
    const dispatched = await f.journal.dispatch(child.call.id, child.intent, f.now, child.proof);
    await f.unknownParent();
    const completed = await f.journal.finish(
      dispatched.id,
      dispatched.version,
      "succeeded",
      { output: "fixture verified result" },
      false,
      f.now,
    );
    expect((await f.journal.create(input, { kind: "approval" }, f.now)).id).toBe(completed.id);
    expect((await f.driver.acceptToolRequest(f.request(input))).result).toEqual(completed.result);
    expect((await f.driver.driveOnce(completed.id)).state).toBe("succeeded");
    await expect(
      f.driver.acceptToolRequest({ ...f.request(input), originalArguments: '{"x":2}' }),
    ).rejects.toThrow("identity changed");
    await expect(
      f.driver.acceptToolRequest(f.request(f.input(input.sessionId, input.taskId))),
    ).rejects.toThrow("write authority");
    expect(f.workerCalls).toBe(0);
  } finally {
    await f.cleanup();
  }
});
