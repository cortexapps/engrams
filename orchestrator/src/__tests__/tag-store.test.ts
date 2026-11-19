/**
 * Live-PG tests for the tag store: tag CRUD with names unique without regard
 * to case, the per-automation tag limit under concurrent adds, and link
 * cleanup by foreign key. Gated on ORCHESTRATOR_DATABASE_URL + a reachability
 * probe, per the db.test.ts convention; every row carries a per-run suffix so
 * the suite is parallel-safe against the shared migrated database.
 */

import { afterAll, describe, expect, test } from "bun:test";
import { eq, inArray, sql } from "drizzle-orm";

import { ENGINE_VERSION } from "../automations/engine/definition.ts";
import { checkDb, getDb } from "../db/client.ts";
import { makeAutomationStore } from "../db/automations.ts";
import { automation as automationTable, tag as tagTable } from "../db/schema.ts";
import { makeTagStore, type TagRow } from "../db/tags.ts";

const DB_URL = process.env["ORCHESTRATOR_DATABASE_URL"];
const dbReachable = DB_URL ? await checkDb() : false;

const UNIQ = `${Date.now()}-${Math.floor(Math.random() * 1e6)}`;
let sequence = 0;
const createdAutomationIds: string[] = [];
const createdTagIds: string[] = [];

/** A suffix unique to one test, so names never collide across tests or runs. */
function suffix(): string {
  return `${UNIQ}-${++sequence}`;
}

async function seedAutomation(): Promise<string> {
  const id = `tag-store-auto-${suffix()}`;
  await getDb().insert(automationTable).values({ id, name: id, description: "", enabled: true, currentVersion: 1 });
  createdAutomationIds.push(id);
  return id;
}

async function archiveAutomation(id: string): Promise<void> {
  await getDb().update(automationTable).set({ archivedAt: new Date() }).where(eq(automationTable.id, id));
}

async function newTag(name: string): Promise<TagRow> {
  const result = await makeTagStore().create({ name, color: "#1f6feb", description: "" });
  if (!("tag" in result)) throw new Error(`create of "${name}" was refused`);
  createdTagIds.push(result.tag.id);
  return result.tag;
}

afterAll(async () => {
  if (!dbReachable) return;
  // Links cascade with either side.
  if (createdAutomationIds.length > 0) {
    await getDb().delete(automationTable).where(inArray(automationTable.id, createdAutomationIds));
  }
  if (createdTagIds.length > 0) {
    await getDb().delete(tagTable).where(inArray(tagTable.id, createdTagIds));
  }
});

describe.skipIf(!dbReachable)("tag store (live PG)", () => {
  const tags = makeTagStore();

  test("create, rename, recolor and describe a tag", async () => {
    const s = suffix();
    const created = await newTag(`Ops ${s}`);
    expect(created).toMatchObject({ name: `Ops ${s}`, color: "#1f6feb", description: "" });

    const updated = await tags.update(created.id, {
      name: `Platform ${s}`,
      color: "#8250df",
      description: "Owned by platform",
    });
    if (!("tag" in updated)) throw new Error("update was refused");
    expect(updated.tag).toMatchObject({
      id: created.id,
      name: `Platform ${s}`,
      color: "#8250df",
      description: "Owned by platform",
    });
  });

  test("update with an empty patch returns the tag unchanged", async () => {
    const created = await newTag(`Same ${suffix()}`);
    const updated = await tags.update(created.id, {});
    if (!("tag" in updated)) throw new Error("update was refused");
    expect(updated.tag).toMatchObject({ id: created.id, name: created.name, color: created.color });
  });

  test("names are unique without regard to case", async () => {
    const s = suffix();
    await newTag(`Ops ${s}`);
    expect(await tags.create({ name: `ops ${s}`, color: "#1f6feb", description: "" })).toEqual({
      refused: "name_taken",
    });
  });

  test("concurrent creates of one name differing in case: one wins", async () => {
    const s = suffix();
    const results = await Promise.all([
      tags.create({ name: `Dup ${s}`, color: "#1f6feb", description: "" }),
      tags.create({ name: `dup ${s}`, color: "#1f6feb", description: "" }),
    ]);
    for (const r of results) if ("tag" in r) createdTagIds.push(r.tag.id);
    expect(results.filter((r) => "refused" in r)).toEqual([{ refused: "name_taken" }]);
    expect(results.filter((r) => "tag" in r)).toHaveLength(1);
  });

  test("rename changes only the case of its own name", async () => {
    const s = suffix();
    const created = await newTag(`ops ${s}`);
    const updated = await tags.update(created.id, { name: `Ops ${s}` });
    if (!("tag" in updated)) throw new Error("update was refused");
    expect(updated.tag.name).toBe(`Ops ${s}`);
  });

  test("rename to another tag's name is refused; an unknown id is not_found", async () => {
    const s = suffix();
    await newTag(`Billing ${s}`);
    const other = await newTag(`Ops ${s}`);
    expect(await tags.update(other.id, { name: `BILLING ${s}` })).toEqual({ refused: "name_taken" });
    expect(await tags.update(`no-such-tag-${s}`, { color: "#000000" })).toEqual({ refused: "not_found" });
  });

  test("list orders by name without regard to case and counts non-archived automations", async () => {
    const s = suffix();
    const b = await newTag(`b ${s}`);
    const a = await newTag(`A ${s}`);
    const kept = await seedAutomation();
    const archived = await seedAutomation();
    expect(await tags.addToAutomation(kept, a.id, 10)).toBe("added");
    expect(await tags.addToAutomation(archived, a.id, 10)).toBe("added");
    await archiveAutomation(archived);

    const mine = (await tags.list()).filter((summary) => summary.tag.name.endsWith(s));
    expect(mine.map((summary) => [summary.tag.name, summary.automationCount])).toEqual([
      [`A ${s}`, 1],
      [`b ${s}`, 0],
    ]);
    expect(mine[1]?.tag.id).toBe(b.id);
  });

  test("addToAutomation is idempotent and refuses past the limit", async () => {
    const s = suffix();
    const [t1, t2, t3] = [await newTag(`t1 ${s}`), await newTag(`t2 ${s}`), await newTag(`t3 ${s}`)];
    const a = await seedAutomation();
    expect(await tags.addToAutomation(a, t1.id, 2)).toBe("added");
    expect(await tags.addToAutomation(a, t1.id, 2)).toBe("added");
    expect(await tags.addToAutomation(a, t2.id, 2)).toBe("added");
    expect(await tags.addToAutomation(a, t3.id, 2)).toBe("full");
    // A tag the automation already has is not a new one, even at the limit.
    expect(await tags.addToAutomation(a, t1.id, 2)).toBe("added");
    expect((await tags.tagsForAutomations([a])).get(a)?.map((t) => t.name)).toEqual([`t1 ${s}`, `t2 ${s}`]);
  });

  test("concurrent adds never pass the limit", async () => {
    const s = suffix();
    const ids = [(await newTag(`c1 ${s}`)).id, (await newTag(`c2 ${s}`)).id, (await newTag(`c3 ${s}`)).id];
    const a = await seedAutomation();
    const outcomes = await Promise.all(ids.map((id) => tags.addToAutomation(a, id, 2)));
    expect(outcomes.filter((o) => o === "added")).toHaveLength(2);
    expect(outcomes.filter((o) => o === "full")).toHaveLength(1);
    expect((await tags.tagsForAutomations([a])).get(a)).toHaveLength(2);
  });

  test("a tag add does not block writes that only reference the automation", async () => {
    const s = suffix();
    const t = await newTag(`lock ${s}`);
    const a = await seedAutomation();
    // FOR KEY SHARE is the lock a foreign-key check takes, e.g. when a run for
    // this automation is inserted. Hold it while the tag add runs.
    await getDb().transaction(async (tx) => {
      await tx.execute(sql`select 1 from automation where id = ${a} for key share`);
      const outcome = await Promise.race([
        tags.addToAutomation(a, t.id, 10),
        new Promise((resolve) => setTimeout(() => resolve("blocked"), 2000)),
      ]);
      expect(outcome).toBe("added");
    });
  });

  test("link writes refuse an archived or unknown automation and an unknown tag", async () => {
    const s = suffix();
    const t = await newTag(`x ${s}`);
    const archived = await seedAutomation();
    await archiveAutomation(archived);
    expect(await tags.addToAutomation(archived, t.id, 10)).toBe("automation_missing");
    expect(await tags.removeFromAutomation(archived, t.id)).toBe("automation_missing");
    expect(await tags.addToAutomation(`no-such-automation-${s}`, t.id, 10)).toBe("automation_missing");

    const live = await seedAutomation();
    expect(await tags.addToAutomation(live, `no-such-tag-${s}`, 10)).toBe("tag_missing");
  });

  test("removeFromAutomation removes one link and ignores an absent one", async () => {
    const s = suffix();
    const [t1, t2] = [await newTag(`r1 ${s}`), await newTag(`r2 ${s}`)];
    const a = await seedAutomation();
    await tags.addToAutomation(a, t1.id, 10);
    await tags.addToAutomation(a, t2.id, 10);
    expect(await tags.removeFromAutomation(a, t1.id)).toBe("removed");
    expect(await tags.removeFromAutomation(a, t1.id)).toBe("removed");
    expect((await tags.tagsForAutomations([a])).get(a)?.map((t) => t.id)).toEqual([t2.id]);
  });

  test("delete removes the tag from every automation", async () => {
    const s = suffix();
    const t = await newTag(`gone ${s}`);
    const [a, b] = [await seedAutomation(), await seedAutomation()];
    await tags.addToAutomation(a, t.id, 10);
    await tags.addToAutomation(b, t.id, 10);
    expect(await tags.delete(t.id)).toBe(true);
    const after = await tags.tagsForAutomations([a, b]);
    expect(after.has(a)).toBe(false);
    expect(after.has(b)).toBe(false);
    expect(await tags.delete(t.id)).toBe(false);
  });

  test("deleting an automation removes its links", async () => {
    const s = suffix();
    const t = await newTag(`kept ${s}`);
    const a = await seedAutomation();
    await tags.addToAutomation(a, t.id, 10);
    await getDb().delete(automationTable).where(eq(automationTable.id, a));
    const summary = (await tags.list()).find((row) => row.tag.id === t.id);
    expect(summary?.automationCount).toBe(0);
  });

  test("tagsForAutomations returns each automation's tags ordered by name", async () => {
    const s = suffix();
    const [c, a, b] = [await newTag(`c ${s}`), await newTag(`a ${s}`), await newTag(`B ${s}`)];
    const [first, second, untagged] = [await seedAutomation(), await seedAutomation(), await seedAutomation()];
    for (const t of [c, a, b]) await tags.addToAutomation(first, t.id, 10);
    await tags.addToAutomation(second, c.id, 10);

    const byAutomation = await tags.tagsForAutomations([first, second, untagged]);
    expect(byAutomation.get(first)?.map((t) => t.name)).toEqual([`a ${s}`, `B ${s}`, `c ${s}`]);
    expect(byAutomation.get(second)?.map((t) => t.name)).toEqual([`c ${s}`]);
    expect(byAutomation.has(untagged)).toBe(false);
    expect((await tags.tagsForAutomations([])).size).toBe(0);
  });

  test("automation create with tagIds links the tags in the same write", async () => {
    const s = suffix();
    const [t1, t2] = [await newTag(`k1 ${s}`), await newTag(`k2 ${s}`)];
    const created = await makeAutomationStore().create(
      {
        name: `Tagged ${s}`,
        description: "",
        enabled: false,
        definition: {
          engine: ENGINE_VERSION,
          trigger: { kind: "manual" },
          blocks: [],
          inputsSchema: [],
          settings: { endSessionsOnFinish: false },
        },
        nextFireAt: null,
        tagIds: [t1.id, t2.id],
      },
      null,
    );
    createdAutomationIds.push(created.id);
    expect((await tags.tagsForAutomations([created.id])).get(created.id)?.map((t) => t.id)).toEqual([t1.id, t2.id]);
  });
});
