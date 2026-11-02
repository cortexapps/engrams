import { expect, test } from "bun:test";
import { effectFixture } from "./mcp-effect-fixture.ts";
import { reviewContext } from "../mcp/journal-review.ts";

test("actual worker read in unknown policy preserves raw args, schema lease and redacted outcome once", async () => {
  const f = await effectFixture({ eligibility: "unknown" });
  try {
    const call = await f.driver.acceptToolRequest(f.request);
    expect(call.originalArguments).toBe(f.request.originalArguments);
    const prepared = await f.driver.driveOnce(call.id);
    expect(prepared.state).toBe("succeeded");
    expect(prepared.result).toMatchObject({
      outcome: "succeeded",
      delivery: "available",
      output: { isError: false, structuredContent: { echo: "[redacted] | [redacted]" } },
    });
    expect(JSON.stringify(prepared)).not.toContain("known-literal-token");
    expect((await f.command("stats")).calls).toBe(1);
    expect(f.leases).toBe(1);
    expect(f.events.at(-1)).toBe("read-fence");
    expect((await f.driver.acceptToolRequest(f.request)).id).toBe(call.id);
    expect((await f.driver.driveOnce(call.id)).state).toBe("succeeded");
    expect((await f.command("stats")).calls).toBe(1);
    await expect(
      f.driver.acceptToolRequest({ ...f.request, originalArguments: '{"x":1}' }),
    ).rejects.toThrow();
  } finally {
    await f.cleanup();
  }
}, 20000);

test("write schema releases capacity before approval and uses a distinct execute generation", async () => {
  const f = await effectFixture({ classification: "write" });
  try {
    const call = await f.driver.acceptToolRequest(f.request);
    expect((await f.driver.driveOnce(call.id)).state).toBe("waiting_approval");
    const schema = (await f.journal.getAdmission(call.id))!;
    expect(schema.state).toBe("released");
    expect(schema.schemaDigest).toBe(call.requestDigest);
    expect(f.leases).toBe(0);
    const approval = await f.journal.requestApproval(call.id, f.deploymentId, new Date());
    await f.journal.decide(call.id, f.actorId, true, reviewContext(call, approval), new Date());
    let executeId = "";
    f.beforeLease(async () => {
      executeId = (await f.journal.getAdmission(call.id))!.reservationId;
    });
    expect((await f.driver.driveOnce(call.id)).state).toBe("succeeded");
    expect(executeId).not.toBe(schema.reservationId);
    expect(f.events.at(-1)).toBe("write-fence");
    expect((await f.command("stats")).calls).toBe(1);
  } finally {
    await f.cleanup();
  }
}, 20000);

for (const response of ["image", "error", "malformed", "oversize", "normalized-limit"] as const)
  test(`actual worker ${response} retains outcome and delivery without replay`, async () => {
    const f = await effectFixture({ classification: "write", response });
    try {
      const call = await f.driver.acceptToolRequest(f.request);
      await f.driver.driveOnce(call.id);
      const approval = await f.journal.requestApproval(call.id, f.deploymentId, new Date());
      await f.journal.decide(call.id, f.actorId, true, reviewContext(call, approval), new Date());
      const result = await f.driver.driveOnce(call.id);
      expect(result.state).toBe(
        response === "error"
          ? "failed"
          : response === "malformed" || response === "oversize"
            ? "outcome_unknown"
            : "succeeded",
      );
      expect(result.resultUnavailable).toBe(
        response === "image" || response === "normalized-limit",
      );
      expect(JSON.stringify(result)).not.toContain("known-literal-token");
      if (response === "error")
        expect(result.result).toMatchObject({
          output: { isError: true, structuredContent: { echo: "[redacted] | [redacted]" } },
        });
      await f.driver.driveOnce(call.id);
      expect((await f.command("stats")).calls).toBe(1);
      if (result.state === "outcome_unknown") {
        const next = { ...f.request, toolCallId: "call-2" };
        f.requests.set(next.toolCallId, next);
        await expect(f.driver.acceptToolRequest(next)).rejects.toThrow("write authority");
      }
    } finally {
      await f.cleanup();
    }
  }, 20000);

test("plan reads pass while plan and unknown writes deny before schema or secrets", async () => {
  for (const classification of ["read", "write"] as const) {
    const f = await effectFixture({ classification, eligibility: "plan" });
    try {
      if (classification === "read") {
        const call = await f.driver.acceptToolRequest(f.request);
        expect((await f.driver.driveOnce(call.id)).state).toBe("succeeded");
      } else {
        await expect(f.driver.acceptToolRequest(f.request)).rejects.toThrow();
        f.setEligibility("unknown");
        await expect(f.driver.acceptToolRequest(f.request)).rejects.toThrow();
        expect(f.leases).toBe(0);
        expect((await f.command("stats")).credentialRequests).toBe(0);
      }
    } finally {
      await f.cleanup();
    }
  }
}, 20000);

test("independent input constraints reject without approval or credentials", async () => {
  const f = await effectFixture({
    classification: "write",
    constraints: [
      {
        $schema: "https://json-schema.org/draft/2020-12/schema",
        $id: "https://schema.example/constraint",
        properties: { x: { maximum: 0 } },
      },
    ],
  });
  try {
    const call = await f.driver.acceptToolRequest(f.request);
    const denied = await f.driver.driveOnce(call.id);
    expect(denied.state).toBe("failed");
    expect(denied.intentId).toBeNull();
    expect(denied.result).toMatchObject({
      source: "local_validation",
      delivery: "available",
      output: { isError: true },
    });
    expect(await f.journal.getAdmission(call.id)).toBeNull();
    expect(await f.journal.pending(f.sessionId, f.actorId, new Date())).toHaveLength(0);
    expect(f.leases).toBe(0);
    expect((await f.command("stats")).calls).toBe(0);
  } finally {
    await f.cleanup();
  }
}, 20000);

test("output schema compiles separately and failed output validation preserves known success", async () => {
  const f = await effectFixture({
    outputSchema: {
      $schema: "https://json-schema.org/draft/2020-12/schema",
      type: "object",
      required: ["missing"],
    },
  });
  try {
    const call = await f.driver.acceptToolRequest(f.request);
    const result = await f.driver.driveOnce(call.id);
    expect(result.state).toBe("succeeded");
    expect(result.resultUnavailable).toBe(true);
    expect(result.result).toMatchObject({ reason: "output_schema" });
    expect((await f.command("stats")).calls).toBe(1);
  } finally {
    await f.cleanup();
  }
}, 20000);

test("changed raw bytes and oversized arguments never acquire credentials", async () => {
  const f = await effectFixture();
  try {
    await expect(
      f.driver.acceptToolRequest({ ...f.request, originalArguments: '{"x":1}' }),
    ).rejects.toThrow("authority");
    await expect(
      f.driver.acceptToolRequest({
        ...f.request,
        originalArguments: JSON.stringify({ x: "x".repeat(256 * 1024) }),
      }),
    ).rejects.toThrow("limit");
    expect(f.leases).toBe(0);
  } finally {
    await f.cleanup();
  }
}, 20000);

test("duplicate concurrent drive crosses one barrier and calls upstream once", async () => {
  const f = await effectFixture({ classification: "write" });
  try {
    const [a, b] = await Promise.all([
      f.driver.acceptToolRequest(f.request),
      f.driver.acceptToolRequest(f.request),
    ]);
    expect(a.id).toBe(b.id);
    expect(a.deadline).toEqual(b.deadline);
    await f.driver.driveOnce(a.id);
    const approval = await f.journal.requestApproval(a.id, f.deploymentId, new Date());
    await f.journal.decide(a.id, f.actorId, true, reviewContext(a, approval), new Date());
    const results = await Promise.allSettled([f.driver.driveOnce(a.id), f.driver.driveOnce(a.id)]);
    expect(results.some((r) => r.status === "fulfilled" && r.value.state === "succeeded")).toBe(
      true,
    );
    expect((await f.command("stats")).calls).toBe(1);
    expect(f.leases).toBe(1);
  } finally {
    await f.cleanup();
  }
}, 20000);

test("credential lease rotation preserves frozen binding and redacts the new token", async () => {
  const f = await effectFixture();
  try {
    const first = await f.driver.acceptToolRequest(f.request);
    expect((await f.driver.driveOnce(first.id)).state).toBe("succeeded");
    f.refresh();
    const request = { ...f.request, toolCallId: "call-2" };
    f.requests.set(request.toolCallId, request);
    const second = await f.driver.acceptToolRequest(request);
    expect(second.bindingHash).toBe(first.bindingHash);
    expect(second.requestContext?.resolvedTool.binding).toEqual(
      first.requestContext?.resolvedTool.binding,
    );
    const result = await f.driver.driveOnce(second.id);
    expect(result.state).toBe("succeeded");
    expect(JSON.stringify(result)).not.toContain("refreshed-literal-token");
    expect((await f.command("stats")).calls).toBe(2);
  } finally {
    await f.cleanup();
  }
}, 20000);

import { eq } from "drizzle-orm";
import { integrationConnection, task, taskSession, user } from "../db/schema.ts";
import { statusMetadata } from "../mcp/credential-store.ts";
import { makeProfileStore } from "../db/profiles.ts";

for (const change of [
  "ban",
  "owner",
  "principal",
  "publication",
  "account",
  "generation",
  "endpoint",
  "zone",
] as const)
  test(`live ${change} change denies the frozen call before credential lease`, async () => {
    const f = await effectFixture();
    try {
      const call = await f.driver.acceptToolRequest(f.request);
      if (change === "ban")
        await f.db.update(user).set({ banned: true }).where(eq(user.id, f.actorId));
      else if (change === "owner")
        await f.db.update(task).set({ createdByUserId: null }).where(eq(task.id, f.taskId));
      else if (change === "principal")
        await f.db
          .update(taskSession)
          .set({ integrationPrincipalId: "other" })
          .where(eq(taskSession.sessionId, f.sessionId));
      else if (change === "publication") {
        await f.db.update(user).set({ role: "admin" }).where(eq(user.id, f.actorId));
        const profiles = makeProfileStore(f.db);
        await profiles.update(f.profileId, (await profiles.get(f.profileId))!, {
          actorId: f.actorId,
          expectedRevision: 1,
        });
        await f.db.update(user).set({ role: "user" }).where(eq(user.id, f.actorId));
      } else if (change === "account")
        await f.db
          .update(integrationConnection)
          .set({ credentialIdentity: "different-account-scope" })
          .where(eq(integrationConnection.id, f.connection.id));
      else if (change === "generation")
        await f.db
          .update(integrationConnection)
          .set({ authGeneration: call.authGeneration + 1 })
          .where(eq(integrationConnection.id, f.connection.id));
      else if (change === "endpoint")
        await f.db
          .update(integrationConnection)
          .set({ config: { ...f.connection.config, endpoint: "https://changed.example.test/mcp" } })
          .where(eq(integrationConnection.id, f.connection.id));
      else f.operatorPolicies[0]!.policyDigest = `sha256:${"a".repeat(64)}`;
      // A duplicate returns the original authority even after revocation, but cannot dispatch it.
      expect((await f.driver.acceptToolRequest(f.request)).requestDigest).toBe(call.requestDigest);
      await expect(f.driver.driveOnce(call.id)).rejects.toThrow();
      expect(f.leases).toBe(0);
      expect((await f.command("stats")).credentialRequests).toBe(0);
    } finally {
      await f.cleanup();
    }
  }, 20000);

test("changed worker boot between barrier and broker identity releases zero credentials", async () => {
  const f = await effectFixture();
  try {
    const call = await f.driver.acceptToolRequest(f.request);
    let changed = false;
    f.beforeFence(async () => {
      if (!changed && (await f.journal.get(call.id))?.state === "dispatching") {
        changed = true;
        await f.command("restart");
      }
    });
    expect((await f.driver.driveOnce(call.id)).state).toBe("failed");
    expect(changed).toBe(true);
    expect(f.leases).toBe(0);
    expect((await f.command("stats")).credentialRequests).toBe(0);
  } finally {
    await f.cleanup();
  }
}, 20000);

test("raw coordinator publication policy mismatch is denied before secret leasing", async () => {
  const f = await effectFixture();
  try {
    const call = await f.driver.acceptToolRequest(f.request);
    f.status.context!.policyDigest = "a".repeat(64);
    await f.db
      .update(integrationConnection)
      .set({ mcpCredential: { status: statusMetadata(f.status) } })
      .where(eq(integrationConnection.id, f.connection.id));
    expect((await f.driver.driveOnce(call.id)).state).toBe("failed");
    expect(f.leases).toBe(0);
    expect((await f.command("stats")).credentialRequests).toBe(0);
  } finally {
    await f.cleanup();
  }
}, 20000);

test("revocation during credential lease is caught by the second same-generation assertion", async () => {
  const f = await effectFixture({ classification: "write" });
  try {
    const call = await f.driver.acceptToolRequest(f.request);
    await f.driver.driveOnce(call.id);
    const approval = await f.journal.requestApproval(call.id, f.deploymentId, new Date());
    await f.journal.decide(call.id, f.actorId, true, reviewContext(call, approval), new Date());
    f.afterLease(async () => {
      await f.db.update(user).set({ banned: true }).where(eq(user.id, f.actorId));
    });
    expect((await f.driver.driveOnce(call.id)).state).toBe("outcome_unknown");
    expect(f.leases).toBe(1);
    expect((await f.command("stats")).credentialRequests).toBe(0);
  } finally {
    await f.cleanup();
  }
}, 20000);

test("final coordinator fence after a lease blocks changed authority", async () => {
  const f = await effectFixture();
  try {
    const call = await f.driver.acceptToolRequest(f.request);
    let postLeaseFences = 0;
    f.beforeFence(async () => {
      if (f.leases && ++postLeaseFences === 2) f.setFence(false);
    });
    expect((await f.driver.driveOnce(call.id)).state).toBe("failed");
    expect(postLeaseFences).toBe(2);
    expect((await f.command("stats")).credentialRequests).toBe(0);
  } finally {
    await f.cleanup();
  }
}, 20000);

test("global kill during final fence prevents the immediate send", async () => {
  const f = await effectFixture();
  try {
    const call = await f.driver.acceptToolRequest(f.request);
    let postLeaseFences = 0;
    f.beforeFence(async () => {
      if (f.leases && ++postLeaseFences === 2) f.setEnabled(false);
    });
    expect((await f.driver.driveOnce(call.id)).state).toBe("failed");
    expect((await f.command("stats")).credentialRequests).toBe(0);
  } finally {
    await f.cleanup();
  }
}, 20000);

test("unpublished legacy human grants require explicit administrator publication", async () => {
  const f = await effectFixture({ published: false });
  try {
    await expect(f.driver.acceptToolRequest(f.request)).rejects.toThrow(
      "administrator publication",
    );
    expect(f.leases).toBe(0);
  } finally {
    await f.cleanup();
  }
}, 20000);

test("trusted shorter deadline freezes once and duplicate delivery cannot extend it", async () => {
  const f = await effectFixture();
  try {
    const deadline = new Date(Date.now() + 5000);
    const call = await f.driver.acceptToolRequest(f.request, deadline);
    expect(call.deadline).toEqual(deadline);
    expect(
      (await f.driver.acceptToolRequest(f.request, new Date(Date.now() + 60000))).deadline,
    ).toEqual(deadline);
    expect((await f.driver.driveOnce(call.id)).state).toBe("succeeded");
  } finally {
    await f.cleanup();
  }
}, 20000);

test("an unresolved write blocks new writes but permits the separately reviewed read", async () => {
  const f = await effectFixture({ classification: "write", response: "malformed" });
  try {
    const call = await f.driver.acceptToolRequest(f.request);
    await f.driver.driveOnce(call.id);
    const approval = await f.journal.requestApproval(call.id, f.deploymentId, new Date());
    await f.journal.decide(call.id, f.actorId, true, reviewContext(call, approval), new Date());
    expect((await f.driver.driveOnce(call.id)).state).toBe("outcome_unknown");
    const readName = f.snapshot.immutableBindings[0]!.tools.find(
      (tool) => tool.tool.classification === "read",
    )!.name;
    const readRequest = { ...f.request, toolCallId: "read-after-unknown", modelToolName: readName };
    f.requests.set(readRequest.toolCallId, readRequest);
    f.setEligibility("plan");
    const read = await f.driver.acceptToolRequest(readRequest);
    // The fixture still returns malformed output. It proves admission and one read call.
    await f.driver.driveOnce(read.id);
    expect((await f.command("stats")).calls).toBe(2);
    expect(f.leases).toBe(2);
  } finally {
    await f.cleanup();
  }
}, 20000);

test("cancel after provider send leaves an unknown write that cannot replay", async () => {
  const f = await effectFixture({ classification: "write", response: "hold" });
  try {
    const call = await f.driver.acceptToolRequest(f.request);
    await f.driver.driveOnce(call.id);
    const approval = await f.journal.requestApproval(call.id, f.deploymentId, new Date());
    await f.journal.decide(call.id, f.actorId, true, reviewContext(call, approval), new Date());
    const controller = new AbortController();
    const pending = f.driver.driveOnce(call.id, controller.signal);
    const until = Date.now() + 5000;
    while (!(await f.command("stats")).calls && Date.now() < until)
      await new Promise((resolve) => setTimeout(resolve, 10));
    expect((await f.command("stats")).calls).toBe(1);
    await f.journal.cancel(call.id, f.actorId, new Date());
    controller.abort();
    expect((await pending).state).toBe("outcome_unknown");
    await f.driver.driveOnce(call.id);
    expect((await f.command("stats")).calls).toBe(1);
  } finally {
    await f.cleanup();
  }
}, 20000);

import { automation, automationRun, automationSession } from "../db/schema.ts";

test("published automation grant uses exact live automation linkage without human approval", async () => {
  const f = await effectFixture({ classification: "write" });
  const automationId = crypto.randomUUID(),
    runId = crypto.randomUUID();
  try {
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
    await f.db
      .insert(automationSession)
      .values({ sessionId: f.sessionId, runId, blockId: "start" });
    await f.db
      .update(taskSession)
      .set({ integrationPrincipalId: `automation:${automationId}` })
      .where(eq(taskSession.sessionId, f.sessionId));
    await f.db.update(task).set({ createdByUserId: null }).where(eq(task.id, f.taskId));
    const call = await f.driver.acceptToolRequest(f.request);
    expect(call.authorizationKind).toBe("automation");
    expect((await f.driver.driveOnce(call.id)).state).toBe("succeeded");
    const request = { ...f.request, toolCallId: "automation-next" };
    f.requests.set(request.toolCallId, request);
    const next = await f.driver.acceptToolRequest(request);
    await f.db.update(automation).set({ enabled: false }).where(eq(automation.id, automationId));
    await expect(f.driver.driveOnce(next.id)).rejects.toThrow("automation");
    expect((await f.command("stats")).calls).toBe(1);
  } finally {
    await f.db.delete(automation).where(eq(automation.id, automationId));
    await f.cleanup();
  }
}, 20000);

test("live admin role is required when the immutable principal is not the current owner", async () => {
  const f = await effectFixture();
  const adminId = crypto.randomUUID();
  try {
    await f.db.insert(user).values({
      id: adminId,
      name: "Fixture admin",
      email: `${adminId}@example.test`,
      role: "admin",
      createdAt: new Date(),
      updatedAt: new Date(),
    });
    await f.db
      .update(taskSession)
      .set({ integrationPrincipalId: adminId })
      .where(eq(taskSession.sessionId, f.sessionId));
    const call = await f.driver.acceptToolRequest(f.request);
    await f.db.update(user).set({ role: "user" }).where(eq(user.id, adminId));
    await expect(f.driver.driveOnce(call.id)).rejects.toThrow();
    expect(f.leases).toBe(0);
  } finally {
    await f.cleanup();
    await f.db.delete(user).where(eq(user.id, adminId));
  }
}, 20000);

import { create } from "@bufbuild/protobuf";
import { McpWritePolicyRefSchema } from "../gen/engram/app/v1/session_pb.ts";
import { bindingSnapshotColumns, readLiveBindingAuthority } from "../mcp/binding-store.ts";
import { compileSessionBindings } from "../mcp/session-bindings.ts";

test("live ancestor restriction denies child execution", async () => {
  const f = await effectFixture();
  const parentId = crypto.randomUUID(),
    parentTaskId = crypto.randomUUID();
  try {
    const [leaf] = await f.db
      .select()
      .from(taskSession)
      .where(eq(taskSession.sessionId, f.sessionId));
    await f.db.insert(task).values({ id: parentTaskId, type: "chat", createdByUserId: f.actorId });
    await f.db.insert(taskSession).values({
      taskId: parentTaskId,
      sessionId: parentId,
      profileId: f.profileId,
      integrationPrincipalId: f.actorId,
      ...bindingSnapshotColumns(f.snapshot, leaf!.integrationConnections!),
    });
    const authority = await readLiveBindingAuthority(f.profileId, f.operatorPolicies, f.db);
    const child = compileSessionBindings({
      launchPolicy: { profileId: f.profileId, integrationGrants: authority.profileGrants },
      liveAuthority: authority,
      bindingEpoch: 0,
      requestedGrants: authority.profileGrants,
      parentAuthority: { sessionId: parentId, snapshot: f.snapshot },
    });
    await f.db.update(task).set({ spawningSessionId: parentId }).where(eq(task.id, f.taskId));
    await f.db
      .update(taskSession)
      .set(bindingSnapshotColumns(child, leaf!.integrationConnections!))
      .where(eq(taskSession.sessionId, f.sessionId));
    const parentRef = create(McpWritePolicyRefSchema, {
      sessionId: parentId,
      bindingEpoch: 0n,
      policyRevision: 1n,
    });
    f.policies.set(parentId, { ref: parentRef, ancestorPolicyRefs: [] });
    f.policies.get(f.sessionId)!.ancestorPolicyRefs = [parentRef];
    await f.resolver.readCurrent(f.sessionId);
    const call = await f.driver.acceptToolRequest(f.request);
    await f.db.update(task).set({ status: "failed" }).where(eq(task.id, parentTaskId));
    await expect(f.driver.driveOnce(call.id)).rejects.toThrow();
    expect(f.leases).toBe(0);
  } finally {
    await f.db.delete(task).where(eq(task.id, f.taskId));
    await f.db.delete(task).where(eq(task.id, parentTaskId));
    await f.cleanup();
  }
}, 20000);

test("credential assertion rejects an altered execution reservation after leasing", async () => {
  const f = await effectFixture();
  try {
    const call = await f.driver.acceptToolRequest(f.request);
    const { mcpAdmission } = await import("../db/schema.ts");
    f.afterLease(async () => {
      await f.db
        .update(mcpAdmission)
        .set({ reservationId: crypto.randomUUID() })
        .where(eq(mcpAdmission.executionId, call.id));
    });
    expect((await f.driver.driveOnce(call.id)).state).toBe("failed");
    expect(f.leases).toBe(1);
    expect((await f.command("stats")).credentialRequests).toBe(0);
  } finally {
    await f.cleanup();
  }
}, 20000);

test("credential delivery expiry during only the final fence prevents send and replay", async () => {
  const f = await effectFixture({ classification: "write" });
  try {
    const call = await f.driver.acceptToolRequest(f.request);
    await f.driver.driveOnce(call.id);
    const approval = await f.journal.requestApproval(call.id, f.deploymentId, new Date());
    await f.journal.decide(call.id, f.actorId, true, reviewContext(call, approval), new Date());
    let postLeaseFences = 0;
    f.beforeFence(async () => {
      if (f.leases && ++postLeaseFences === 2) f.advanceNow(11000);
    });
    const result = await f.driver.driveOnce(call.id);
    expect(postLeaseFences).toBe(2);
    // Only the ten-second credential handoff expires; the full tool deadline stays live.
    expect(result.intentExpiresAt!.getTime() - (Date.now() + 11000)).toBeGreaterThan(45000);
    expect(result.state).toBe("outcome_unknown");
    expect(f.leases).toBe(1);
    expect((await f.command("stats")).credentialRequests).toBe(0);
    expect((await f.command("stats")).calls).toBe(0);
    expect((await f.driver.acceptToolRequest(f.request)).id).toBe(call.id);
    expect((await f.driver.driveOnce(call.id)).state).toBe("outcome_unknown");
    expect(f.leases).toBe(1);
    expect((await f.command("stats")).credentialRequests).toBe(0);
  } finally {
    await f.cleanup();
  }
}, 20000);
