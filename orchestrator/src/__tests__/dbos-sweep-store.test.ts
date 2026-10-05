import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { DBOS } from "@dbos-inc/dbos-sdk";
import { sql } from "drizzle-orm";

import { checkDb, getDb } from "../db/client.ts";
import { makeAutomationStore } from "../db/automations.ts";
import { makeOrgSettingStore, RETENTION_KEY } from "../db/org-settings.ts";
import { makeRetentionStore } from "../db/retention.ts";
import { automation as automationTable } from "../db/schema.ts";

const DB_URL = process.env["ORCHESTRATOR_DATABASE_URL"];
const dbReachable = DB_URL ? await checkDb() : false;
const runId = `sweeptest-${Date.now()}`;

async function sweepModule() {
  return import("../db/dbos-sweep.ts");
}

describe("DBOS sweep schema", () => {
  test("exports the three sweep tables", async () => {
    const schema = await import("../db/schema.ts");
    expect(schema.dbosVersionHeartbeats).toBeDefined();
    expect(schema.dbosSweepLedger).toBeDefined();
    expect(schema.dbosSweepLease).toBeDefined();
  });
});

describe("in-memory heartbeat store", () => {
  test("beats by app/pod and reports only versions inside the strict grace window", async () => {
    const { makeInMemoryHeartbeatStore } = await sweepModule();
    let nowMs = 1_000;
    const store = makeInMemoryHeartbeatStore(() => new Date(nowMs));

    await store.beat("v-old", "pod-a");
    nowMs = 1_050;
    await store.beat("v-live", "pod-a");
    nowMs = 1_075;
    await store.beat("v-live", "pod-b");
    nowMs = 1_100;

    expect(await store.liveVersions(50)).toEqual(["v-live"]);

    await store.beat("v-old", "pod-a");
    expect(await store.liveVersions(50)).toEqual(["v-live", "v-old"]);
    // The abandonment clock: ms since the freshest beat per version,
    // regardless of grace.
    expect(await store.abandonedMsByVersion()).toEqual(
      new Map([
        ["v-live", 25],
        ["v-old", 0],
      ]),
    );
  });

  test("livePods lists the pods with a beat inside the grace window, on any version", async () => {
    const { makeInMemoryHeartbeatStore } = await sweepModule();
    let nowMs = 1_000;
    const store = makeInMemoryHeartbeatStore(() => new Date(nowMs));
    await store.beat("v-old", "pod-a");
    nowMs = 1_060;
    await store.beat("v-live", "pod-b");
    nowMs = 1_100;
    expect(await store.livePods(50)).toEqual(["pod-b"]);
    await store.beat("v-old", "pod-a");
    expect(await store.livePods(50)).toEqual(["pod-a", "pod-b"]);
  });

  test("prune drops rows past retention and reports the count", async () => {
    const { makeInMemoryHeartbeatStore } = await sweepModule();
    let nowMs = 10_000;
    const store = makeInMemoryHeartbeatStore(() => new Date(nowMs));
    await store.beat("v-old", "pod-a");
    nowMs = 20_000;
    await store.beat("v-new", "pod-a");
    nowMs = 21_000;

    expect(await store.prune(5_000)).toBe(1);
    // The un-pruned old row would still satisfy this wide grace window.
    expect(await store.liveVersions(20_000)).toEqual(["v-new"]);
  });
});

describe("in-memory sweep lease store", () => {
  test("acquires once, steals only after expiry, and releases only for the owner", async () => {
    const { makeInMemorySweepLeaseStore } = await sweepModule();
    let nowMs = 10_000;
    const store = makeInMemorySweepLeaseStore(() => new Date(nowMs));

    expect(await store.tryAcquire("pod-a", 100)).toBe(true);
    expect(await store.tryAcquire("pod-b", 100)).toBe(false);
    nowMs = 10_100;
    expect(await store.tryAcquire("pod-b", 100)).toBe(false);
    nowMs = 10_101;
    expect(await store.tryAcquire("pod-b", 100)).toBe(true);

    await store.release("pod-a");
    expect(await store.tryAcquire("pod-c", 100)).toBe(false);
    await store.release("pod-b");
    expect(await store.tryAcquire("pod-c", 100)).toBe(true);
  });
});

describe("in-memory sweep ledger store", () => {
  test("records sweep history, flags, cleanup, alerts, and watermarks", async () => {
    const { makeInMemorySweepLedgerStore } = await sweepModule();
    let nowMs = 20_000;
    const store = makeInMemorySweepLedgerStore(() => new Date(nowMs));

    const first = await store.recordSweep("wf-1", "WorkflowOne");
    expect(first).toMatchObject({
      workflowUuid: "wf-1",
      workflowName: "WorkflowOne",
      sweepCount: 1,
      suppressed: false,
      alertedAt: null,
      terminalAlertedAt: null,
    });
    expect(first.firstSweptAt?.getTime()).toBe(20_000);
    expect(first.lastSweptAt?.getTime()).toBe(20_000);

    nowMs = 21_000;
    const second = await store.recordSweep("wf-1", "WorkflowOneRenamed");
    expect(second.sweepCount).toBe(2);
    expect(second.workflowName).toBe("WorkflowOneRenamed");
    expect(second.firstSweptAt?.getTime()).toBe(20_000);
    expect(second.lastSweptAt?.getTime()).toBe(21_000);

    await store.setSuppressed("wf-1", true);
    nowMs = 23_000;
    await store.markAlerted("wf-1", "WorkflowOne");
    expect(await store.get("wf-1")).toMatchObject({
      suppressed: true,
    });
    expect((await store.get("wf-1"))?.alertedAt?.getTime()).toBe(23_000);
    nowMs = 24_000;
    await store.markTerminalAlerted("wf-1", "WorkflowOne");
    expect((await store.get("wf-1"))?.terminalAlertedAt?.getTime()).toBe(
      24_000,
    );
    // The two alert streams keep distinct dedup keys.
    expect((await store.get("wf-1"))?.alertedAt?.getTime()).toBe(23_000);
    expect(await store.get("missing")).toBeNull();
  });
});

describe("in-memory DBOS status store", () => {
  test("lists dead-version work in age order with a batch cap", async () => {
    const { makeInMemoryDbosStatusStore } = await sweepModule();
    const store = makeInMemoryDbosStatusStore([
      {
        workflowUuid: "pending-old",
        name: "WorkflowOne",
        status: "PENDING",
        applicationVersion: "dead-a",
        createdAtEpochMs: 100,
        updatedAtEpochMs: 100,
        recoveryAttempts: 2,
      },
      {
        workflowUuid: "enqueued-next",
        name: "WorkflowOne",
        status: "ENQUEUED",
        applicationVersion: "dead-b",
        createdAtEpochMs: 200,
        updatedAtEpochMs: 200,
        recoveryAttempts: 0,
      },
      {
        workflowUuid: "pending-same-time",
        name: "WorkflowOne",
        status: "PENDING",
        applicationVersion: "dead-c",
        createdAtEpochMs: 100,
        updatedAtEpochMs: 100,
        recoveryAttempts: 0,
      },
      {
        workflowUuid: "pending-live",
        name: "WorkflowOne",
        status: "PENDING",
        applicationVersion: "live",
        createdAtEpochMs: 50,
        updatedAtEpochMs: 50,
        recoveryAttempts: 0,
      },
      {
        workflowUuid: "pending-null",
        name: "WorkflowOne",
        status: "PENDING",
        applicationVersion: null,
        createdAtEpochMs: 25,
        updatedAtEpochMs: 25,
        recoveryAttempts: 0,
      },
      {
        workflowUuid: "terminal",
        name: "WorkflowOne",
        status: "SUCCESS",
        applicationVersion: "dead-a",
        createdAtEpochMs: 10,
        updatedAtEpochMs: 10,
        recoveryAttempts: 0,
      },
    ]);

    expect(await store.listNonTerminalOnVersionsNotIn(["live"], 1)).toEqual([
      {
        workflowUuid: "pending-old",
        name: "WorkflowOne",
        status: "PENDING",
        applicationVersion: "dead-a",
        createdAtEpochMs: 100,
        recoveryAttempts: 2,
      },
    ]);
    expect(
      (await store.listNonTerminalOnVersionsNotIn([], 10)).map((row) => row.workflowUuid),
    ).toEqual([
      "pending-live",
      "pending-old",
      "pending-same-time",
      "enqueued-next",
    ]);
    expect(
      (
        await store.listNonTerminalOnVersionsNotIn([], 10, {
          createdAtEpochMs: 100,
          workflowUuid: "pending-old",
        })
      ).map((row) => row.workflowUuid),
    ).toEqual(["pending-same-time", "enqueued-next"]);
  });

  test("adopts only PENDING and clears only the version on ENQUEUED", async () => {
    const {
      makeInMemoryDbosStatusStore,
      makeInMemorySweepLedgerStore,
    } = await sweepModule();
    let nowMs = 5_000;
    const ledger = makeInMemorySweepLedgerStore(
      () => new Date(nowMs),
    );
    const store = makeInMemoryDbosStatusStore(
      [
        {
          workflowUuid: "pending",
          name: "WorkflowOne",
          status: "PENDING",
          applicationVersion: "dead",
          createdAtEpochMs: 100,
          updatedAtEpochMs: 100,
          recoveryAttempts: 7,
          queueName: "old-queue",
          workflowDeadlineEpochMs: 600,
          deduplicationId: "dedup",
          startedAtEpochMs: 400,
        },
        {
          workflowUuid: "enqueued",
          name: "WorkflowOne",
          status: "ENQUEUED",
          applicationVersion: "dead",
          createdAtEpochMs: 200,
          updatedAtEpochMs: 200,
          recoveryAttempts: 3,
          queueName: "custom",
          workflowDeadlineEpochMs: 700,
          deduplicationId: "keep",
          startedAtEpochMs: 450,
        },
        {
          workflowUuid: "terminal",
          name: "WorkflowOne",
          status: "ERROR",
          applicationVersion: "dead",
          createdAtEpochMs: 300,
          updatedAtEpochMs: 300,
          recoveryAttempts: 9,
        },
      ],
      () => new Date(nowMs),
      {
        recordSweep: (workflowUuid, workflowName) =>
          ledger.recordSweep(workflowUuid, workflowName),
      },
    );

    expect(
      await store.adoptPendingRecording(
        {
          workflowUuid: "pending",
          expectedVersion: "dead",
          workflowName: "WorkflowOne",
        },
        60_000,
      ),
    ).toEqual({ flipped: true, sweepCount: 1 });
    expect(
      await store.adoptPendingRecording(
        {
          workflowUuid: "enqueued",
          expectedVersion: "dead",
          workflowName: "WorkflowOne",
        },
        60_000,
      ),
    ).toEqual({ flipped: false });
    expect(
      await store.adoptPendingRecording(
        {
          workflowUuid: "terminal",
          expectedVersion: "dead",
          workflowName: "WorkflowOne",
        },
        60_000,
      ),
    ).toEqual({ flipped: false });
    expect(store.inspect("pending")).toMatchObject({
      status: "ENQUEUED",
      queueName: "_dbos_internal_queue",
      applicationVersion: null,
      workflowDeadlineEpochMs: null,
      deduplicationId: null,
      startedAtEpochMs: null,
      updatedAtEpochMs: 5_000,
      recoveryAttempts: 7,
    });
    expect(store.inspect("enqueued")).toMatchObject({
      applicationVersion: "dead",
      queueName: "custom",
      workflowDeadlineEpochMs: 700,
      deduplicationId: "keep",
      startedAtEpochMs: 450,
    });

    nowMs = 6_000;
    expect(
      await store.clearVersionOnEnqueuedRecording(
        {
          workflowUuid: "enqueued",
          expectedVersion: "dead",
          workflowName: "WorkflowOne",
        },
        60_000,
      ),
    ).toEqual({ flipped: true, sweepCount: 1 });
    expect(
      await store.clearVersionOnEnqueuedRecording(
        {
          workflowUuid: "terminal",
          expectedVersion: "dead",
          workflowName: "WorkflowOne",
        },
        60_000,
      ),
    ).toEqual({ flipped: false });
    expect(store.inspect("enqueued")).toMatchObject({
      applicationVersion: null,
      queueName: "custom",
      workflowDeadlineEpochMs: 700,
      deduplicationId: "keep",
      startedAtEpochMs: 450,
      updatedAtEpochMs: 200,
      recoveryAttempts: 3,
    });
    expect(store.inspect("terminal")).toMatchObject({
      status: "ERROR",
      applicationVersion: "dead",
      recoveryAttempts: 9,
    });
  });

  test("lists unhandled terminal failures inside the lookback, oldest first", async () => {
    const { makeInMemoryDbosStatusStore } = await sweepModule();
    const nowMs = 10_000;
    const handled = new Set(["already-handled"]);
    const store = makeInMemoryDbosStatusStore(
      [
        {
          workflowUuid: "too-old",
          name: "WorkflowOne",
          status: "ERROR",
          applicationVersion: null,
          createdAtEpochMs: 10,
          updatedAtEpochMs: nowMs - 5_000,
          recoveryAttempts: 1,
        },
        {
          workflowUuid: "already-handled",
          name: "WorkflowTwo",
          status: "ERROR",
          applicationVersion: "dead",
          createdAtEpochMs: 20,
          updatedAtEpochMs: nowMs - 400,
          recoveryAttempts: 2,
        },
        {
          workflowUuid: "max-recovery",
          name: "WorkflowTwo",
          status: "MAX_RECOVERY_ATTEMPTS_EXCEEDED",
          applicationVersion: "dead",
          createdAtEpochMs: 30,
          updatedAtEpochMs: nowMs - 300,
          recoveryAttempts: 10,
        },
        {
          workflowUuid: "error",
          name: "WorkflowOne",
          status: "ERROR",
          applicationVersion: "dead",
          createdAtEpochMs: 40,
          updatedAtEpochMs: nowMs - 200,
          recoveryAttempts: 2,
        },
        {
          workflowUuid: "success",
          name: "WorkflowOne",
          status: "SUCCESS",
          applicationVersion: "dead",
          createdAtEpochMs: 50,
          updatedAtEpochMs: nowMs - 100,
          recoveryAttempts: 0,
        },
      ],
      () => new Date(nowMs),
      {
        isTerminalFailureHandled: (workflowUuid) => handled.has(workflowUuid),
      },
    );

    // The lookback excludes too-old, the anti-join excludes already-handled,
    // the status filter excludes success, and the limit binds after both.
    expect(
      (await store.listUnhandledTerminalFailures(1_000, 1)).map(
        (row) => row.workflowUuid,
      ),
    ).toEqual(["max-recovery"]);
    expect(
      (await store.listUnhandledTerminalFailures(1_000, 10)).map(
        (row) => row.workflowUuid,
      ),
    ).toEqual(["max-recovery", "error"]);
  });
});

describe("DBOS sweep stores with live Postgres", () => {
  beforeAll(async () => {
    if (!dbReachable) return;
    DBOS.setConfig({
      name: "engrams-orchestrator",
      systemDatabaseUrl: DB_URL!,
      systemDatabaseSchemaName: "dbos",
      runAdminServer: false,
    });
    await DBOS.launch();
  });

  afterAll(async () => {
    if (!dbReachable) return;
    const db = getDb();
    await db.execute(sql`delete from "review"
                         where "task_id" like ${`${runId}-%`}`);
    await db.execute(sql`delete from "task_session"
                         where "task_id" like ${`${runId}-%`}`);
    await db.execute(sql`delete from "task"
                         where "id" like ${`${runId}-%`}`);
    await db.execute(sql`delete from "dbos"."workflow_status"
                         where "workflow_uuid" like ${`${runId}-%`}`);
    // Runs and their step rows cascade from the automation.
    await db.execute(sql`delete from "automation"
                         where "id" like ${`${runId}-%`}`);
    await db.execute(sql`delete from "org_setting"
                         where "key" like ${`${runId}-%`}`);
    await db.execute(sql`delete from "dbos_version_heartbeats"
                         where "pod_name" like ${`${runId}-%`}`);
    await db.execute(sql`delete from "dbos_sweep_ledger"
                         where "workflow_uuid" like ${`${runId}-%`}`);
    await db.execute(sql`delete from "dbos_sweep_lease"
                         where "owner" like ${`${runId}-%`}`);
    await DBOS.shutdown();
  });

  test.skipIf(!dbReachable)("heartbeat uses server time and strict grace", async () => {
    const { makeHeartbeatStore } = await sweepModule();
    const store = makeHeartbeatStore();
    await store.beat("live-version", `${runId}-pod`);
    await getDb().execute(sql`
      insert into "dbos_version_heartbeats"
        ("application_version", "pod_name", "last_seen")
      values ('old-version', ${`${runId}-old-pod`}, now() - interval '1 hour')
    `);

    expect(await store.liveVersions(60_000)).toEqual(["live-version"]);
    // Abandonment ages are computed entirely on PG's clock — no Date.now()
    // enters the comparison.
    const abandoned = await store.abandonedMsByVersion();
    expect(abandoned.get("live-version")).toBeLessThan(60_000);
    expect(abandoned.get("old-version")).toBeGreaterThan(30 * 60_000);
  });

  test.skipIf(!dbReachable)("prune deletes only rows older than the retention window", async () => {
    const { makeHeartbeatStore } = await sweepModule();
    const store = makeHeartbeatStore();
    await store.beat("prune-fresh-version", `${runId}-prune-fresh`);
    await getDb().execute(sql`
      insert into "dbos_version_heartbeats"
        ("application_version", "pod_name", "last_seen")
      values ('prune-old-version', ${`${runId}-prune-old`}, now() - interval '8 days')
    `);

    expect(await store.prune(7 * 24 * 60 * 60 * 1_000)).toBeGreaterThanOrEqual(1);

    const rows = await getDb().execute(sql`
      select "pod_name" from "dbos_version_heartbeats"
      where "pod_name" like ${`${runId}-prune-%`}
      order by "pod_name"
    `);
    expect(rows.rows.map((row) => row.pod_name)).toEqual([
      `${runId}-prune-fresh`,
    ]);
  });

  test.skipIf(!dbReachable)("lease steals only after expiry and release is owner-guarded", async () => {
    const { makeSweepLeaseStore } = await sweepModule();
    const store = makeSweepLeaseStore();
    const ownerA = `${runId}-owner-a`;
    const ownerB = `${runId}-owner-b`;

    expect(await store.tryAcquire(ownerA, 60_000)).toBe(true);
    expect(await store.tryAcquire(ownerB, 60_000)).toBe(false);
    await store.release(ownerB);
    expect(await store.tryAcquire(ownerB, 60_000)).toBe(false);
    await getDb().execute(sql`
      update "dbos_sweep_lease" set "expires_at" = now() - interval '1 second'
    `);
    expect(await store.tryAcquire(ownerB, 60_000)).toBe(true);
    await store.release(ownerB);
  });

  test.skipIf(!dbReachable)("ledger round-trips through Postgres", async () => {
    const { makeSweepLedgerStore } = await sweepModule();
    const store = makeSweepLedgerStore();
    const workflowUuid = `${runId}-ledger`;

    expect((await store.recordSweep(workflowUuid, "WorkflowOne")).sweepCount).toBe(1);
    expect((await store.recordSweep(workflowUuid, "WorkflowOne")).sweepCount).toBe(2);
    await store.setSuppressed(workflowUuid, true);
    await store.markAlerted(workflowUuid, "WorkflowOne");
    await store.markTerminalAlerted(workflowUuid, "WorkflowOne");
    expect(await store.get(workflowUuid)).toMatchObject({
      workflowUuid,
      workflowName: "WorkflowOne",
      sweepCount: 2,
      suppressed: true,
    });
    expect((await store.get(workflowUuid))?.alertedAt).not.toBeNull();
    expect((await store.get(workflowUuid))?.terminalAlertedAt).not.toBeNull();
  });

  test.skipIf(!dbReachable)(
    "DBOS status queries cap batches and preserve guarded transition fields",
    async () => {
      const { makeDbosStatusStore } = await sweepModule();
      const store = makeDbosStatusStore();
      const pendingA = `${runId}-pending-a`;
      const pendingB = `${runId}-pending-b`;
      const enqueued = `${runId}-enqueued`;
      const live = `${runId}-live`;
      const terminal = `${runId}-terminal`;
      const deadVersionA = `${runId}-dead-a`;
      const deadVersionB = `${runId}-dead-b`;
      const liveVersion = `${runId}-live-version`;

      await getDb().execute(sql`
        insert into "dbos"."workflow_status"
          ("workflow_uuid", "status", "name", "application_version",
           "recovery_attempts", "created_at", "updated_at", "queue_name",
           "workflow_deadline_epoch_ms", "deduplication_id", "started_at_epoch_ms")
        values
          (${pendingA}, 'PENDING', 'WorkflowOne', ${deadVersionA}, 7, 100, 100,
           'old-queue', 900, ${`${runId}-dedup-a`}, 500),
          (${pendingB}, 'PENDING', 'WorkflowOne', ${deadVersionA}, 1, 200, 200,
           null, null, null, null),
          (${enqueued}, 'ENQUEUED', 'WorkflowTwo', ${deadVersionB}, 3, 300, 300,
           'custom-queue', 901, ${`${runId}-dedup-b`}, 501),
          (${live}, 'PENDING', 'WorkflowOne', ${liveVersion}, 0, 50, 50,
           null, null, null, null),
          (${terminal}, 'SUCCESS', 'WorkflowOne', ${deadVersionA}, 9, 25, 25,
           null, null, null, null)
      `);

      expect(
        (await store.listNonTerminalOnVersionsNotIn([liveVersion], 2)).map(
          (row) => row.workflowUuid,
        ),
      ).toEqual([pendingA, pendingB]);

      expect(
        await store.adoptPendingRecording(
          {
            workflowUuid: pendingA,
            expectedVersion: deadVersionA,
            workflowName: "WorkflowOne",
          },
          60_000,
        ),
      ).toEqual({ flipped: true, sweepCount: 1 });
      expect(
        await store.adoptPendingRecording(
          {
            workflowUuid: enqueued,
            expectedVersion: deadVersionB,
            workflowName: "WorkflowTwo",
          },
          60_000,
        ),
      ).toEqual({ flipped: false });
      expect(
        await store.adoptPendingRecording(
          {
            workflowUuid: terminal,
            expectedVersion: deadVersionA,
            workflowName: "WorkflowOne",
          },
          60_000,
        ),
      ).toEqual({ flipped: false });
      expect(
        await store.clearVersionOnEnqueuedRecording(
          {
            workflowUuid: enqueued,
            expectedVersion: deadVersionB,
            workflowName: "WorkflowTwo",
          },
          60_000,
        ),
      ).toEqual({ flipped: true, sweepCount: 1 });
      expect(
        await store.clearVersionOnEnqueuedRecording(
          {
            workflowUuid: terminal,
            expectedVersion: deadVersionA,
            workflowName: "WorkflowOne",
          },
          60_000,
        ),
      ).toEqual({ flipped: false });

      const result = await getDb().execute(sql`
        select "workflow_uuid", "status", "application_version", "queue_name",
               "workflow_deadline_epoch_ms", "deduplication_id",
               "started_at_epoch_ms", "recovery_attempts", "updated_at"
        from "dbos"."workflow_status"
        where "workflow_uuid" in (${pendingA}, ${enqueued}, ${terminal})
        order by "workflow_uuid"
      `);
      const byId = new Map(result.rows.map((row) => [row.workflow_uuid, row]));
      expect(byId.get(pendingA)).toMatchObject({
        status: "ENQUEUED",
        application_version: null,
        queue_name: "_dbos_internal_queue",
        workflow_deadline_epoch_ms: null,
        deduplication_id: null,
        started_at_epoch_ms: null,
        recovery_attempts: "7",
      });
      expect(Number(byId.get(pendingA)?.updated_at)).toBeGreaterThan(1_000);
      expect(byId.get(enqueued)).toMatchObject({
        status: "ENQUEUED",
        application_version: null,
        queue_name: "custom-queue",
        workflow_deadline_epoch_ms: "901",
        deduplication_id: `${runId}-dedup-b`,
        started_at_epoch_ms: "501",
        recovery_attempts: "3",
        updated_at: "300",
      });
      expect(byId.get(terminal)).toMatchObject({
        status: "SUCCESS",
        application_version: deadVersionA,
        recovery_attempts: "9",
        updated_at: "25",
      });
    },
  );

  test.skipIf(!dbReachable)(
    "adoption is fenced by a heartbeat that arrives after listing",
    async () => {
      const {
        makeDbosStatusStore,
        makeSweepLedgerStore,
      } = await sweepModule();
      const store = makeDbosStatusStore();
      const workflowUuid = `${runId}-heartbeat-fence`;
      const applicationVersion = `${runId}-heartbeat-fence-version`;
      const podName = `${runId}-heartbeat-fence-pod`;

      await getDb().execute(sql`
        insert into "dbos"."workflow_status"
          ("workflow_uuid", "status", "name", "application_version",
           "recovery_attempts", "created_at", "updated_at")
        values (
          ${workflowUuid}, 'PENDING', 'WorkflowOne', ${applicationVersion},
          0, 100, 100
        )
      `);
      await getDb().execute(sql`
        insert into "dbos_version_heartbeats"
          ("application_version", "pod_name", "last_seen")
        values (${applicationVersion}, ${podName}, now())
      `);

      expect(
        await store.adoptPendingRecording(
          {
            workflowUuid,
            expectedVersion: applicationVersion,
            workflowName: "WorkflowOne",
          },
          60_000,
        ),
      ).toEqual({ flipped: false });
      expect(
        await makeSweepLedgerStore().get(workflowUuid),
      ).toBeNull();

      await getDb().execute(sql`
        update "dbos_version_heartbeats"
        set "last_seen" = now() - interval '1 hour'
        where "application_version" = ${applicationVersion}
          and "pod_name" = ${podName}
      `);
      expect(
        await store.adoptPendingRecording(
          {
            workflowUuid,
            expectedVersion: applicationVersion,
            workflowName: "WorkflowOne",
          },
          60_000,
        ),
      ).toEqual({ flipped: true, sweepCount: 1 });
    },
  );

  test.skipIf(!dbReachable)(
    "a ledger failure rolls the status flip back in the same transaction",
    async () => {
      const { makeDbosStatusStore } = await sweepModule();
      const store = makeDbosStatusStore();
      const workflowUuid = `${runId}-atomic-rollback`;
      const applicationVersion = `${runId}-atomic-rollback-version`;

      await getDb().execute(sql`
        insert into "dbos"."workflow_status"
          ("workflow_uuid", "status", "name", "application_version",
           "recovery_attempts", "created_at", "updated_at")
        values (
          ${workflowUuid}, 'PENDING', 'WorkflowOne', ${applicationVersion},
          0, 100, 100
        )
      `);
      await getDb().execute(sql`
        insert into "dbos_sweep_ledger"
          ("workflow_uuid", "workflow_name", "sweep_count")
        values (${workflowUuid}, 'WorkflowOne', 2147483647)
      `);

      await expect(
        store.adoptPendingRecording(
          {
            workflowUuid,
            expectedVersion: applicationVersion,
            workflowName: "WorkflowOne",
          },
          60_000,
        ),
      ).rejects.toThrow();

      const result = await getDb().execute(sql`
        select "workflow"."status", "workflow"."application_version",
               "ledger"."sweep_count"
        from "dbos"."workflow_status" as "workflow"
        inner join "dbos_sweep_ledger" as "ledger"
          on "ledger"."workflow_uuid" = "workflow"."workflow_uuid"
        where "workflow"."workflow_uuid" = ${workflowUuid}
      `);
      expect(result.rows[0]).toMatchObject({
        status: "PENDING",
        application_version: applicationVersion,
      });
      expect(Number(result.rows[0]?.sweep_count)).toBe(2147483647);
    },
  );

  test.skipIf(!dbReachable)(
    "stranded PENDING work on a live version is listed by dead executor and re-enqueued under a fence",
    async () => {
      const { makeDbosStatusStore, makeHeartbeatStore } = await sweepModule();
      const store = makeDbosStatusStore();
      const heartbeats = makeHeartbeatStore();
      const liveVersion = `${runId}-stranded-live-version`;
      const livePod = `${runId}-pod-live`;
      const deadPod = `${runId}-pod-dead`;
      const onLive = `${runId}-stranded-on-live`;
      const onDead = `${runId}-stranded-on-dead`;
      const onLocal = `${runId}-stranded-on-local`;
      const fresh = `${runId}-stranded-fresh`;
      const db = getDb();
      await heartbeats.beat(liveVersion, livePod);
      await db.execute(sql`
        insert into "dbos"."workflow_status"
          ("workflow_uuid", "status", "name", "application_version",
           "recovery_attempts", "created_at", "updated_at", "executor_id",
           "queue_name", "workflow_deadline_epoch_ms", "deduplication_id", "started_at_epoch_ms")
        values
          (${onLive}, 'PENDING', 'WorkflowOne', ${liveVersion}, 0, 100, 100, ${livePod},
           null, null, null, null),
          (${onDead}, 'PENDING', 'WorkflowOne', ${liveVersion}, 2, 100, 200, ${deadPod},
           null, 900, ${`${runId}-dedup-dead`}, 500),
          (${onLocal}, 'PENDING', 'WorkflowOne', ${liveVersion}, 0, 100, 150, 'local',
           null, null, null, null),
          (${fresh}, 'PENDING', 'WorkflowOne', ${liveVersion}, 0, 100,
           (extract(epoch from now()) * 1000)::bigint, ${deadPod},
           null, null, null, null)
      `);

      // Oldest updated first; the live pod's row and the fresh row are absent.
      expect(
        (await store.listPendingOnDeadExecutors([liveVersion], 60_000, 10)).map((row) => [
          row.workflowUuid,
          row.executorId,
        ]),
      ).toEqual([
        [onLocal, "local"],
        [onDead, deadPod],
      ]);

      expect(
        await store.requeueStrandedPendingRecording(
          { workflowUuid: onDead, executorId: deadPod, workflowName: "WorkflowOne" },
          60_000,
        ),
      ).toEqual({ flipped: true, sweepCount: 1 });
      const [moved] = (
        await db.execute(sql`
          select "status", "queue_name", "application_version", "executor_id",
                 "workflow_deadline_epoch_ms", "deduplication_id", "started_at_epoch_ms"
          from "dbos"."workflow_status"
          where "workflow_uuid" = ${onDead}
        `)
      ).rows;
      expect(moved).toMatchObject({
        status: "ENQUEUED",
        queue_name: "_dbos_internal_queue",
        application_version: liveVersion,
        executor_id: deadPod,
        workflow_deadline_epoch_ms: null,
        deduplication_id: null,
        started_at_epoch_ms: null,
      });

      // The fence: the dead pod beats again before the flip → no-op.
      await heartbeats.beat(liveVersion, deadPod);
      expect(
        await store.requeueStrandedPendingRecording(
          { workflowUuid: onLocal, executorId: "local", workflowName: "WorkflowOne" },
          60_000,
        ),
      ).toEqual({ flipped: true, sweepCount: 1 });
      expect(
        (await store.listPendingOnDeadExecutors([liveVersion], 60_000, 10)).map(
          (row) => row.workflowUuid,
        ),
      ).toEqual([]);
      expect(
        await store.requeueStrandedPendingRecording(
          { workflowUuid: onLive, executorId: livePod, workflowName: "WorkflowOne" },
          60_000,
        ),
      ).toEqual({ flipped: false });
    },
  );

  test.skipIf(!dbReachable)("terminal failure scan anti-joins the ledger's completion marks", async () => {
    const { makeDbosStatusStore, makeSweepLedgerStore } = await sweepModule();
    const store = makeDbosStatusStore();
    const ledger = makeSweepLedgerStore();
    const tooOld = `${runId}-failed-too-old`;
    const handled = `${runId}-failed-handled`;
    const alertedOnly = `${runId}-failed-alerted-only`;
    const fresh = `${runId}-failed-fresh`;

    const nowExpr = sql`(extract(epoch from now()) * 1000)::bigint`;
    await getDb().execute(sql`
      insert into "dbos"."workflow_status"
        ("workflow_uuid", "status", "name", "application_version",
         "recovery_attempts", "created_at", "updated_at")
      values
        (${tooOld}, 'ERROR', 'WorkflowOne', null, 1, 10, ${nowExpr} - 5000),
        (${handled}, 'ERROR', 'WorkflowTwo', 'dead', 2, 20, ${nowExpr} - 400),
        (${alertedOnly}, 'MAX_RECOVERY_ATTEMPTS_EXCEEDED', 'WorkflowTwo',
         'dead', 10, 30, ${nowExpr} - 300),
        (${fresh}, 'ERROR', 'WorkflowOne', 'dead', 2, 40, ${nowExpr} - 200)
    `);
    // handled carries the terminal-alert mark and is excluded; a decision
    // alert (alertedAt) must NOT count as handled.
    await ledger.markTerminalAlerted(handled, "WorkflowTwo");
    await ledger.markAlerted(alertedOnly, "WorkflowTwo");

    const rows = (await store.listUnhandledTerminalFailures(1_000, 500))
      .map((row) => row.workflowUuid)
      .filter((workflowUuid) => workflowUuid.startsWith(runId));
    expect(rows).toEqual([alertedOnly, fresh]);
  });

  test.skipIf(!dbReachable)("org settings upsert by key and read back the document", async () => {
    const store = makeOrgSettingStore();
    const key = `${runId}-${RETENTION_KEY}`;
    expect(await store.get(key)).toBeNull();
    await store.set(key, { runDetailDays: 45 }, `${runId}-admin`);
    expect(await store.get(key)).toEqual({ runDetailDays: 45 });
    await store.set(key, { runDetailDays: 60 }, null);
    expect(await store.get(key)).toEqual({ runDetailDays: 60 });
    const rows = await getDb().execute(sql`
      select "updated_by_user_id" from "org_setting" where "key" = ${key}
    `);
    expect(rows.rows.map((row) => row.updated_by_user_id)).toEqual([null]);
  });

  test.skipIf(!dbReachable)("retention prunes the step rows of old ended runs, keeping the run", async () => {
    const automationId = `${runId}-retention-auto`;
    await getDb().insert(automationTable).values({
      id: automationId,
      name: "retention test",
      description: "",
      enabled: true,
      currentVersion: 1,
    });
    const runs = makeAutomationStore(getDb());
    const trigger = { source: "manual", receivedAt: "2026-10-05T10:00:00Z" } as const;
    const day = 24 * 60 * 60 * 1_000;
    const now = Date.now();
    const seed = async (suffix: string, endedAt: Date | undefined) => {
      const id = `${runId}-run-${suffix}`;
      await runs.insertRun({
        id,
        automationId,
        version: 1,
        trigger,
        deliveryKey: `${runId}-${suffix}`,
        concurrencyKey: null,
        scheduledFor: null,
        ...(endedAt ? { status: "succeeded", endedAt } : {}),
      });
      await getDb().execute(sql`
        insert into "automation_step_run" ("run_id", "block_id", "attempt", "status")
        values (${id}, 'a', 1, 'succeeded'), (${id}, 'b', 1, 'succeeded')
      `);
      return id;
    };
    const oldest = await seed("oldest", new Date(now - 50 * day));
    const old = await seed("old", new Date(now - 40 * day));
    const fresh = await seed("fresh", new Date(now - 10 * day));
    const open = await seed("open", undefined);
    const store = makeRetentionStore();
    const cutoff = new Date(now - 30 * day);

    const stepsOf = async (id: string) =>
      Number(
        (await getDb().execute(sql`select count(*)::int as "n" from "automation_step_run" where "run_id" = ${id}`))
          .rows[0]!.n,
      );
    // Oldest first, one ROW per call: the batch bounds the transaction. The
    // oldest run still holds a row after the first call, so it is not
    // stamped yet.
    expect(await store.pruneRunDetails(cutoff, 1)).toEqual({ rows: 1, runs: 0 });
    expect(await stepsOf(oldest)).toBe(1);
    expect(await store.pruneRunDetails(cutoff, 1)).toEqual({ rows: 1, runs: 1 });
    expect(await stepsOf(oldest)).toBe(0);
    expect(await stepsOf(old)).toBe(2);
    expect(await store.pruneRunDetails(cutoff, 10)).toEqual({ rows: 2, runs: 1 });
    expect(await store.pruneRunDetails(cutoff, 10)).toEqual({ rows: 0, runs: 0 });
    expect([await stepsOf(old), await stepsOf(fresh), await stepsOf(open)]).toEqual([0, 2, 2]);
    // The run rows stay, the pruned ones stamped.
    const kept = await getDb().execute(sql`
      select "id", "details_pruned_at" is not null as "pruned"
      from "automation_run" where "id" like ${`${runId}-run-%`} order by "id"
    `);
    expect(kept.rows.map((row) => [row.id, row.pruned])).toEqual([
      [fresh, false],
      [old, true],
      [oldest, true],
      [open, false],
    ]);
  });

  test.skipIf(!dbReachable)("retention deletes terminal DBOS workflows past the cutoff with their records", async () => {
    const oldDone = `${runId}-ret-old-done`;
    const oldError = `${runId}-ret-old-error`;
    const oldPending = `${runId}-ret-old-pending`;
    const oldChild = `${runId}-ret-old-child`;
    const freshDone = `${runId}-ret-fresh-done`;
    const day = 24 * 60 * 60 * 1_000;
    const now = Date.now();
    await getDb().execute(sql`
      insert into "dbos"."workflow_status"
        ("workflow_uuid", "status", "name", "application_version",
         "recovery_attempts", "created_at", "updated_at", "parent_workflow_id")
      values
        (${oldPending}, 'PENDING', 'WorkflowOne', 'v', 0, 1, 1, null),
        (${oldChild}, 'SUCCESS', 'WorkflowOne', 'v', 0, 2, 2, ${oldPending}),
        (${oldDone}, 'SUCCESS', 'WorkflowOne', 'v', 0, 3, 3, null),
        (${oldError}, 'ERROR', 'WorkflowOne', 'v', 1, 4, 4, null),
        (${freshDone}, 'SUCCESS', 'WorkflowOne', 'v', 0, ${now - 10 * day}, ${now - 10 * day}, null)
    `);
    // The records the SDK keeps per workflow; the real schema cascades them.
    await getDb().execute(sql`
      insert into "dbos"."operation_outputs" ("workflow_uuid", "function_id", "function_name", "output")
      values (${oldDone}, 0, 'step', '"x"'), (${freshDone}, 0, 'step', '"y"')
    `);
    await getDb().execute(sql`
      insert into "dbos"."workflow_events" ("workflow_uuid", "key", "value")
      values (${oldDone}, 'k', '"v"')
    `);
    await getDb().execute(sql`
      insert into "dbos"."notifications" ("destination_uuid", "topic", "message")
      values (${oldDone}, 't', '"m"')
    `);
    await getDb().execute(sql`
      insert into "dbos"."streams" ("workflow_uuid", "key", "value", "offset")
      values (${oldDone}, 's', '"v"', 0)
    `);
    const store = makeRetentionStore();
    const cutoff = new Date(now - 30 * day);

    // Oldest first with the cap. These rows are older than any other test's
    // (created_at 1..4 ms), so the cap of one lands here: the PENDING row
    // and its child are skipped and the first SUCCESS goes.
    expect(await store.pruneDbosWorkflows(cutoff, 1)).toBe(1);
    const remaining = async () =>
      (
        await getDb().execute(sql`
          select "workflow_uuid" from "dbos"."workflow_status"
          where "workflow_uuid" like ${`${runId}-ret-%`} order by "workflow_uuid"
        `)
      ).rows.map((row) => row.workflow_uuid);
    expect(await remaining()).toEqual([freshDone, oldChild, oldError, oldPending]);
    // The next sweep takes the ERROR row — and whatever terminal rows the
    // earlier tests in this file left behind.
    expect(await store.pruneDbosWorkflows(cutoff, 1_000)).toBeGreaterThanOrEqual(1);
    expect(await store.pruneDbosWorkflows(cutoff, 1_000)).toBe(0);
    expect(await remaining()).toEqual([freshDone, oldChild, oldPending]);

    const countIn = async (table: string, column: string) =>
      Number(
        (
          await getDb().execute(
            sql`select count(*)::int as "n" from "dbos".${sql.identifier(table)} where ${sql.identifier(column)} like ${`${runId}-ret-%`}`,
          )
        ).rows[0]!.n,
      );
    expect(await countIn("operation_outputs", "workflow_uuid")).toBe(1);
    expect(await countIn("workflow_events", "workflow_uuid")).toBe(0);
    expect(await countIn("notifications", "destination_uuid")).toBe(0);
    expect(await countIn("streams", "workflow_uuid")).toBe(0);
  });
});
