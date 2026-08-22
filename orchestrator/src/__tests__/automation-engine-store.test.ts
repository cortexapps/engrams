/**
 * Live-PG tests for the engine stores (ADR 0119): version pinning, the
 * concurrency claim under contention, queue promotion, per-attempt step
 * rows, and session bindings. Gated on ORCHESTRATOR_DATABASE_URL + a
 * reachability probe, per the db.test.ts convention; rows use unique ids so
 * the suite is parallel-safe against the shared migrated database.
 */

import { afterAll, describe, expect, test } from "bun:test";
import { eq, inArray, sql } from "drizzle-orm";

import { checkDb, getDb } from "../db/client.ts";
import {
  makeAutomationEngineStore,
  makeAutomationStore,
  resolveAutomationInputs,
} from "../db/automations.ts";
import {
  automation as automationTable,
  automationRun as automationRunTable,
} from "../db/schema.ts";
import type { AutomationRunTrigger } from "../db/schema.ts";

const DB_URL = process.env["ORCHESTRATOR_DATABASE_URL"];
const dbReachable = DB_URL ? await checkDb() : false;

const UNIQ = `${Date.now()}-${Math.floor(Math.random() * 1e6)}`;
const AUTO_ID = `eng-store-auto-${UNIQ}`;
const createdAutomationIds: string[] = [];

const TRIGGER: AutomationRunTrigger = {
  source: "manual",
  receivedAt: "2026-08-21T10:00:00Z",
};

async function seedAutomation(id: string): Promise<void> {
  const db = getDb();
  await db.insert(automationTable).values({
    id,
    name: `Engine store test ${id}`,
    description: "",
    enabled: true,
    currentVersion: 1,
  });
  createdAutomationIds.push(id);
  // Insert version 1 directly so this seed is independent of the legacy
  // trigger/action facade.
  await db.execute(sql`
    insert into automation_version (automation_id, version, trigger, blocks, inputs_schema, settings)
    values (${id}, 1, ${JSON.stringify({ kind: "manual" })}::jsonb,
            ${JSON.stringify([
              {
                id: "create_session",
                type: "create_session",
                config: { profileId: "p1", promptTemplate: "go", includeEventContext: false },
              },
            ])}::jsonb,
            '[]'::jsonb,
            ${JSON.stringify({ endSessionsOnFinish: false })}::jsonb)
  `);
}

afterAll(async () => {
  if (!dbReachable || createdAutomationIds.length === 0) return;
  const db = getDb();
  // Runs, versions, sessions, steps, claims cascade from the automation rows.
  await db.delete(automationTable).where(inArray(automationTable.id, createdAutomationIds));
});

async function seedIntegrationAutomation(
  id: string,
  trigger: Record<string, unknown>,
): Promise<void> {
  const db = getDb();
  await db.insert(automationTable).values({
    id,
    name: `Integration trigger test ${id}`,
    description: "",
    enabled: true,
    currentVersion: 1,
  });
  createdAutomationIds.push(id);
  await db.execute(sql`
    insert into automation_version (automation_id, version, trigger, blocks, inputs_schema, settings)
    values (${id}, 1, ${JSON.stringify(trigger)}::jsonb,
            ${JSON.stringify([
              {
                id: "create_session",
                type: "create_session",
                config: { profileId: "p1", promptTemplate: "go", includeEventContext: false },
              },
            ])}::jsonb,
            '[]'::jsonb,
            ${JSON.stringify({ endSessionsOnFinish: false })}::jsonb)
  `);
}

describe("integration trigger store (live PG)", () => {
  test.skipIf(!dbReachable)(
    "listEnabledForIntegrationTrigger filters by kind, provider, and connection",
    async () => {
      const connectionId = `conn-${UNIQ}`;
      const hit = `${AUTO_ID}-int-hit`;
      const otherConn = `${AUTO_ID}-int-conn`;
      const otherProvider = `${AUTO_ID}-int-prov`;
      const cron = `${AUTO_ID}-int-cron`;
      await seedIntegrationAutomation(hit, {
        kind: "integration",
        provider: "github",
        connectionId,
        eventKeys: ["pull_request.opened"],
      });
      await seedIntegrationAutomation(otherConn, {
        kind: "integration",
        provider: "github",
        connectionId: `${connectionId}-other`,
        eventKeys: ["pull_request.opened"],
      });
      await seedIntegrationAutomation(otherProvider, {
        kind: "integration",
        provider: "slack",
        connectionId,
        eventKeys: ["app_mention"],
      });
      await seedIntegrationAutomation(cron, {
        kind: "cron",
        schedule: "0 9 * * 1-5",
        timezone: "UTC",
      });

      const store = makeAutomationStore(getDb());
      const targets = await store.listEnabledForIntegrationTrigger("github", connectionId);
      const ids = targets.map((t) => t.automation.id);
      expect(ids).toEqual([hit]);
      expect(targets[0]!.definition.trigger).toMatchObject({
        kind: "integration",
        provider: "github",
        connectionId,
      });
    },
  );
});

describe("automation engine store (live PG)", () => {
  test.skipIf(!dbReachable)("loadSnapshot pins the run's version and inputs", async () => {
    const id = `${AUTO_ID}-snapshot`;
    await seedAutomation(id);
    const store = makeAutomationStore(getDb());
    const runId = `autorun:${id}:manual:1`;
    await store.insertRun({
      id: runId,
      automationId: id,
      version: 1,
      trigger: TRIGGER,
      deliveryKey: "manual:1",
      concurrencyKey: null,
      scheduledFor: null,
    });

    const engine = makeAutomationEngineStore();
    const snapshot = await engine.loadSnapshot(runId);
    expect(snapshot.automationId).toBe(id);
    expect(snapshot.version).toBe(1);
    expect(snapshot.definition.blocks).toHaveLength(1);
    expect(snapshot.trigger).toMatchObject({ kind: "manual", deliveryKey: "manual:1" });

    await engine.markRunning(runId, Date.parse("2026-08-21T10:00:05Z"));
    const run = await engine.getRun(runId);
    expect(run?.status).toBe("running");
    expect(run?.startedAt?.toISOString()).toBe("2026-08-21T10:00:05.000Z");
  });

  test.skipIf(!dbReachable)(
    "loadSnapshot merges block overrides over the pinned version; dry_run rides the run row",
    async () => {
      const id = `${AUTO_ID}-overrides`;
      await seedAutomation(id);
      const db = getDb();
      // Mark promptTemplate tunable on the seeded version, then store an override.
      await db.execute(sql`
        update automation_version
        set blocks = ${JSON.stringify([
          {
            id: "create_session",
            type: "create_session",
            config: { profileId: "p1", promptTemplate: "go", includeEventContext: false },
            tunable: ["promptTemplate"],
          },
        ])}::jsonb
        where automation_id = ${id} and version = 1
      `);
      const store = makeAutomationStore(db);
      await store.setBlockOverrides(id, { create_session: { promptTemplate: "overridden" } });
      const runId = `autorun:${id}:dryrun:1`;
      await store.insertRun({
        id: runId,
        automationId: id,
        version: 1,
        trigger: TRIGGER,
        deliveryKey: "dryrun:1",
        concurrencyKey: null,
        scheduledFor: null,
        dryRun: true,
      });

      const snapshot = await makeAutomationEngineStore().loadSnapshot(runId);
      expect(snapshot.definition.blocks[0]!.config["promptTemplate"]).toBe("overridden");
      expect(snapshot.dryRun).toBe(true);
      // The stored version itself is untouched.
      const version = await store.getVersion(id, 1);
      expect(version?.blocks[0]!.config["promptTemplate"]).toBe("go");
    },
  );

  test.skipIf(!dbReachable)("latestRuns and runCounts7d summarize per automation", async () => {
    const id = `${AUTO_ID}-summary`;
    await seedAutomation(id);
    const store = makeAutomationStore(getDb());
    for (const [n, status] of [["1", "completed"], ["2", "failed"], ["3", "filtered"]] as const) {
      await store.insertRun({
        id: `autorun:${id}:manual:${n}`,
        automationId: id,
        version: 1,
        trigger: TRIGGER,
        deliveryKey: `manual:${n}`,
        concurrencyKey: null,
        scheduledFor: null,
        status,
      });
    }
    const latest = await store.latestRuns([id]);
    expect(latest.get(id)).toBeDefined();
    const counts = await store.runCounts7d([id], new Date());
    const days = counts.get(id) ?? [];
    expect(days.length).toBeGreaterThanOrEqual(1);
    const total = days.reduce((acc, d) => acc + d.completed + d.failed + d.filtered + d.other, 0);
    expect(total).toBe(3);
    expect(days.reduce((acc, d) => acc + d.completed, 0)).toBe(1);
    expect(days.reduce((acc, d) => acc + d.failed, 0)).toBe(1);
    expect(days.reduce((acc, d) => acc + d.filtered, 0)).toBe(1);
  });

  test.skipIf(!dbReachable)("claim race: one winner, holder visible to the loser", async () => {
    const id = `${AUTO_ID}-race`;
    await seedAutomation(id);
    const store = makeAutomationStore(getDb());
    const [a, b] = await Promise.all([
      store.claimConcurrency(id, "pr-7", "run-a"),
      store.claimConcurrency(id, "pr-7", "run-b"),
    ]);
    const winners = [a, b].filter((r) => r.claimed);
    const losers = [a, b].filter((r) => !r.claimed);
    expect(winners).toHaveLength(1);
    expect(losers).toHaveLength(1);
    const loser = losers[0]!;
    if (!loser.claimed) {
      expect(["run-a", "run-b"]).toContain(loser.holderRunId);
    }
    // CAS moves the claim only from the current holder.
    const holder = winners[0] === a ? "run-a" : "run-b";
    expect(await store.casConcurrency(id, "pr-7", "run-neither", "run-c")).toBe(false);
    expect(await store.casConcurrency(id, "pr-7", holder, "run-c")).toBe(true);
  });

  test.skipIf(!dbReachable)(
    "releaseConcurrency promotes the oldest pending queued run exactly once",
    async () => {
      const id = `${AUTO_ID}-queue`;
      await seedAutomation(id);
      const store = makeAutomationStore(getDb());
      const engine = makeAutomationEngineStore();
      const holder = `autorun:${id}:manual:h`;
      await store.insertRun({
        id: holder,
        automationId: id,
        version: 1,
        trigger: TRIGGER,
        deliveryKey: "manual:h",
        concurrencyKey: "q",
        scheduledFor: null,
      });
      expect(await store.claimConcurrency(id, "q", holder)).toEqual({ claimed: true });
      // Two queued successors; created_at ordering decides.
      const first = `autorun:${id}:manual:q1`;
      const second = `autorun:${id}:manual:q2`;
      await store.insertRun({
        id: first,
        automationId: id,
        version: 1,
        trigger: TRIGGER,
        deliveryKey: "manual:q1",
        concurrencyKey: "q",
        scheduledFor: null,
      });
      await store.insertRun({
        id: second,
        automationId: id,
        version: 1,
        trigger: TRIGGER,
        deliveryKey: "manual:q2",
        concurrencyKey: "q",
        scheduledFor: null,
      });

      // Production order (interpreter finalize): terminal status first, then
      // release+promote, so a released run can never be re-promoted.
      await engine.finalizeRun(holder, "completed");
      const promoted = await engine.releaseConcurrency(holder);
      expect(promoted).toBe(first);
      // The claim now names the successor; releasing the old holder again is
      // a no-op (the supersede-skips-release property).
      expect(await engine.releaseConcurrency(holder)).toBeNull();
      await engine.finalizeRun(first, "completed");
      expect(await engine.releaseConcurrency(first)).toBe(second);
    },
  );

  test.skipIf(!dbReachable)("recordStep keeps one row per attempt", async () => {
    const id = `${AUTO_ID}-steps`;
    await seedAutomation(id);
    const store = makeAutomationStore(getDb());
    const engine = makeAutomationEngineStore();
    const runId = `autorun:${id}:manual:s`;
    await store.insertRun({
      id: runId,
      automationId: id,
      version: 1,
      trigger: TRIGGER,
      deliveryKey: "manual:s",
      concurrencyKey: null,
      scheduledFor: null,
    });

    await engine.recordStep(runId, "launch", 0, { status: "running", inputs: { a: 1 } });
    await engine.recordStep(runId, "launch", 0, { status: "failed", error: "boom" });
    await engine.recordStep(runId, "launch", 1, { status: "succeeded", outputs: { ok: true } });

    const db = getDb();
    const rows = await db.execute(sql`
      select block_id, attempt, status, error, outputs from automation_step_run
      where run_id = ${runId} order by attempt
    `);
    expect(rows.rows).toHaveLength(2);
    expect(rows.rows[0]).toMatchObject({ attempt: 0, status: "failed", error: "boom" });
    expect(rows.rows[1]).toMatchObject({ attempt: 1, status: "succeeded" });

    await engine.finalizeRun(runId, "failed", "block launch failed");
    const run = await engine.getRun(runId);
    expect(run).toMatchObject({ status: "failed", error: "block launch failed" });
    expect(run?.endedAt).not.toBeNull();
  });

  test.skipIf(!dbReachable)("session bindings round-trip", async () => {
    const id = `${AUTO_ID}-sessions`;
    await seedAutomation(id);
    const store = makeAutomationStore(getDb());
    const engine = makeAutomationEngineStore();
    const runId = `autorun:${id}:manual:b`;
    await store.insertRun({
      id: runId,
      automationId: id,
      version: 1,
      trigger: TRIGGER,
      deliveryKey: "manual:b",
      concurrencyKey: null,
      scheduledFor: null,
    });
    const sessionId = `sess-${UNIQ}`;
    await engine.recordSessionBinding({
      sessionId,
      runId,
      blockId: "launch",
      role: "primary",
      keep: false,
    });
    expect(await engine.findSessionBinding(sessionId)).toEqual({
      runId,
      blockId: "launch",
      role: "primary",
    });
    expect(await engine.listRunSessions(runId)).toEqual([{ sessionId, keep: false }]);
  });

  test.skipIf(!dbReachable)("delivery-key uniqueness dedupes a redelivery", async () => {
    const id = `${AUTO_ID}-dedupe`;
    await seedAutomation(id);
    const store = makeAutomationStore(getDb());
    const runId = `autorun:${id}:webhook:d-1`;
    const insert = () =>
      store.insertRun({
        id: runId,
        automationId: id,
        version: 1,
        trigger: { source: "webhook", deliveryId: "d-1", receivedAt: TRIGGER.receivedAt },
        deliveryKey: "webhook:d-1",
        concurrencyKey: null,
        scheduledFor: null,
      });
    const first = await insert();
    const second = await insert();
    expect(second.id).toBe(first.id);
    const db = getDb();
    const rows = await db
      .select({ id: automationRunTable.id })
      .from(automationRunTable)
      .where(eq(automationRunTable.automationId, id));
    expect(rows).toHaveLength(1);
  });
});

describe("resolveAutomationInputs", () => {
  test("defaults overlay stored values", () => {
    const resolved = resolveAutomationInputs(
      [
        { key: "mention", label: "Mention", type: "string", default: "@engrams" },
        { key: "max", label: "Max", type: "number", default: 5 },
      ],
      { max: 9 },
    );
    expect(resolved).toEqual({ mention: "@engrams", max: 9 });
  });
});
