import { describe, expect, test } from "bun:test";
import pino, { type Logger } from "pino";

import {
  DEFAULT_RETENTION_POLICY,
  makeInMemoryOrgSettingStore,
  parseRetentionPolicy,
  readRetentionPolicy,
  RETENTION_KEY,
  RETENTION_RUN_DETAIL_DAYS_DEFAULT,
} from "../../db/org-settings.ts";
import { makeInMemoryRetentionStore } from "../../db/retention.ts";
import { runRetentionTick } from "../retention.ts";

const NOW = new Date("2026-10-05T12:00:00.000Z");
const DAY_MS = 24 * 60 * 60 * 1_000;
const log: Logger = pino({ level: "silent" });
const daysAgo = (days: number) => new Date(NOW.getTime() - days * DAY_MS);

describe("retention policy parsing", () => {
  test("defaults when the setting is absent, malformed, or out of bounds", () => {
    expect(parseRetentionPolicy(null)).toEqual(DEFAULT_RETENTION_POLICY);
    expect(parseRetentionPolicy("30")).toEqual(DEFAULT_RETENTION_POLICY);
    expect(parseRetentionPolicy([30])).toEqual(DEFAULT_RETENTION_POLICY);
    expect(parseRetentionPolicy({ runDetailDays: 3 })).toEqual(DEFAULT_RETENTION_POLICY);
    expect(parseRetentionPolicy({ runDetailDays: 1000 })).toEqual(DEFAULT_RETENTION_POLICY);
    expect(parseRetentionPolicy({ runDetailDays: 7.5 })).toEqual(DEFAULT_RETENTION_POLICY);
    expect(DEFAULT_RETENTION_POLICY.runDetailDays).toBe(RETENTION_RUN_DETAIL_DAYS_DEFAULT);
  });

  test("reads a stored document, dropping unknown fields", () => {
    expect(parseRetentionPolicy({ runDetailDays: 90, sessionDays: 5 })).toEqual({ runDetailDays: 90 });
  });

  test("readRetentionPolicy goes through the org setting store", async () => {
    const settings = makeInMemoryOrgSettingStore();
    expect(await readRetentionPolicy(settings)).toEqual(DEFAULT_RETENTION_POLICY);
    await settings.set(RETENTION_KEY, { runDetailDays: 14 }, "u1");
    expect(await readRetentionPolicy(settings)).toEqual({ runDetailDays: 14 });
  });
});

describe("runRetentionTick", () => {
  test("prunes run details and terminal DBOS workflows older than the policy", async () => {
    const settings = makeInMemoryOrgSettingStore({ [RETENTION_KEY]: { runDetailDays: 30 } });
    const store = makeInMemoryRetentionStore({
      runs: [
        { id: "run-old", endedAt: daysAgo(31), steps: 4 },
        { id: "run-fresh", endedAt: daysAgo(29), steps: 2 },
        { id: "run-open", endedAt: null, steps: 1 },
        { id: "run-old-empty", endedAt: daysAgo(40), steps: 0, pruned: true },
      ],
      workflows: [
        { id: "wf-old-success", status: "SUCCESS", createdAt: daysAgo(31) },
        { id: "wf-old-error", status: "ERROR", createdAt: daysAgo(45) },
        { id: "wf-old-pending", status: "PENDING", createdAt: daysAgo(31) },
        { id: "wf-old-enqueued", status: "ENQUEUED", createdAt: daysAgo(31) },
        { id: "wf-fresh-success", status: "SUCCESS", createdAt: daysAgo(29) },
        { id: "wf-old-child", status: "SUCCESS", createdAt: daysAgo(35), parentId: "wf-old-pending" },
      ],
    });

    const result = await runRetentionTick({ settings, store, now: () => NOW, log });

    expect(result).toEqual({
      policy: { runDetailDays: 30 },
      cutoff: daysAgo(30).toISOString(),
      runDetailRowsPruned: 4,
      runsPruned: 1,
      workflowsPruned: 2,
    });
    expect(store.runs.get("run-old")).toEqual({ endedAt: daysAgo(31), steps: 0, pruned: true });
    expect(store.runs.get("run-fresh")?.steps).toBe(2);
    expect(store.runs.get("run-open")?.steps).toBe(1);
    expect([...store.workflows.keys()].sort()).toEqual([
      "wf-fresh-success",
      "wf-old-child",
      "wf-old-enqueued",
      "wf-old-pending",
    ]);
  });

  test("caps each prune at the batch of rows, oldest first, so a backlog drains over cycles", async () => {
    const settings = makeInMemoryOrgSettingStore();
    const store = makeInMemoryRetentionStore({
      runs: [
        { id: "run-a", endedAt: daysAgo(50), steps: 3 },
        { id: "run-b", endedAt: daysAgo(40), steps: 1 },
        { id: "run-c", endedAt: daysAgo(35), steps: 0 },
      ],
      workflows: [
        { id: "wf-a", status: "SUCCESS", createdAt: daysAgo(50) },
        { id: "wf-b", status: "SUCCESS", createdAt: daysAgo(40) },
        { id: "wf-c", status: "CANCELLED", createdAt: daysAgo(35) },
      ],
    });

    // Two rows of the oldest run go; nothing is stamped yet (run-a still
    // holds a row, and run-c is outside this cycle's frontier of two).
    const first = await runRetentionTick({ settings, store, now: () => NOW, batch: 2, log });
    expect([first.runDetailRowsPruned, first.runsPruned, first.workflowsPruned]).toEqual([2, 0, 2]);
    expect(store.runs.get("run-a")?.steps).toBe(1);
    expect([...store.workflows.keys()]).toEqual(["wf-c"]);

    // run-a's last row and run-b's one row: both stamped.
    const second = await runRetentionTick({ settings, store, now: () => NOW, batch: 2, log });
    expect([second.runDetailRowsPruned, second.runsPruned, second.workflowsPruned]).toEqual([2, 2, 1]);
    expect(store.workflows.size).toBe(0);

    // run-c never had rows: stamped so it leaves the frontier.
    const third = await runRetentionTick({ settings, store, now: () => NOW, batch: 2, log });
    expect([third.runDetailRowsPruned, third.runsPruned]).toEqual([0, 1]);
    expect(store.runs.get("run-c")?.pruned).toBe(true);
  });

  test("reads the policy fresh each tick", async () => {
    const settings = makeInMemoryOrgSettingStore({ [RETENTION_KEY]: { runDetailDays: 60 } });
    const store = makeInMemoryRetentionStore({
      runs: [{ id: "run-45", endedAt: daysAgo(45), steps: 1 }],
    });
    expect((await runRetentionTick({ settings, store, now: () => NOW, log })).runsPruned).toBe(0);
    await settings.set(RETENTION_KEY, { runDetailDays: 30 }, "u1");
    expect((await runRetentionTick({ settings, store, now: () => NOW, log })).runsPruned).toBe(1);
  });
});
