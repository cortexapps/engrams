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
  automationSession as automationSessionTable,
} from "../db/schema.ts";
import type { AutomationRunTrigger } from "../db/schema.ts";
import { interpretAutomation } from "../automations/engine/interpreter.ts";
import type { EngineDeps } from "../automations/engine/deps.ts";

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
    "a run on inputs the pinned schema rejects ends failed AND releases its concurrency claim (phase 4.3b)",
    async () => {
      // A built-in version bump that tightens a rule after the org saved its
      // values: the run must end `failed` with the routed reason, terminal
      // and visible, never start on inputs the schema rejects — and it must
      // still go through finalize, or a queue|join|skip key stays held by
      // the dead run forever (#1350 review finding).
      const id = `${AUTO_ID}-bad-inputs`;
      await seedAutomation(id);
      const db = getDb();
      await db.execute(sql`
        update automation_version
        set inputs_schema = ${JSON.stringify([
          { key: "limit", label: "Limit", type: "number", required: true },
        ])}::jsonb
        where automation_id = ${id} and version = 1
      `);
      await db.execute(sql`
        update automation set inputs = ${JSON.stringify({ limit: "ten" })}::jsonb where id = ${id}
      `);
      const store = makeAutomationStore(db);
      const runId = `autorun:${id}:manual:1`;
      const queued = `autorun:${id}:manual:2`;
      for (const [rid, key] of [
        [runId, "manual:1"],
        [queued, "manual:2"],
      ] as const) {
        await store.insertRun({
          id: rid,
          automationId: id,
          version: 1,
          trigger: TRIGGER,
          deliveryKey: key,
          concurrencyKey: "k",
          scheduledFor: null,
        });
      }
      expect(await store.claimConcurrency(id, "k", runId)).toEqual({ claimed: true });

      const engine = makeAutomationEngineStore();
      await expect(engine.loadSnapshot(runId)).rejects.toThrow(/inputs.limit: must be a number/);

      const promoted: string[] = [];
      const deps: EngineDeps = {
        step: (fn) => fn(),
        recv: async () => null,
        store: engine,
        sessions: {
          createSession: () => Promise.reject(new Error("unused")),
          setSessionRelay: () => Promise.reject(new Error("unused")),
          sendPrompt: () => Promise.reject(new Error("unused")),
          endSession: () => Promise.reject(new Error("unused")),
          exec: () => Promise.reject(new Error("unused")),
          writeFiles: () => Promise.reject(new Error("unused")),
      getSession: () => Promise.resolve({ found: false as const }),
        },
        clock: { nowMs: () => 0 },
        startQueuedRun: async (rid) => {
          promoted.push(rid);
        },
      };
      const result = await interpretAutomation({ runId, automationId: id }, deps);
      expect(result.status).toBe("failed");
      expect(result.error).toContain("inputs.limit: must be a number");

      const run = await engine.getRun(runId);
      expect(run?.status).toBe("failed");
      expect(run?.error).toContain("inputs.limit: must be a number");
      expect(run?.endedAt).not.toBeNull();
      // The claim moved to the queued successor and it was started.
      expect(promoted).toEqual([queued]);
      expect(await store.claimConcurrency(id, "k", "autorun:x")).toEqual({
        claimed: false,
        holderRunId: queued,
      });
    },
  );

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
    // The peek reads the holder without claiming (a continue-only delivery).
    expect(await store.getConcurrencyHolder(id, "pr-7")).toBe(holder);
    expect(await store.getConcurrencyHolder(id, "pr-8")).toBeNull();
    expect(await store.casConcurrency(id, "pr-7", "run-neither", "run-c")).toBe(false);
    expect(await store.casConcurrency(id, "pr-7", holder, "run-c")).toBe(true);
    expect(await store.getConcurrencyHolder(id, "pr-7")).toBe("run-c");
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
      relay: false,
    });
    expect(await engine.listRunSessions(runId)).toEqual([{ sessionId, keep: false }]);
    // Contract 3: the relay block flips curated-event forwarding on.
    await engine.setSessionRelay(sessionId, true);
    expect((await engine.findSessionBinding(sessionId))?.relay).toBe(true);
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

describe("session adoption (live PG, ADR 0119 D11)", () => {
  test.skipIf(!dbReachable)(
    "adoptSession transfers only terminal-run bindings in the same automation",
    async () => {
      const db = getDb();
      const autoId = `${AUTO_ID}-adopt`;
      const otherAutoId = `${AUTO_ID}-adopt-other`;
      await seedAutomation(autoId);
      await seedAutomation(otherAutoId);
      const store = makeAutomationEngineStore();

      const doneRun = `autorun:${autoId}:done`;
      const liveRun = `autorun:${autoId}:live`;
      const meRun = `autorun:${autoId}:me`;
      const foreignRun = `autorun:${otherAutoId}:done`;
      await db.insert(automationRunTable).values([
        { id: doneRun, automationId: autoId, trigger: TRIGGER, status: "completed" },
        { id: liveRun, automationId: autoId, trigger: TRIGGER, status: "waiting" },
        { id: meRun, automationId: autoId, trigger: TRIGGER, status: "running" },
        { id: foreignRun, automationId: otherAutoId, trigger: TRIGGER, status: "completed" },
      ]);
      const kept = `sess-${UNIQ}-kept`;
      const busy = `sess-${UNIQ}-busy`;
      const foreign = `sess-${UNIQ}-foreign`;
      await db.insert(automationSessionTable).values([
        { sessionId: kept, runId: doneRun, blockId: "launch", keep: true },
        { sessionId: busy, runId: liveRun, blockId: "launch", keep: true },
        { sessionId: foreign, runId: foreignRun, blockId: "launch", keep: true },
      ]);

      // Terminal owner in the same automation: transfers.
      expect(await store.adoptSession({ runId: meRun, automationId: autoId, sessionId: kept })).toBe(
        "adopted",
      );
      expect(await store.getSessionBinding(kept)).toMatchObject({
        automationId: autoId,
        runId: meRun,
        ownerTerminal: false,
      });
      // A replayed step (crash before checkpoint) is already_ours, not a
      // refusal.
      expect(await store.adoptSession({ runId: meRun, automationId: autoId, sessionId: kept })).toBe(
        "already_ours",
      );

      // A live owner keeps exclusive routing.
      expect(await store.adoptSession({ runId: meRun, automationId: autoId, sessionId: busy })).toBe(
        "owner_live",
      );
      expect((await store.getSessionBinding(busy))?.runId).toBe(liveRun);

      // Another automation's binding, and no binding at all, are foreign.
      expect(
        await store.adoptSession({ runId: meRun, automationId: autoId, sessionId: foreign }),
      ).toBe("foreign");
      expect(
        await store.adoptSession({ runId: meRun, automationId: autoId, sessionId: "no-such" }),
      ).toBe("foreign");
      expect(await store.getSessionBinding("no-such")).toBeNull();
    },
  );
});
