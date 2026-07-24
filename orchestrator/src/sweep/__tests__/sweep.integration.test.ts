/**
 * SDK contract test for the orphan sweep. It deliberately strands workflows
 * in a child process, then proves SDK 4.21.6's version-gated recovery, the
 * production PENDING -> NULL-version ENQUEUED flip, queue claim, replay, and
 * terminal-failure alert/cleanup paths against real Postgres.
 */
import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { DBOS } from "@dbos-inc/dbos-sdk";
import { sql } from "drizzle-orm";
import { fileURLToPath } from "node:url";
import pino, { type Logger } from "pino";

import { checkDb, getDb } from "../../db/client.ts";
import {
  makeDbosStatusStore,
  makeHeartbeatStore,
  makeSweepLeaseStore,
  makeSweepLedgerStore,
} from "../../db/dbos-sweep.ts";
import { makeSweepAlerter } from "../alerts.ts";
import { resolvePolicy, type ResolvedPolicy } from "../policy.ts";
import {
  DEFAULT_SWEEP_CONFIG,
  runSweepTick,
  type SweepConfig,
  type SweepTickDeps,
} from "../sweeper.ts";

const DB_URL = process.env["ORCHESTRATOR_DATABASE_URL"];
const dbReachable = DB_URL ? await checkDb() : false;
const runId = `sweepit-${Date.now()}`;
const deadVersion = `dead-${runId}`;
const childEntrypoint = fileURLToPath(
  new URL("./helpers/strand-child.ts", import.meta.url),
);
const log: Logger = pino({ level: "silent" });

// The module reads both switches at import time. The parent is the new deploy:
// it registers body B; children import the same module in their own process
// with body A.
process.env["SWEEP_TEST_RUNID"] = runId;
process.env["SWEEP_TEST_BODY"] = "b";
const {
  sweepTestChangedWorkflowName,
  sweepTestRecvWorkflowName,
} = await import("./helpers/sweep-test-workflows.ts");

const workflowIds = new Set<string>();
const heartbeatPodPrefix = `${runId}-pod`;
const POLL_INTERVAL_MS = 100;

interface WorkflowRow {
  workflowUuid: string;
  name: string;
  status: string;
  applicationVersion: string | null;
  executorId: string | null;
  updatedAtEpochMs: number;
}

function numberValue(value: unknown): number {
  if (
    typeof value === "number" ||
    typeof value === "string" ||
    typeof value === "bigint"
  ) {
    return Number(value);
  }
  throw new Error(`expected numeric value, received ${String(value)}`);
}

async function loadWorkflowRow(workflowId: string): Promise<WorkflowRow | null> {
  const result = await getDb().execute(sql`
    select "workflow_uuid", "name", "status", "application_version",
           "executor_id", "updated_at"
    from "dbos"."workflow_status"
    where "workflow_uuid" = ${workflowId}
    limit 1
  `);
  const row = result.rows[0];
  if (!row) return null;
  return {
    workflowUuid: String(row.workflow_uuid),
    name: String(row.name),
    status: String(row.status),
    applicationVersion:
      row.application_version === null
        ? null
        : String(row.application_version),
    executorId: row.executor_id === null ? null : String(row.executor_id),
    updatedAtEpochMs: numberValue(row.updated_at),
  };
}

function observed(value: unknown): string {
  try {
    return JSON.stringify(value);
  } catch {
    return String(value);
  }
}

async function pollUntil<T>(
  description: string,
  timeoutMs: number,
  read: () => Promise<T>,
  accept: (value: T) => boolean,
): Promise<T> {
  const deadline = Date.now() + timeoutMs;
  let last: T;
  while (true) {
    last = await read();
    if (accept(last)) return last;
    if (Date.now() >= deadline) {
      throw new Error(
        `Timed out waiting ${timeoutMs}ms for ${description}; ` +
          `last observed: ${observed(last)}`,
      );
    }
    await Bun.sleep(POLL_INTERVAL_MS);
  }
}

async function assertRemains<T>(
  description: string,
  durationMs: number,
  read: () => Promise<T>,
  predicate: (value: T) => boolean,
): Promise<void> {
  const deadline = Date.now() + durationMs;
  let last: T;
  while (true) {
    last = await read();
    if (!predicate(last)) {
      throw new Error(
        `${description} changed before ${durationMs}ms elapsed; ` +
          `observed: ${observed(last)}`,
      );
    }
    if (Date.now() >= deadline) return;
    await Bun.sleep(POLL_INTERVAL_MS);
  }
}

async function spawnStranded(
  workflowKind: "recv" | "changed",
  workflowId: string,
  body: "a" | "b" = "a",
): Promise<void> {
  workflowIds.add(workflowId);
  const child = Bun.spawn([process.execPath, childEntrypoint], {
    cwd: fileURLToPath(new URL("../../..", import.meta.url)),
    env: {
      ...process.env,
      ORCHESTRATOR_DATABASE_URL: DB_URL!,
      SWEEP_TEST_RUNID: runId,
      SWEEP_TEST_BODY: body,
      SWEEP_TEST_WORKFLOW: workflowKind,
      SWEEP_TEST_WORKFLOW_ID: workflowId,
      DBOS__APPVERSION: deadVersion,
      DBOS__VMID: "dead-pod",
    },
    stdout: "pipe",
    stderr: "pipe",
  });
  const stdoutPromise = new Response(child.stdout).text();
  const stderrPromise = new Response(child.stderr).text();
  const [exitCode, stdout, stderr] = await Promise.all([
    child.exited,
    stdoutPromise,
    stderrPromise,
  ]);
  if (exitCode !== 0 || !stdout.includes("STRANDED")) {
    throw new Error(
      `strand child failed for ${workflowId}: exit=${exitCode}, ` +
        `stdout=${observed(stdout)}, stderr=${observed(stderr)}`,
    );
  }
}

function integrationPolicy(name: string): ResolvedPolicy {
  if (
    name === sweepTestRecvWorkflowName ||
    name === sweepTestChangedWorkflowName
  ) {
    return { mode: "adopt", staleAfterHours: 48 };
  }
  return resolvePolicy(name);
}

function config(overrides: Partial<SweepConfig> = {}): SweepConfig {
  return {
    ...DEFAULT_SWEEP_CONFIG,
    heartbeatIntervalMs: 1_000,
    sweepIntervalMs: 5_000,
    graceMs: 5_000,
    batchCap: 5,
    ...overrides,
  };
}

function sweepDeps(
  owner: string,
  overrides: Partial<SweepTickDeps> = {},
): SweepTickDeps {
  return {
    owner: `${runId}-${owner}`,
    appVersion: () => DBOS.applicationVersion,
    config: config(),
    heartbeats: makeHeartbeatStore(),
    lease: makeSweepLeaseStore(),
    ledger: makeSweepLedgerStore(),
    status: makeDbosStatusStore(),
    cancelWorkflow: (workflowId) => DBOS.cancelWorkflow(workflowId),
    resolvePolicy: integrationPolicy,
    log,
    ...overrides,
  };
}

async function beatParent(gracePod = "parent"): Promise<void> {
  await makeHeartbeatStore().beat(
    DBOS.applicationVersion,
    `${heartbeatPodPrefix}-${gracePod}`,
  );
}

async function insertSyntheticPending(
  workflowId: string,
  applicationVersion: string,
): Promise<void> {
  const now = Date.now();
  workflowIds.add(workflowId);
  await getDb().execute(sql`
    insert into "dbos"."workflow_status"
      ("workflow_uuid", "status", "name", "application_version",
       "executor_id", "recovery_attempts", "created_at", "updated_at")
    values (
      ${workflowId}, 'PENDING', ${sweepTestRecvWorkflowName},
      ${applicationVersion}, ${`${runId}-synthetic`}, 0, ${now}, ${now}
    )
  `);
}

async function deleteSynthetic(workflowId: string): Promise<void> {
  await DBOS.cancelWorkflow(workflowId).catch(() => {});
  await getDb().execute(sql`
    delete from "dbos"."workflow_status"
    where "workflow_uuid" = ${workflowId}
  `);
}

describe.skipIf(!dbReachable)("DBOS orphan sweep (real engine + Postgres)", () => {
  beforeAll(async () => {
    DBOS.setConfig({
      name: "engrams-orchestrator",
      systemDatabaseUrl: DB_URL!,
      systemDatabaseSchemaName: "dbos",
      runAdminServer: false,
    });
    await DBOS.launch();
  });

  afterAll(async () => {
    for (const workflowId of workflowIds) {
      try {
        const row = await loadWorkflowRow(workflowId);
        if (
          row &&
          ["PENDING", "ENQUEUED", "DELAYED"].includes(row.status)
        ) {
          await DBOS.cancelWorkflow(workflowId).catch(() => {});
        }
      } catch {
        // Best-effort cancellation; the SQL cleanup below is authoritative.
      }
    }

    await DBOS.shutdown();

    const db = getDb();
    await db.execute(sql`
      delete from "dbos"."workflow_status"
      where "workflow_uuid" like ${`%${runId}%`}
    `);
    await db.execute(sql`
      delete from "dbos_version_heartbeats"
      where "application_version" like ${`%${runId}%`}
         or "pod_name" like ${`${runId}%`}
    `);
    await db.execute(sql`
      delete from "dbos_sweep_ledger"
      where "workflow_uuid" like ${`%${runId}%`}
    `);
    await db.execute(sql`
      delete from "dbos_sweep_lease"
      where "owner" like ${`${runId}%`}
    `);
    await db.execute(sql`
      delete from "dbos"."application_versions"
      where "version_name" like ${`%${runId}%`}
    `);
  });

  test(
    "stranded receive workflow is adopted and drains its queued backlog",
    async () => {
      const workflowId = `sweepit-recv-${runId}`;
      await spawnStranded("recv", workflowId);

      expect(await loadWorkflowRow(workflowId)).toMatchObject({
        workflowUuid: workflowId,
        name: sweepTestRecvWorkflowName,
        status: "PENDING",
        applicationVersion: deadVersion,
        executorId: "dead-pod",
      });

      const payloads = ["one", "two", "three"];
      for (const [index, payload] of payloads.entries()) {
        await DBOS.send(
          workflowId,
          payload,
          "sweeptest",
          `${workflowId}:payload:${index}`,
        );
      }

      await assertRemains(
        `${workflowId} to remain stranded on its dead application version`,
        2_000,
        () => loadWorkflowRow(workflowId),
        (row) =>
          row?.status === "PENDING" &&
          row.applicationVersion === deadVersion &&
          row.executorId === "dead-pod",
      );
      await DBOS.send(
        workflowId,
        "stop",
        "sweeptest",
        `${workflowId}:stop`,
      );

      await beatParent("adopt-recv");
      const tick = await runSweepTick(sweepDeps("adopt-recv"));
      expect(
        tick.decisions.find(
          (decision) => decision.workflowUuid === workflowId,
        ),
      ).toMatchObject({ action: "adopted" });

      await pollUntil(
        `${workflowId} to reach SUCCESS after the NULL-version queue claim`,
        20_000,
        () => loadWorkflowRow(workflowId),
        (row) => row?.status === "SUCCESS",
      );

      const output = await DBOS.retrieveWorkflow<string[]>(
        workflowId,
      ).getResult();
      expect(output).toEqual(payloads);
      expect(await loadWorkflowRow(workflowId)).toMatchObject({
        status: "SUCCESS",
        executorId: DBOS.executorID,
        applicationVersion: DBOS.applicationVersion,
      });
    },
    45_000,
  );

  test(
    "changed body fails loud after adoption and alerts and cleans up once",
    async () => {
      const workflowId = `sweepit-changed-${runId}`;
      await spawnStranded("changed", workflowId, "a");

      const childSteps = await DBOS.listWorkflowSteps(workflowId);
      expect(childSteps).toEqual(
        expect.arrayContaining([
          expect.objectContaining({ name: "alpha", output: "a" }),
        ]),
      );

      await beatParent("adopt-changed");
      const tick = await runSweepTick(sweepDeps("adopt-changed"));
      expect(
        tick.decisions.find(
          (decision) => decision.workflowUuid === workflowId,
        ),
      ).toMatchObject({ action: "adopted" });

      const failedRow = await pollUntil(
        `${workflowId} to fail replay with DBOSUnexpectedStepError`,
        20_000,
        () => loadWorkflowRow(workflowId),
        (row) => row?.status === "ERROR",
      );
      if (failedRow === null) {
        throw new Error(`${workflowId} disappeared after reaching ERROR`);
      }
      const failedStatus = await DBOS.getWorkflowStatus(workflowId);
      const errorText =
        failedStatus?.error instanceof Error
          ? `${failedStatus.error.name}: ${failedStatus.error.message}`
          : observed(failedStatus?.error);
      expect(errorText).toContain("alpha");
      expect(errorText).toContain("beta");
      expect(failedStatus?.status).toBe("ERROR");

      const ledger = makeSweepLedgerStore();
      const posts: string[] = [];
      // Scan only this scenario's workflow so unrelated terminal failures in
      // a shared test database cannot skew the counts.
      const rawStatus = makeDbosStatusStore();
      const status = {
        ...rawStatus,
        async listUnhandledTerminalFailures(
          lookbackMs: number,
          limit: number,
        ) {
          const rows = await rawStatus.listUnhandledTerminalFailures(
            lookbackMs,
            limit,
          );
          return rows.filter((row) => row.workflowUuid === workflowId);
        },
      };
      // Alerts are error-level log lines; capture the terminal-failure ones.
      const alertLog = {
        info() {},
        warn() {},
        error(fields: Record<string, unknown>, message: string) {
          if (message === "DBOS workflow terminal failure") {
            posts.push(`${message} ${JSON.stringify(fields)}`);
          }
        },
      } as unknown as Logger;
      const alerter = makeSweepAlerter({
        ledger,
        status,
        log: alertLog,
      });

      const firstScan = await alerter.scanTerminalFailures();
      expect(firstScan).toEqual({ scanned: 1 });
      expect(posts).toHaveLength(1);
      expect(posts[0]).toContain(workflowId);
      expect((await ledger.get(workflowId))?.terminalAlertedAt).not.toBeNull();

      // The terminal-alert mark is recorded, so the anti-join now excludes
      // the row entirely — no re-scan, no duplicate alert line.
      const secondScan = await alerter.scanTerminalFailures();
      expect(secondScan.scanned).toBe(0);
      expect(posts).toHaveLength(1);
    },
    45_000,
  );

  test(
    "a live version is protected until its heartbeat leaves the grace window",
    async () => {
      const workflowId = `sweepit-grace-${runId}`;
      const otherVersion = `other-live-${runId}`;
      await insertSyntheticPending(workflowId, otherVersion);

      try {
        const heartbeats = makeHeartbeatStore();
        await heartbeats.beat(otherVersion, `${heartbeatPodPrefix}-other`);
        await beatParent("grace-first");
        const firstTick = await runSweepTick(
          sweepDeps("grace-first", {
            config: config({ graceMs: 5_000 }),
          }),
        );
        expect(firstTick.liveVersions).toContain(otherVersion);
        expect(
          firstTick.decisions.some(
            (decision) => decision.workflowUuid === workflowId,
          ),
        ).toBe(false);
        expect(await loadWorkflowRow(workflowId)).toMatchObject({
          status: "PENDING",
          applicationVersion: otherVersion,
        });

        await Bun.sleep(50);
        const parentPod = `${heartbeatPodPrefix}-grace-second`;
        await heartbeats.beat(DBOS.applicationVersion, parentPod);
        // A literal 1ms grace is intentionally used below. Pin only this
        // namespaced test heartbeat slightly into the future so connection
        // scheduling cannot make the sweeper reject its own version before
        // it evaluates the deliberately expired other-version heartbeat.
        await getDb().execute(sql`
          update "dbos_version_heartbeats"
          set "last_seen" = now() + interval '5 seconds'
          where "application_version" = ${DBOS.applicationVersion}
            and "pod_name" = ${parentPod}
        `);
        const secondTick = await runSweepTick(
          sweepDeps("grace-second", {
            config: config({ graceMs: 1 }),
          }),
        );
        expect(secondTick.liveVersions).not.toContain(otherVersion);
        expect(
          secondTick.decisions.find(
            (decision) => decision.workflowUuid === workflowId,
          ),
        ).toMatchObject({ action: "adopted" });
      } finally {
        await deleteSynthetic(workflowId);
      }
    },
    30_000,
  );

  test(
    "concurrent sweepers lease once and perform one version-clearing flip",
    async () => {
      const workflowId = `sweepit-concurrent-${runId}`;
      await insertSyntheticPending(workflowId, `concurrent-dead-${runId}`);

      try {
        await beatParent("concurrent");
        const [first, second] = await Promise.all([
          runSweepTick(sweepDeps("concurrent-a")),
          runSweepTick(sweepDeps("concurrent-b")),
        ]);
        // The lease is released when a tick completes, so two fast ticks can
        // hold it SEQUENTIALLY — instantaneous exclusion is not assertable
        // here. The durable property is single-flip idempotency: whichever
        // subset of ticks ran, the row was adopted exactly once.
        expect([first, second].some((result) => result.leaseHeld)).toBe(true);
        expect(
          [...first.decisions, ...second.decisions].filter(
            (decision) =>
              decision.workflowUuid === workflowId &&
              decision.action === "adopted",
          ),
        ).toHaveLength(1);
        expect(await loadWorkflowRow(workflowId)).toMatchObject({
          status: "ENQUEUED",
          applicationVersion: null,
        });
        expect((await makeSweepLedgerStore().get(workflowId))?.sweepCount).toBe(
          1,
        );
      } finally {
        await deleteSynthetic(workflowId);
      }
    },
    30_000,
  );
});
