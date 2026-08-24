/**
 * Live-PG tests for the automation state store (ADR 0119 D10): version
 * bumps, CAS with the writer-tag replay amnesty, create-only, idempotent
 * delete, the caps, and prefix listing with LIKE escaping. Gated on
 * ORCHESTRATOR_DATABASE_URL + a reachability probe, per the db.test.ts
 * convention; rows use unique ids so the suite is parallel-safe against the
 * shared migrated database.
 */

import { afterAll, describe, expect, test } from "bun:test";
import { inArray } from "drizzle-orm";

import { checkDb, getDb } from "../db/client.ts";
import {
  makeAutomationStateStore,
  STATE_KEY_MAX_CHARS,
  STATE_VALUE_MAX_BYTES,
} from "../db/automation-state.ts";
import { StateLimitError } from "../automations/engine/deps.ts";
import { automation as automationTable } from "../db/schema.ts";

const DB_URL = process.env["ORCHESTRATOR_DATABASE_URL"];
const dbReachable = DB_URL ? await checkDb() : false;

const UNIQ = `${Date.now()}-${Math.floor(Math.random() * 1e6)}`;
const createdAutomationIds: string[] = [];

async function seedAutomation(suffix: string): Promise<string> {
  const id = `state-store-auto-${UNIQ}-${suffix}`;
  await getDb().insert(automationTable).values({
    id,
    name: `State store test ${id}`,
    description: "",
    enabled: true,
    currentVersion: 1,
  });
  createdAutomationIds.push(id);
  return id;
}

afterAll(async () => {
  if (!dbReachable || createdAutomationIds.length === 0) return;
  // automation_state rows cascade with the automation.
  await getDb().delete(automationTable).where(inArray(automationTable.id, createdAutomationIds));
});

describe.skipIf(!dbReachable)("automation state store (live PG)", () => {
  test("upsert inserts at version 1, updates bump, and the writer stamps", async () => {
    const autoId = await seedAutomation("upsert");
    const store = makeAutomationStateStore();

    const first = await store.set(autoId, "ticket:ENG-1", { pr: null }, { writer: "run-a:save" });
    expect(first).toEqual({ ok: true, version: 1 });

    const second = await store.set(autoId, "ticket:ENG-1", { pr: 7 }, { writer: "run-b:save" });
    expect(second).toEqual({ ok: true, version: 2 });

    const entry = await store.get(autoId, "ticket:ENG-1");
    expect(entry).toMatchObject({ value: { pr: 7 }, version: 2, writer: "run-b:save" });
  });

  test("CAS: hit bumps, miss reports current, and a replay of our own write is amnestied", async () => {
    const autoId = await seedAutomation("cas");
    const store = makeAutomationStateStore();
    await store.set(autoId, "k", { n: 1 }, { writer: "w0" });

    const hit = await store.set(autoId, "k", { n: 2 }, { writer: "run-a:step", expectVersion: 1 });
    expect(hit).toEqual({ ok: true, version: 2 });

    // A crash between the write and its checkpoint re-executes the same
    // step: same expectVersion, same writer. That must NOT be a conflict.
    const replay = await store.set(autoId, "k", { n: 2 }, { writer: "run-a:step", expectVersion: 1 });
    expect(replay).toEqual({ ok: true, version: 2 });

    // A genuinely different writer at the stale version loses.
    const lost = await store.set(autoId, "k", { n: 9 }, { writer: "run-b:step", expectVersion: 1 });
    expect(lost.ok).toBe(false);
    if (!lost.ok) expect(lost.current).toMatchObject({ version: 2, value: { n: 2 } });
    const entry = await store.get(autoId, "k");
    expect(entry?.value).toEqual({ n: 2 });
  });

  test("expectVersion 0 is create-only, with the same replay amnesty", async () => {
    const autoId = await seedAutomation("create");
    const store = makeAutomationStateStore();

    const created = await store.set(autoId, "k", 1, { writer: "run-a:init", expectVersion: 0 });
    expect(created).toEqual({ ok: true, version: 1 });

    const replay = await store.set(autoId, "k", 1, { writer: "run-a:init", expectVersion: 0 });
    expect(replay).toEqual({ ok: true, version: 1 });

    const other = await store.set(autoId, "k", 2, { writer: "run-b:init", expectVersion: 0 });
    expect(other.ok).toBe(false);
  });

  test("delete is idempotent and its CAS reports the current version", async () => {
    const autoId = await seedAutomation("delete");
    const store = makeAutomationStateStore();
    await store.set(autoId, "k", 1, { writer: "w" });
    await store.set(autoId, "k", 2, { writer: "w" }); // version 2

    const wrong = await store.delete(autoId, "k", { expectVersion: 1 });
    expect(wrong.ok).toBe(false);
    if (!wrong.ok) expect(wrong.current.version).toBe(2);

    const right = await store.delete(autoId, "k", { expectVersion: 2 });
    expect(right).toEqual({ ok: true, deleted: true });

    // Replay of the conditional delete (row already gone) and an
    // unconditional delete of nothing are both successes.
    expect(await store.delete(autoId, "k", { expectVersion: 2 })).toEqual({ ok: true, deleted: false });
    expect(await store.delete(autoId, "k", {})).toEqual({ ok: true, deleted: false });
  });

  test("the caps are typed errors: key length, value size, automation capacity", async () => {
    const autoId = await seedAutomation("caps");
    const store = makeAutomationStateStore({ maxKeys: 2 });

    const longKey = "k".repeat(STATE_KEY_MAX_CHARS + 1);
    await expect(store.set(autoId, longKey, 1, { writer: "w" })).rejects.toThrow(StateLimitError);

    const bigValue = "x".repeat(STATE_VALUE_MAX_BYTES);
    await expect(store.set(autoId, "big", bigValue, { writer: "w" })).rejects.toThrow(
      StateLimitError,
    );

    await store.set(autoId, "a", 1, { writer: "w" });
    await store.set(autoId, "b", 2, { writer: "w" });
    await expect(store.set(autoId, "c", 3, { writer: "w" })).rejects.toThrow(StateLimitError);
    // Updating an EXISTING key is never capacity-limited.
    expect((await store.set(autoId, "a", 9, { writer: "w" })).ok).toBe(true);
  });

  test("list orders by key, honors the limit with a truncation flag, and escapes LIKE wildcards", async () => {
    const autoId = await seedAutomation("list");
    const store = makeAutomationStateStore();
    await store.set(autoId, "ticket:B", 2, { writer: "w" });
    await store.set(autoId, "ticket:A", 1, { writer: "w" });
    await store.set(autoId, "ticketX", 3, { writer: "w" });
    await store.set(autoId, "pr:1", 4, { writer: "w" });
    await store.set(autoId, "a_b", 5, { writer: "w" });
    await store.set(autoId, "axb", 6, { writer: "w" });

    const tickets = await store.list(autoId, { prefix: "ticket:" });
    expect(tickets.entries.map((e) => e.key)).toEqual(["ticket:A", "ticket:B"]);
    expect(tickets.truncated).toBe(false);

    const limited = await store.list(autoId, { prefix: "ticket", limit: 2 });
    expect(limited.entries.map((e) => e.key)).toEqual(["ticket:A", "ticket:B"]);
    expect(limited.truncated).toBe(true);

    // `_` is a LIKE wildcard; a literal prefix must not treat it as one.
    const underscore = await store.list(autoId, { prefix: "a_" });
    expect(underscore.entries.map((e) => e.key)).toEqual(["a_b"]);

    const all = await store.list(autoId);
    expect(all.entries.length).toBe(6);
  });
});
