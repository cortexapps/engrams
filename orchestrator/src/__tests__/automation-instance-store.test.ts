/**
 * Live-PG tests for the automation instance store (ADR 0120 addendum):
 * idempotent race-safe open, the one-open-instance-per-key partial unique
 * (closed history accumulates), close CAS, the exclusive handle ledger
 * (recorded / already_ours / conflict), resolveHandles, and the run-table
 * dedupe split (occurrence identity gains instance_id; delivery identity
 * stays instance-blind but scoped to non-cron rows) plus the instance-aware
 * cron claim predicates. Gated on ORCHESTRATOR_DATABASE_URL + a
 * reachability probe, per the db.test.ts convention; rows use unique ids so
 * the suite is parallel-safe against the shared migrated database.
 */

import { afterAll, describe, expect, test } from "bun:test";
import { inArray } from "drizzle-orm";

import { sql } from "drizzle-orm";

import { checkDb, getDb } from "../db/client.ts";
import {
  INSTANCE_KEY_MAX_CHARS,
  InstanceLimitError,
  makeAutomationInstanceStore,
  newInstanceId,
} from "../db/automation-instances.ts";
import { makeAutomationEngineStore, makeAutomationStore } from "../db/automations.ts";
import {
  automation as automationTable,
  automationRun as automationRunTable,
  automationSession as automationSessionTable,
} from "../db/schema.ts";
import type { AutomationRunTrigger } from "../db/schema.ts";

const DB_URL = process.env["ORCHESTRATOR_DATABASE_URL"];
const dbReachable = DB_URL ? await checkDb() : false;

const UNIQ = `${Date.now()}-${Math.floor(Math.random() * 1e6)}`;
const createdAutomationIds: string[] = [];

async function seedAutomation(suffix: string): Promise<string> {
  const id = `instance-store-auto-${UNIQ}-${suffix}`;
  await getDb().insert(automationTable).values({
    id,
    name: `Instance store test ${id}`,
    description: "",
    enabled: true,
    currentVersion: 1,
  });
  createdAutomationIds.push(id);
  return id;
}

afterAll(async () => {
  if (!dbReachable || createdAutomationIds.length === 0) return;
  // Instances, handles, and runs cascade with the automation.
  await getDb().delete(automationTable).where(inArray(automationTable.id, createdAutomationIds));
});

const MANUAL_TRIGGER: AutomationRunTrigger = {
  source: "manual",
  receivedAt: "2026-08-25T00:00:00Z",
};

describe.skipIf(!dbReachable)("automation instance store (live PG)", () => {
  test("openInstance is idempotent for a key and a concurrent open joins the winner", async () => {
    const autoId = await seedAutomation("open");
    const store = makeAutomationInstanceStore();

    const first = await store.openInstance({
      automationId: autoId,
      key: "project-ENG-1",
      inputs: { channel: "#eng-1" },
      openedBy: "user:u-1",
    });
    expect(first.id.startsWith("ai_")).toBe(true);
    expect(first.status).toBe("open");

    // A concurrent open of the same key must JOIN, never mutate the snapshot.
    const [a, b] = await Promise.all([
      store.openInstance({
        automationId: autoId,
        key: "project-ENG-1",
        inputs: { channel: "#other" },
        openedBy: "user:u-2",
      }),
      store.openInstance({
        automationId: autoId,
        key: "project-ENG-1",
        inputs: { channel: "#third" },
        openedBy: "user:u-3",
      }),
    ]);
    expect(a.id).toBe(first.id);
    expect(b.id).toBe(first.id);
    expect(a.inputs).toEqual({ channel: "#eng-1" });

    // Same key, different automation = a different instance.
    const otherAuto = await seedAutomation("open-other");
    const other = await store.openInstance({
      automationId: otherAuto,
      key: "project-ENG-1",
      inputs: {},
      openedBy: "user:u-1",
    });
    expect(other.id).not.toBe(first.id);
  });

  test("close is a CAS and reopening the key mints a NEW instance (closed history accumulates)", async () => {
    const autoId = await seedAutomation("close");
    const store = makeAutomationInstanceStore();

    const first = await store.openInstance({
      automationId: autoId,
      key: "project-ENG-2",
      inputs: { round: 1 },
      openedBy: "user:u-1",
    });
    expect(await store.closeInstance({ instanceId: first.id, reason: "shipped" })).toBe(true);
    // Second close is a no-op, not an error.
    expect(await store.closeInstance({ instanceId: first.id })).toBe(false);

    const closed = await store.getInstance(first.id);
    expect(closed).toMatchObject({ status: "closed", closeReason: "shipped" });
    expect(closed?.closedAt).not.toBeNull();
    expect(await store.getOpenInstanceByKey(autoId, "project-ENG-2")).toBeNull();

    const second = await store.openInstance({
      automationId: autoId,
      key: "project-ENG-2",
      inputs: { round: 2 },
      openedBy: "user:u-1",
    });
    expect(second.id).not.toBe(first.id);
    expect(second.inputs).toEqual({ round: 2 });

    const open = await store.listOpenInstances(autoId);
    expect(open.map((i) => i.id)).toEqual([second.id]);
  });

  test("listOpenInstances is oldest-first and capped", async () => {
    const autoId = await seedAutomation("list");
    // Injected advancing clock: three same-millisecond opens would tie on
    // opened_at and fall to the (random) id tiebreak.
    let tick = Date.parse("2026-08-25T00:00:00Z");
    const store = makeAutomationInstanceStore({ now: () => new Date(++tick) });
    for (const key of ["k-1", "k-2", "k-3"]) {
      await store.openInstance({ automationId: autoId, key, inputs: {}, openedBy: "" });
    }
    const capped = await store.listOpenInstances(autoId, 2);
    expect(capped.map((i) => i.key)).toEqual(["k-1", "k-2"]);
  });

  test("key limits refuse before touching the table", async () => {
    const autoId = await seedAutomation("limits");
    const store = makeAutomationInstanceStore();
    for (const key of ["", "x".repeat(INSTANCE_KEY_MAX_CHARS + 1)]) {
      await expect(
        store.openInstance({ automationId: autoId, key, inputs: {}, openedBy: "" }),
      ).rejects.toBeInstanceOf(InstanceLimitError);
    }
    await expect(
      store.recordInstanceHandle({
        automationId: autoId,
        handle: "",
        instanceId: "ai_x",
        writtenBy: "",
      }),
    ).rejects.toBeInstanceOf(InstanceLimitError);
  });

  test("the handle ledger is exclusive: recorded, replay-idempotent, conflict — and permanent past close", async () => {
    const autoId = await seedAutomation("handles");
    const store = makeAutomationInstanceStore();
    const one = await store.openInstance({
      automationId: autoId,
      key: "thread-a",
      inputs: {},
      openedBy: "",
    });
    const two = await store.openInstance({
      automationId: autoId,
      key: "thread-b",
      inputs: {},
      openedBy: "",
    });

    const handle = "slack:C1:1724500000.000100";
    expect(
      await store.recordInstanceHandle({
        automationId: autoId,
        handle,
        instanceId: one.id,
        writtenBy: "run-1:post",
      }),
    ).toEqual({ kind: "recorded" });
    // A DBOS step retry replays the same write: converges, no conflict.
    expect(
      await store.recordInstanceHandle({
        automationId: autoId,
        handle,
        instanceId: one.id,
        writtenBy: "run-1:post",
      }),
    ).toEqual({ kind: "already_ours" });
    // A different instance claiming the same handle refuses loudly.
    expect(
      await store.recordInstanceHandle({
        automationId: autoId,
        handle,
        instanceId: two.id,
        writtenBy: "run-2:post",
      }),
    ).toEqual({ kind: "conflict", instanceId: one.id });

    // Handles outlive the instance (audit; reopen never re-routes old
    // threads) — resolution now reports the closed owner.
    await store.closeInstance({ instanceId: one.id });
    const resolved = await store.resolveHandles(autoId, [handle, "slack:C1:none"]);
    expect(resolved).toEqual([
      { handle, instanceId: one.id, instanceStatus: "closed" },
    ]);
    expect(await store.resolveHandles(autoId, [])).toEqual([]);

    // Another automation's ledger is independent.
    const otherAuto = await seedAutomation("handles-other");
    expect(await store.resolveHandles(otherAuto, [handle])).toEqual([]);
  });
});

describe.skipIf(!dbReachable)("run dedupe split + instance-aware cron claims (live PG)", () => {
  test("one external delivery lands in at most one instance (delivery unique is instance-blind)", async () => {
    const autoId = await seedAutomation("delivery");
    const store = makeAutomationStore();

    const first = await store.insertRun({
      id: `autorun:${autoId}:d-1:a`,
      automationId: autoId,
      version: 1,
      instanceId: "ai_one",
      trigger: MANUAL_TRIGGER,
      deliveryKey: "github:delivery-1",
      concurrencyKey: null,
      scheduledFor: null,
    });
    expect(first.instanceId).toBe("ai_one");

    // The same delivery admitted toward a DIFFERENT instance converges onto
    // the incumbent row (insert conflicts, reselect by id finds nothing —
    // the admission retry path); asserting the raw unique here.
    await expect(
      store.insertRun({
        id: `autorun:${autoId}:d-1:b`,
        automationId: autoId,
        version: 1,
        instanceId: "ai_two",
        trigger: MANUAL_TRIGGER,
        deliveryKey: "github:delivery-1",
        concurrencyKey: null,
        scheduledFor: null,
      }),
    ).rejects.toThrow(/disappeared after insert/);

    // An unbound run with a distinct delivery key still inserts beside it.
    const unbound = await store.insertRun({
      id: `autorun:${autoId}:d-2`,
      automationId: autoId,
      version: 1,
      trigger: MANUAL_TRIGGER,
      deliveryKey: "github:delivery-2",
      concurrencyKey: null,
      scheduledFor: null,
    });
    expect(unbound.instanceId).toBe("");
  });

  test("a cron occurrence fans out one row per instance, and claims never cross instances", async () => {
    const autoId = await seedAutomation("cron");
    const store = makeAutomationStore();
    const scheduledFor = new Date("2026-08-25T12:00:00Z");
    const now = new Date("2026-08-25T12:00:05Z");
    const lease = { leaseOwner: "pod-a", leaseExpiresAt: new Date(now.getTime() + 60_000), now };

    // The same occurrence claims once per instance ('' = the classic global
    // row): three sibling rows share delivery_key `cron:<epoch>` — the
    // occurrence unique admits them, the delivery unique ignores cron rows.
    for (const instanceId of ["", "ai_one", "ai_two"]) {
      const claim = await store.claimCronOccurrence({
        runId: `autorun:${autoId}:cron${instanceId ? `:i-${instanceId}` : ""}`,
        automationId: autoId,
        version: 1,
        ...(instanceId ? { instanceId } : {}),
        scheduledFor,
        ...lease,
      });
      expect(claim?.kind).toBe("claimed");
      expect(claim?.run.instanceId).toBe(instanceId);
    }

    // Re-claiming ai_one's occurrence under a live lease yields nothing —
    // and must NOT reacquire a sibling instance's row.
    const reclaim = await store.claimCronOccurrence({
      runId: `autorun:${autoId}:cron:i-ai_one`,
      automationId: autoId,
      version: 1,
      instanceId: "ai_one",
      scheduledFor,
      leaseOwner: "pod-b",
      leaseExpiresAt: new Date(now.getTime() + 120_000),
      now,
    });
    expect(reclaim).toBeNull();

    // After ai_one's lease expires, reacquire targets exactly that row.
    const expiredNow = new Date(now.getTime() + 61_000);
    const reacquired = await store.claimCronOccurrence({
      runId: `autorun:${autoId}:cron:i-ai_one`,
      automationId: autoId,
      version: 1,
      instanceId: "ai_one",
      scheduledFor,
      leaseOwner: "pod-b",
      leaseExpiresAt: new Date(expiredNow.getTime() + 60_000),
      now: expiredNow,
    });
    expect(reacquired?.kind).toBe("claimed");
    expect(reacquired?.run.instanceId).toBe("ai_one");
    expect(reacquired?.run.leaseOwner).toBe("pod-b");

    // A row whose workflow STARTED (left pending) classifies in_flight —
    // the occurrence fired; the scheduler must not wait for it to finish.
    await getDb()
      .update(automationRunTable)
      .set({ status: "running", leaseOwner: null, leaseExpiresAt: null })
      .where(sql`id = ${`autorun:${autoId}:cron:i-ai_one`}`);
    const inFlight = await store.claimCronOccurrence({
      runId: `autorun:${autoId}:cron:i-ai_one`,
      automationId: autoId,
      version: 1,
      instanceId: "ai_one",
      scheduledFor,
      leaseOwner: "pod-d",
      leaseExpiresAt: new Date(expiredNow.getTime() + 60_000),
      now: expiredNow,
    });
    expect(inFlight?.kind).toBe("in_flight");
    expect(inFlight?.run.instanceId).toBe("ai_one");

    // Terminal detection is per-instance too.
    await store.markRunSkipped(`autorun:${autoId}:cron:i-ai_two`, "no work");
    const terminal = await store.claimCronOccurrence({
      runId: `autorun:${autoId}:cron:i-ai_two`,
      automationId: autoId,
      version: 1,
      instanceId: "ai_two",
      scheduledFor,
      leaseOwner: "pod-c",
      leaseExpiresAt: new Date(expiredNow.getTime() + 60_000),
      now: expiredNow,
    });
    expect(terminal?.kind).toBe("terminal");
    expect(terminal?.run.instanceId).toBe("ai_two");
  });

  test("newInstanceId never contains a scoping separator", () => {
    for (let i = 0; i < 32; i++) {
      const id = newInstanceId();
      expect(id).toMatch(/^ai_[a-z2-7]{16}$/);
    }
  });
});

describe.skipIf(!dbReachable)("instance-aware engine store (live PG)", () => {
  async function seedVersion(automationId: string): Promise<void> {
    await getDb().execute(sql`
      insert into automation_version (automation_id, version, trigger, blocks, inputs_schema, settings)
      values (${automationId}, 1, ${JSON.stringify({ kind: "manual" })}::jsonb, '[]'::jsonb,
              ${JSON.stringify([
                { key: "channel", label: "Channel", type: "string" },
                { key: "project", label: "Project", type: "string" },
              ])}::jsonb,
              ${JSON.stringify({ endSessionsOnFinish: false })}::jsonb)
    `);
  }

  test("loadSnapshot resolves inputs from the instance snapshot and carries id + key", async () => {
    const autoId = await seedAutomation("snapshot");
    await seedVersion(autoId);
    await getDb()
      .update(automationTable)
      .set({ inputs: { channel: "#general", project: "row-default" } })
      .where(sql`id = ${autoId}`);
    const instances = makeAutomationInstanceStore();
    const instance = await instances.openInstance({
      automationId: autoId,
      key: "project-ENG-9",
      inputs: { project: "ENG-9" },
      openedBy: "test",
    });
    const store = makeAutomationStore();
    const engine = makeAutomationEngineStore();
    await store.insertRun({
      id: `autorun:${autoId}:main:i-${instance.id}:manual:1`,
      automationId: autoId,
      version: 1,
      instanceId: instance.id,
      trigger: MANUAL_TRIGGER,
      deliveryKey: "manual:1",
      concurrencyKey: null,
      scheduledFor: null,
    });
    const snapshot = await engine.loadSnapshot(`autorun:${autoId}:main:i-${instance.id}:manual:1`);
    // The instance value wins; unset fields fall back to the automation row.
    expect(snapshot.inputs).toEqual({ channel: "#general", project: "ENG-9" });
    expect(snapshot.instanceId).toBe(instance.id);
    expect(snapshot.instanceKey).toBe("project-ENG-9");

    // An unbound run of the same automation sees the row values untouched.
    await store.insertRun({
      id: `autorun:${autoId}:manual:2`,
      automationId: autoId,
      version: 1,
      trigger: MANUAL_TRIGGER,
      deliveryKey: "manual:2",
      concurrencyKey: null,
      scheduledFor: null,
    });
    const unbound = await engine.loadSnapshot(`autorun:${autoId}:manual:2`);
    expect(unbound.inputs).toEqual({ channel: "#general", project: "row-default" });
    expect(unbound.instanceId).toBeUndefined();
  });

  test("adoptSession never crosses workstreams", async () => {
    const autoId = await seedAutomation("adopt");
    const instances = makeAutomationInstanceStore();
    const a = await instances.openInstance({
      automationId: autoId, key: "a", inputs: {}, openedBy: "",
    });
    const b = await instances.openInstance({
      automationId: autoId, key: "b", inputs: {}, openedBy: "",
    });
    const store = makeAutomationStore();
    const engine = makeAutomationEngineStore();
    // The owner run (instance a) is terminal; its session is adoptable —
    // but only by another instance-a run.
    await store.insertRun({
      id: `autorun:${autoId}:main:i-${a.id}:owner`,
      automationId: autoId,
      version: 1,
      instanceId: a.id,
      trigger: MANUAL_TRIGGER,
      deliveryKey: "owner",
      concurrencyKey: null,
      scheduledFor: null,
      status: "completed",
    });
    for (const [id, instanceId] of [
      [`autorun:${autoId}:main:i-${a.id}:next`, a.id],
      [`autorun:${autoId}:main:i-${b.id}:thief`, b.id],
    ] as const) {
      await store.insertRun({
        id,
        automationId: autoId,
        version: 1,
        instanceId,
        trigger: MANUAL_TRIGGER,
        deliveryKey: id,
        concurrencyKey: null,
        scheduledFor: null,
      });
    }
    const sessionId = `sess-${UNIQ}-adopt`;
    await getDb().insert(automationSessionTable).values({
      sessionId,
      runId: `autorun:${autoId}:main:i-${a.id}:owner`,
      blockId: "create",
      keep: true,
    });

    const thief = await engine.adoptSession({
      runId: `autorun:${autoId}:main:i-${b.id}:thief`,
      automationId: autoId,
      sessionId,
      instanceId: b.id,
    });
    expect(thief).toBe("foreign");

    const heir = await engine.adoptSession({
      runId: `autorun:${autoId}:main:i-${a.id}:next`,
      automationId: autoId,
      sessionId,
      instanceId: a.id,
    });
    expect(heir).toBe("adopted");

    const binding = await engine.getSessionBinding(sessionId);
    expect(binding).toMatchObject({
      runId: `autorun:${autoId}:main:i-${a.id}:next`,
      instanceId: a.id,
    });
  });

  test("the drops ring records and caps", async () => {
    const autoId = await seedAutomation("drops");
    const store = makeAutomationInstanceStore();
    for (let i = 0; i < 55; i++) {
      await store.recordDrop({
        automationId: autoId,
        entrypointId: "main",
        eventKey: "message",
        reason: "no_handle_match",
        detail: `d-${i}`,
      });
    }
    const drops = await store.listRecentDrops(autoId);
    expect(drops.length).toBe(50);
    expect(drops[0]).toMatchObject({ detail: "d-54", reason: "no_handle_match" });
    expect(drops.at(-1)).toMatchObject({ detail: "d-5" });
  });
});
