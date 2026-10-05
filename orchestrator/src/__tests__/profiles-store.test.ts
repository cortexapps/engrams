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
  apps: [],
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
      integrationGrants: [
        { connectionId: "connection-github", operation: "issues:write", resourceConstraints: [] },
        { connectionId: "connection-datadog", operation: "metrics:read", resourceConstraints: [] },
      ],
      network: { default: "deny" as const, allowHosts: [], allowHostPatterns: [] },
      secrets: [],
      repos: [],
      apps: [{ name: "web", port: 3000 }, { name: "api", port: 8080 }],
    };
    const created = await store.create(input);
    try {
      expect(created.id).toBeDefined();
      expect(created.deletedAt).toBeNull();
      expect(created.integrationGrants).toEqual(input.integrationGrants);
      // ADR 0118: apps round-trip through the store.
      expect(created.apps).toEqual([
        { name: "web", port: 3000 },
        { name: "api", port: 8080 },
      ]);

      const active = await store.getActive(created.id);
      expect(active?.name).toBe(input.name);
      expect(active?.apps).toEqual([
        { name: "web", port: 3000 },
        { name: "api", port: 8080 },
      ]);

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
