import { expect, test, describe } from "bun:test";
import { checkDb, getDb } from "../db/client.ts";
import { isUniqueViolation } from "../db/pg-errors.ts";
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
  integrationGrants: [],
  network: { default: "deny" as const, allowHosts: [], allowHostPatterns: [] },
  secrets: [],
  repos: [],
  portExposures: [],
};

describe("ProfileStore", () => {
  test.skipIf(!dbReachable)("setDesignation sets, transfers, and clears a designation", async () => {
    const store = makeProfileStore(getDb());
    const designation = "pr_reviewer";
    const previous = await store.getByDesignation(designation);
    const a = await store.create({ ...baseInput, name: `Reviewer A ${Date.now()}` });
    const b = await store.create({ ...baseInput, name: `Reviewer B ${Date.now()}` });
    try {
      await store.setDesignation(a.id, designation);
      expect((await store.get(a.id))?.designation).toBe(designation);
      expect((await store.getByDesignation(designation))?.id).toBe(a.id);

      await store.setDesignation(b.id, designation);
      expect((await store.get(a.id))?.designation).toBeNull();
      expect((await store.get(b.id))?.designation).toBe(designation);
      expect((await store.getByDesignation(designation))?.id).toBe(b.id);

      await store.setDesignation(b.id, null);
      expect((await store.get(b.id))?.designation).toBeNull();
      expect(await store.getByDesignation(designation)).toBeNull();
    } finally {
      await store.setDesignation(a.id, null).catch(() => {});
      await store.setDesignation(b.id, null).catch(() => {});
      await getDb()
        .delete(profileTable)
        .where(inArray(profileTable.id, [a.id, b.id]))
        .catch(() => {});
      if (previous) await store.setDesignation(previous.id, designation).catch(() => {});
    }
  });

  test.skipIf(!dbReachable)("designation lookup, preservation, and uniqueness", async () => {
    const store = makeProfileStore(getDb());
    const designation = "pr_reviewer";
    const createdIds: string[] = [];
    try {
      const created = await store.create(
        { ...baseInput, name: `Reviewer ${Date.now()}` },
        designation,
      );
      createdIds.push(created.id);

      expect(created.designation).toBe(designation);
      expect((await store.getByDesignation(designation))?.id).toBe(created.id);
      expect(await store.getByDesignation("unused_designation")).toBeNull();

      const updated = await store.update(created.id, {
        ...baseInput,
        name: "Updated Reviewer",
      });
      expect(updated?.designation).toBe(designation);

      await store.softDelete(created.id);
      expect(await store.getByDesignation(designation)).toBeNull();

      const duplicate = await store
        .create({ ...baseInput, name: `Duplicate Reviewer ${Date.now()}` }, designation)
        .then(() => null)
        .catch((e: unknown) => e);
      expect(duplicate).not.toBeNull();
      // Drizzle wraps the driver error inside a transaction; the predicate the
      // seed relies on must still recognize the unique violation.
      expect(isUniqueViolation(duplicate)).toBe(true);
    } finally {
      if (createdIds.length > 0) {
        await getDb()
          .delete(profileTable)
          .where(inArray(profileTable.id, createdIds))
          .catch(() => {});
      }
    }
  });

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
      integrationGrants: [
        { connectionId: "connection-github", operation: "issues:write", resourceConstraints: [] },
        { connectionId: "connection-datadog", operation: "metrics:read", resourceConstraints: [] },
      ],
      network: { default: "deny" as const, allowHosts: [], allowHostPatterns: [] },
      secrets: [],
      repos: [],
      portExposures: [3000, 8080],
    };
    const created = await store.create(input);
    try {
      expect(created.id).toBeDefined();
      expect(created.deletedAt).toBeNull();
      expect(created.integrationGrants).toEqual(input.integrationGrants);
      // ADR 0064: port_exposures round-trip through the store.
      expect(created.portExposures).toEqual([3000, 8080]);

      const active = await store.getActive(created.id);
      expect(active?.name).toBe(input.name);
      expect(active?.portExposures).toEqual([3000, 8080]);

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

});
