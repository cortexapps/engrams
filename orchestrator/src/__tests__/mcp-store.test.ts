import { describe, expect, test } from "bun:test";
import { eq, inArray, sql } from "drizzle-orm";
import { checkDb, getDb } from "../db/client.ts";
import { makeIntegrationConnectionStore } from "../db/integration-connections.ts";
import {
  integrationConnection,
  mcpAdmission,
  mcpAdmissionScope,
  mcpApproval,
  mcpCatalog,
  mcpExecution,
  mcpResolution,
  task,
  taskSession,
} from "../db/schema.ts";
import { makeMcpRepository } from "../mcp/repository.ts";
import { makeMcpJournal } from "../mcp/journal.ts";
import { canonicalDigest } from "../mcp/hash.ts";

const reachable = process.env.ORCHESTRATOR_DATABASE_URL ? await checkDb() : false;
const tool = {
  upstreamName: "Write.Record",
  description: "Write a record",
  inputSchema: { type: "object" },
};
const config = {
  endpoint: "https://tools.example.test/mcp",
  zone: "public",
  auth: { kind: "api_key", secretRef: "mcp.service.api-key" },
};
async function fixture() {
  const db = getDb();
  const connections = makeIntegrationConnectionStore(db);
  const row = await connections.create({
    alias: `mcp-${crypto.randomUUID()}`,
    provider: "mcp",
    displayName: "Tools",
    config,
  });
  const taskId = crypto.randomUUID();
  const sessionId = crypto.randomUUID();
  await db.insert(task).values({ id: taskId, type: "chat", createdByUserId: "owner" });
  await db.insert(taskSession).values({ taskId, sessionId });
  const repo = makeMcpRepository(db);
  const now = new Date("2026-01-01T00:00:00Z");
  await repo.setAuthorization(row.id, 1, 1, "account-a", now);
  const live = (await connections.get(row.id))!;
  const draft = await repo.saveDraft(row.id, live.mcp!, [tool], "admin", now);
  const published = await repo.publish(
    row.id,
    draft.id,
    0,
    [{ ...tool, classification: "write", enabled: true }],
    "admin",
    now,
  );
  await repo.setEnabled(row.id, 1, true, now);
  const journal = makeMcpJournal(db);
  const deploymentId = crypto.randomUUID();
  const input = {
    taskId,
    sessionId,
    runId: "run-1",
    toolCallId: "call-1",
    connectionId: row.id,
    catalogId: published.id,
    principalId: "owner",
    bindingHash: canonicalDigest({ binding: 1 }),
    argumentHash: canonicalDigest({ x: 1 }),
    upstreamName: tool.upstreamName,
    configRevision: 1,
    authGeneration: live.mcp!.authGeneration,
    classification: "write" as const,
    deadline: new Date(now.getTime() + 60_000),
  };
  return {
    db,
    connections,
    row,
    repo,
    now,
    draft,
    published,
    journal,
    input,
    deploymentId,
    async cleanup() {
      const executions = await db
        .select({ id: mcpExecution.id })
        .from(mcpExecution)
        .where(eq(mcpExecution.sessionId, sessionId));
      const ids = executions.map((e) => e.id);
      if (ids.length) {
        await db.delete(mcpAdmission).where(inArray(mcpAdmission.executionId, ids));
        await db.delete(mcpApproval).where(inArray(mcpApproval.executionId, ids));
        await db.delete(mcpResolution).where(inArray(mcpResolution.executionId, ids));
        await db.delete(mcpExecution).where(inArray(mcpExecution.id, ids));
      }
      await db.delete(mcpAdmissionScope).where(eq(mcpAdmissionScope.id, deploymentId));
      await db.delete(mcpCatalog).where(eq(mcpCatalog.connectionId, row.id));
      await db.delete(task).where(eq(task.id, taskId));
      await connections.delete(row.id);
    },
  };
}

describe("MCP durable authority", () => {
  test.skipIf(!reachable)("publish CAS has one winner and refuses a stale discovery", async () => {
    const f = await fixture();
    try {
      const outcomes = await Promise.allSettled(
        [1, 2].map(() =>
          f.repo.publish(
            f.row.id,
            f.draft.id,
            1,
            [{ ...tool, classification: "write", enabled: true }],
            "admin",
            f.now,
          ),
        ),
      );
      expect(outcomes.filter((o) => o.status === "fulfilled")).toHaveLength(1);
      await f.repo.updateConfig(
        f.row.id,
        1,
        { alias: f.row.alias, displayName: "Tools", config },
        f.now,
      );
      await expect(
        f.repo.publish(
          f.row.id,
          f.draft.id,
          2,
          [{ ...tool, classification: "write", enabled: true }],
          "admin",
          f.now,
        ),
      ).rejects.toThrow("stale");
      expect(await f.connections.markTested(f.row.id, f.now)).toBeNull();
    } finally {
      await f.cleanup();
    }
  });
  test.skipIf(!reachable)(
    "call digest, approval and dispatch are single-use under concurrency",
    async () => {
      const f = await fixture();
      try {
        const call = await f.journal.create(f.input, { kind: "approval" }, f.now);
        expect((await f.journal.create(f.input, { kind: "approval" }, f.now)).id).toBe(call.id);
        await expect(
          f.journal.create(
            { ...f.input, argumentHash: canonicalDigest({ x: 2 }) },
            { kind: "approval" },
            f.now,
          ),
        ).rejects.toThrow("digest");
        await f.journal.requestApproval(call.id, f.deploymentId, f.now);
        await expect(f.journal.decide(call.id, "intruder", false, true, f.now)).rejects.toThrow(
          "owner",
        );
        await f.journal.decide(call.id, "owner", false, true, f.now);
        await f.journal.admit(call.id, f.deploymentId, f.now);
        const results = await Promise.allSettled(
          [1, 2].map((n) =>
            f.journal.dispatch(
              call.id,
              {
                bootId: "boot-1",
                intentId: `intent-${call.id}-${n}`,
                expiresAt: new Date(f.now.getTime() + 10_000),
              },
              f.now,
            ),
          ),
        );
        expect(results.filter((r) => r.status === "fulfilled")).toHaveLength(1);
        const current = (await f.journal.get(call.id))!;
        expect(current.state).toBe("dispatching");
        await f.journal.finish(call.id, current.version, "outcome_unknown", null, false, f.now);
        const next = await f.journal.create(
          { ...f.input, toolCallId: "call-2" },
          {
            kind: "automation",
            grant: {
              upstreamName: tool.upstreamName,
              automationWrite: true,
              argumentConstraintDocuments: [],
              unrestricted: true,
            },
          },
          f.now,
        );
        await f.journal.admit(next.id, f.deploymentId, f.now);
        await expect(
          f.journal.dispatch(
            next.id,
            {
              bootId: "boot-1",
              intentId: `next-${call.id}`,
              expiresAt: new Date(f.now.getTime() + 10_000),
            },
            f.now,
          ),
        ).rejects.toThrow("unknown");
        await f.journal.resolveUnknown(call.id, "owner", false, "acknowledged", f.now);
        expect(await f.journal.hasUnknown(f.input.sessionId, f.row.id)).toBe(false);
        await expect(
          Promise.resolve(
            f.db.execute(
              sql`UPDATE mcp_execution SET request_digest = 'changed' WHERE id = ${call.id}`,
            ),
          ),
        ).rejects.toThrow();
      } finally {
        await f.cleanup();
      }
    },
  );
  test.skipIf(!reachable)(
    "distributed admission bounds session concurrency and expires pending approvals",
    async () => {
      const f = await fixture();
      try {
        const calls = await Promise.all(
          Array.from({ length: 6 }, (_, n) =>
            f.journal.create(
              { ...f.input, toolCallId: `call-${n}`, classification: "read" },
              { kind: "read" },
              f.now,
            ),
          ),
        );
        const statuses = await Promise.all(
          calls.map((c) => f.journal.admit(c.id, f.deploymentId, f.now)),
        );
        expect(statuses.filter((s) => s === "admitted")).toHaveLength(4);
        expect(statuses.filter((s) => s === "queued")).toHaveLength(2);
        const pending = await f.journal.create(
          { ...f.input, toolCallId: "approval" },
          { kind: "approval" },
          f.now,
        );
        await f.journal.requestApproval(pending.id, f.deploymentId, f.now);
        await f.journal.expire(f.deploymentId, new Date(f.now.getTime() + 601_000));
        expect((await f.journal.get(pending.id))!.state).toBe("canceled");
        expect(await f.journal.pending(f.input.sessionId, "owner", false, f.now)).toHaveLength(0);
      } finally {
        await f.cleanup();
      }
    },
  );
});

describe("MCP authority isolation", () => {
  test.skipIf(!reachable)(
    "rejects cross-connection catalog and unapproved write transitions",
    async () => {
      const first = await fixture();
      const second = await fixture();
      try {
        await expect(
          first.journal.create(first.input, { kind: "read" }, first.now),
        ).rejects.toThrow("write requires");
        await expect(
          first.journal.create(
            first.input,
            {
              kind: "automation",
              grant: {
                upstreamName: tool.upstreamName,
                automationWrite: false,
                argumentConstraintDocuments: [],
                unrestricted: true,
              },
            },
            first.now,
          ),
        ).rejects.toThrow("explicit automation");
        const call = await first.journal.create(
          { ...first.input, catalogId: second.published.id },
          { kind: "approval" },
          first.now,
        );
        await first.journal.requestApproval(call.id, first.deploymentId, first.now);
        await first.journal.decide(call.id, "owner", false, true, first.now);
        await first.journal.admit(call.id, first.deploymentId, first.now);
        await expect(
          first.journal.dispatch(
            call.id,
            {
              bootId: "boot",
              intentId: crypto.randomUUID(),
              expiresAt: new Date(first.now.getTime() + 1000),
            },
            first.now,
          ),
        ).rejects.toThrow("stale");
        await expect(
          Promise.resolve(
            first.db.execute(
              sql`UPDATE mcp_execution SET state = 'succeeded', version = version + 1 WHERE id = ${call.id}`,
            ),
          ),
        ).rejects.toThrow();
        await expect(
          Promise.resolve(
            first.db.execute(
              sql`UPDATE mcp_catalog SET tools = '[]'::jsonb WHERE id = ${first.published.id}`,
            ),
          ),
        ).rejects.toThrow();
        await expect(
          first.journal.pending(first.input.sessionId, "intruder", false, first.now),
        ).resolves.toHaveLength(0);
      } finally {
        await first.cleanup();
        await second.cleanup();
      }
    },
  );
  test.skipIf(!reachable)(
    "pending approvals are owner-scoped and expired approvals cannot dispatch",
    async () => {
      const f = await fixture();
      try {
        const call = await f.journal.create(f.input, { kind: "approval" }, f.now);
        await f.journal.requestApproval(call.id, f.deploymentId, f.now);
        await expect(
          f.journal.pending(f.input.sessionId, "intruder", false, f.now),
        ).rejects.toThrow("owner");
        expect(await f.journal.pending(f.input.sessionId, "owner", false, f.now)).toHaveLength(1);
        await f.journal.decide(call.id, "owner", false, true, f.now);
        await f.journal.admit(call.id, f.deploymentId, f.now);
        const later = new Date(f.now.getTime() + 61_000);
        await expect(
          f.journal.dispatch(
            call.id,
            {
              bootId: "boot",
              intentId: crypto.randomUUID(),
              expiresAt: new Date(later.getTime() + 1000),
            },
            later,
          ),
        ).rejects.toThrow("cannot dispatch");
        await f.journal.expire(f.deploymentId, later);
        expect((await f.journal.get(call.id))!.state).toBe("canceled");
      } finally {
        await f.cleanup();
      }
    },
  );
  test.skipIf(!reachable)(
    "an expired dispatch becomes unknown and accepts one verified late result",
    async () => {
      const f = await fixture();
      try {
        const call = await f.journal.create(
          f.input,
          {
            kind: "automation",
            grant: {
              upstreamName: tool.upstreamName,
              automationWrite: true,
              argumentConstraintDocuments: [],
              unrestricted: true,
            },
          },
          f.now,
        );
        await f.journal.admit(call.id, f.deploymentId, f.now);
        await f.journal.dispatch(
          call.id,
          {
            bootId: "boot",
            intentId: crypto.randomUUID(),
            expiresAt: new Date(f.now.getTime() + 1000),
          },
          f.now,
        );
        await f.journal.expire(f.deploymentId, new Date(f.now.getTime() + 61_000));
        const unknown = (await f.journal.get(call.id))!;
        expect(unknown.state).toBe("outcome_unknown");
        expect(await f.journal.hasUnknown(f.input.sessionId, f.row.id)).toBe(true);
        const late = await f.journal.finish(
          call.id,
          unknown.version,
          "succeeded",
          null,
          true,
          new Date(f.now.getTime() + 62_000),
        );
        expect(late.resultUnavailable).toBe(true);
        expect(await f.journal.hasUnknown(f.input.sessionId, f.row.id)).toBe(false);
        await expect(
          f.journal.finish(call.id, unknown.version, "failed", null, false, f.now),
        ).rejects.toThrow("stale");
      } finally {
        await f.cleanup();
      }
    },
  );
});

test.skipIf(!reachable)(
  "approval has a ten-minute window separate from the tool deadline",
  async () => {
    const f = await fixture();
    try {
      const call = await f.journal.create(
        { ...f.input, deadline: new Date(f.now.getTime() + 690_000) },
        { kind: "approval" },
        f.now,
      );
      const approval = await f.journal.requestApproval(call.id, f.deploymentId, f.now);
      expect(approval.expiresAt.getTime() - f.now.getTime()).toBe(600_000);
      const afterFiveMinutes = new Date(f.now.getTime() + 300_000);
      await f.journal.decide(call.id, "owner", false, true, afterFiveMinutes);
      await f.journal.admit(call.id, f.deploymentId, afterFiveMinutes);
      const dispatch = await f.journal.dispatch(
        call.id,
        {
          bootId: "boot",
          intentId: crypto.randomUUID(),
          expiresAt: new Date(afterFiveMinutes.getTime() + 1000),
        },
        afterFiveMinutes,
      );
      expect(dispatch.state).toBe("dispatching");
      await f.journal.expire(f.deploymentId, new Date(afterFiveMinutes.getTime() + 61_000));
      expect((await f.journal.get(call.id))!.state).toBe("outcome_unknown");
    } finally {
      await f.cleanup();
    }
  },
);

test.skipIf(!reachable)("replicas share connection and deployment admission ceilings", async () => {
  const fixtures: Awaited<ReturnType<typeof fixture>>[] = [];
  const auth = {
    kind: "automation" as const,
    grant: {
      upstreamName: tool.upstreamName,
      automationWrite: true,
      argumentConstraintDocuments: [],
      unrestricted: true,
    },
  };
  try {
    for (let i = 0; i < 9; i++) fixtures.push(await fixture());
    const first = fixtures[0]!;
    const allCalls = await Promise.all(
      fixtures.flatMap((f) =>
        Array.from({ length: 4 }, (_, i) =>
          f.journal.create({ ...f.input, toolCallId: `limit-${i}` }, auth, f.now),
        ),
      ),
    );
    const states = await Promise.all(
      allCalls.map((c) => makeMcpJournal(first.db).admit(c.id, first.deploymentId, first.now)),
    );
    expect(states.filter((state) => state === "admitted")).toHaveLength(32);
    expect(states.filter((state) => state === "queued")).toHaveLength(4);
    // Clear the deployment test through normal task retention.
    for (const f of fixtures) await f.db.delete(task).where(eq(task.id, f.input.taskId));
    await first.db
      .insert(task)
      .values({ id: first.input.taskId, type: "chat", createdByUserId: "owner" });
    const sessions = [first.input.sessionId, crypto.randomUUID(), crypto.randomUUID()];
    for (const sessionId of sessions)
      await first.db.insert(taskSession).values({ taskId: first.input.taskId, sessionId });
    const connectionCalls = await Promise.all(
      sessions.flatMap((sessionId) =>
        Array.from({ length: 4 }, (_, i) =>
          first.journal.create(
            { ...first.input, sessionId, toolCallId: `connection-${i}` },
            auth,
            first.now,
          ),
        ),
      ),
    );
    const connectionStates = await Promise.all(
      connectionCalls.map((c) =>
        makeMcpJournal(first.db).admit(c.id, first.deploymentId, first.now),
      ),
    );
    expect(connectionStates.filter((state) => state === "admitted")).toHaveLength(8);
    expect(connectionStates.filter((state) => state === "queued")).toHaveLength(4);
  } finally {
    // Session-owned call payloads and child rows use the existing task retention path.
    for (const f of fixtures) await f.db.delete(task).where(eq(task.id, f.input.taskId));
    for (const f of fixtures) await f.cleanup();
  }
});

test.skipIf(!reachable)(
  "credential completion cannot authorize a connection edited during the flow",
  async () => {
    const f = await fixture();
    try {
      await f.repo.updateConfig(
        f.row.id,
        1,
        { alias: f.row.alias, displayName: "Edited", config },
        f.now,
      );
      await expect(
        f.repo.setAuthorization(f.row.id, 1, f.input.authGeneration, "account-b", f.now),
      ).rejects.toThrow("stale");
    } finally {
      await f.cleanup();
    }
  },
);

test.skipIf(!reachable)(
  "recovers calls after a crash before approval or admission is recorded",
  async () => {
    const f = await fixture();
    try {
      const waiting = await f.journal.create(f.input, { kind: "approval" }, f.now);
      const ready = await f.journal.create(
        { ...f.input, toolCallId: "before-admission" },
        {
          kind: "automation",
          grant: {
            upstreamName: tool.upstreamName,
            automationWrite: true,
            argumentConstraintDocuments: [],
            unrestricted: true,
          },
        },
        f.now,
      );
      const count = await f.journal.expireUnadmitted(new Date(f.now.getTime() + 61_000));
      expect(count).toBeGreaterThanOrEqual(2);
      expect((await f.journal.get(waiting.id))!.state).toBe("canceled");
      expect((await f.journal.get(ready.id))!.state).toBe("canceled");
    } finally {
      await f.cleanup();
    }
  },
);

test.skipIf(!reachable)(
  "admission retries preserve queue expiry and promotion starts a tool window",
  async () => {
    const f = await fixture();
    const authorization = {
      kind: "automation" as const,
      grant: {
        upstreamName: tool.upstreamName,
        automationWrite: true,
        argumentConstraintDocuments: [],
        unrestricted: true,
      },
    };
    const at = (seconds: number) => new Date(f.now.getTime() + seconds * 1000);
    try {
      const calls = await Promise.all(
        Array.from({ length: 6 }, (_, i) =>
          f.journal.create(
            { ...f.input, toolCallId: `fixed-queue-${i}`, deadline: at(300) },
            authorization,
            f.now,
          ),
        ),
      );
      for (const call of calls.slice(0, 4))
        expect(await f.journal.admit(call.id, f.deploymentId, f.now)).toBe("admitted");
      for (const call of calls.slice(4)) {
        expect(await f.journal.admit(call.id, f.deploymentId, f.now)).toBe("queued");
        expect(await f.journal.admit(call.id, f.deploymentId, at(20))).toBe("queued");
        const [lease] = await f.db
          .select()
          .from(mcpAdmission)
          .where(eq(mcpAdmission.executionId, call.id));
        expect(lease!.expiresAt).toEqual(at(30));
      }
      await f.journal.cancel(calls[0]!.id, "owner", false, at(25));
      expect(await f.journal.admit(calls[4]!.id, f.deploymentId, at(25))).toBe("admitted");
      const [promoted] = await f.db
        .select()
        .from(mcpAdmission)
        .where(eq(mcpAdmission.executionId, calls[4]!.id));
      expect(promoted!.expiresAt).toEqual(at(85));
      await f.journal.expire(f.deploymentId, at(31));
      expect((await f.journal.get(calls[5]!.id))!.state).toBe("canceled");
      expect((await f.journal.get(calls[4]!.id))!.state).toBe("authorized");
    } finally {
      await f.cleanup();
    }
  },
);
