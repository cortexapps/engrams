import { expect, test, describe } from "bun:test";
import { checkDb, getDb } from "../db/client.ts";
import { makeProfileStore } from "../db/profiles.ts";
import { profile as profileTable } from "../db/schema.ts";
import { eq, inArray } from "drizzle-orm";

const DB_URL = process.env["ORCHESTRATOR_DATABASE_URL"];
const dbReachable = DB_URL ? await checkDb() : false;

const baseInput = {
  description: "d",
  icon: "Bot",
  imageId: "img-1",
  harness: "claude",
  model: null,
  effort: null,
  includeUserTokens: false,
  envVars: {},
  skills: [],
  capabilities: [],
  network: { default: "deny" as const, allowHosts: [], allowHostPatterns: [] },
  secrets: [],
  isDefault: false,
};

describe("ProfileStore", () => {
  test.skipIf(!dbReachable)("create → getActive → list → update → softDelete", async () => {
    const store = makeProfileStore(getDb());
    const input = {
      name: `Store Test ${Date.now()}`,
      description: "d",
      icon: "Bot",
      imageId: "img-1",
      harness: "claude",
      model: null,
      effort: null,
      includeUserTokens: false,
      envVars: { ANTHROPIC_MODEL: "claude-opus-4-8" },
      skills: ["skills"],
      capabilities: ["github:issues:write", "datadog:metrics:read"],
      network: { default: "deny" as const, allowHosts: [], allowHostPatterns: [] },
      secrets: [],
      isDefault: false,
    };
    const created = await store.create(input);
    try {
      expect(created.id).toBeDefined();
      expect(created.deletedAt).toBeNull();
      // ADR 0056: capabilities round-trip through the store.
      expect(created.capabilities).toEqual(["github:issues:write", "datadog:metrics:read"]);

      const active = await store.getActive(created.id);
      expect(active?.name).toBe(input.name);

      const listed = await store.list({ includeArchived: false });
      expect(listed.some((p) => p.id === created.id)).toBe(true);

      const updated = await store.update(created.id, { ...input, includeUserTokens: true });
      expect(updated?.includeUserTokens).toBe(true);

      await store.softDelete(created.id);
      expect(await store.getActive(created.id)).toBeNull();
      // Still resolvable via get() (history) and getByIds().
      expect((await store.get(created.id))?.deletedAt).not.toBeNull();
      const byIds = await store.getByIds([created.id]);
      expect(byIds[0]?.deletedAt).not.toBeNull();
      // Archived hidden from default list, shown with includeArchived.
      expect((await store.list({ includeArchived: false })).some((p) => p.id === created.id)).toBe(false);
      expect((await store.list({ includeArchived: true })).some((p) => p.id === created.id)).toBe(true);

      // update on an archived id → null.
      expect(await store.update(created.id, input)).toBeNull();
    } finally {
      await getDb().delete(profileTable).where(eq(profileTable.id, created.id)).catch(() => {});
    }
  });

  // ADR 0060: at-most-one default. Setting a profile default clears the prior;
  // soft-deleting the default leaves none. getDefault() returns the active one.
  test.skipIf(!dbReachable)("is_default: at-most-one + getDefault + clears on soft-delete", async () => {
    const store = makeProfileStore(getDb());
    const a = await store.create({ ...baseInput, name: `Def A ${Date.now()}`, isDefault: true });
    const b = await store.create({ ...baseInput, name: `Def B ${Date.now()}`, isDefault: true });
    try {
      // Creating B as default cleared A — exactly one active default.
      expect((await store.get(a.id))?.isDefault).toBe(false);
      expect((await store.get(b.id))?.isDefault).toBe(true);
      expect((await store.getDefault())?.id).toBe(b.id);

      // Re-promoting A via update flips the default back (and clears B).
      const promoted = await store.update(a.id, { ...baseInput, name: a.name, isDefault: true });
      expect(promoted?.isDefault).toBe(true);
      expect((await store.get(b.id))?.isDefault).toBe(false);
      expect((await store.getDefault())?.id).toBe(a.id);

      // Soft-deleting the default leaves none.
      await store.softDelete(a.id);
      expect(await store.getDefault()).toBeNull();
    } finally {
      await getDb().delete(profileTable).where(inArray(profileTable.id, [a.id, b.id])).catch(() => {});
    }
  });
});
