import { describe, expect, test } from "bun:test";

import { checkDb, getDb } from "../db/client.ts";
import { makeIntegrationConnectionStore } from "../db/integration-connections.ts";

const DB_URL = process.env["ORCHESTRATOR_DATABASE_URL"];
const dbReachable = DB_URL ? await checkDb() : false;

describe("IntegrationConnectionStore", () => {
  test.skipIf(!dbReachable)(
    "creates one opaque default connection per provider",
    async () => {
      const provider = `test-provider-${crypto.randomUUID()}`;
      const store = makeIntegrationConnectionStore(getDb());
      const createdIds: string[] = [];
      try {
        const first = await store.ensureDefault(provider, "Test provider (default)");
        createdIds.push(first.id);
        const second = await store.ensureDefault(provider, "Ignored replacement name");

        expect(first.id).toMatch(
          /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/,
        );
        expect(first.id).not.toContain(provider);
        expect(first.isDefault).toBe(true);
        expect(second.id).toBe(first.id);
        expect((await store.getDefault(provider))?.id).toBe(first.id);

        const named = await store.create({
          alias: `${provider}-named`,
          provider,
          displayName: "Named connection",
          config: {},
        });
        createdIds.push(named.id);
        expect(named.isDefault).toBe(false);
        expect((await store.getDefault(provider))?.id).toBe(first.id);
      } finally {
        await Promise.all(createdIds.map((id) => store.delete(id)));
      }
    },
  );
});
