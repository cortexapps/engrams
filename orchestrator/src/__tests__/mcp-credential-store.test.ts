import { expect, test } from "bun:test";
import { makeIntegrationConnectionStore } from "../db/integration-connections.ts";
import { makeCredentialMetadataStore } from "../mcp/credential-store.ts";
import { getDb, checkDb } from "../db/client.ts";
import { makeMcpRepository } from "../mcp/repository.ts";
const reachable = process.env.ORCHESTRATOR_DATABASE_URL ? await checkDb() : false;
const config = {
  endpoint: "https://tools.example.test/mcp",
  zone: "public",
  auth: { kind: "api_key", secretRef: "" },
};
test.skipIf(!reachable)(
  "credential transitions serialize across replicas and publish exact target revision",
  async () => {
    const connections = makeIntegrationConnectionStore();
    const row = await connections.create({
      alias: `mcp-${crypto.randomUUID()}`,
      displayName: "Tools",
      provider: "mcp",
      config,
    });
    const a = makeCredentialMetadataStore(),
      b = makeCredentialMetadataStore();
    try {
      const now = new Date();
      const results = await Promise.allSettled(
        [a, b].map((s) => s.claim(row.id, 1, 1, "admin", config, now)),
      );
      expect(results.filter((r) => r.status === "fulfilled")).toHaveLength(1);
      const won = results.find((r) => r.status === "fulfilled");
      if (!won || won.status !== "fulfilled") throw new Error("claim");
      const claim = won.value;
      const stale = (await connections.get(row.id))!;
      expect(stale.mcp!.revoked).toBe(true);
      await expect(
        makeMcpRepository().updateConfig(
          row.id,
          stale.mcp!.configRevision,
          { alias: row.alias, displayName: "Tools", config },
          now,
        ),
      ).rejects.toThrow("stale");
      const status = {
        protocol: 1,
        context: {
          connectionId: row.id,
          configRevision: String(claim.revision),
          authorizationGeneration: String(claim.generation + 1),
          credentialIdentity: "identity-new",
          resource: { zoneId: "public", endpoint: config.endpoint },
          policyDigest: "a".repeat(64),
          policyRevision: "v1",
        },
        reference: {
          kind: "CREDENTIAL_KIND_API_KEY",
          name: "mcp.fresh",
          connectionId: row.id,
          generation: "1",
        },
        version: "1",
        status: "connected",
      };
      await expect(
        a.publish(
          claim,
          { ...config, auth: { kind: "api_key", secretRef: "mcp.fresh" } },
          { ...status, reference: { ...status.reference, kind: "CREDENTIAL_KIND_CLIENT_SECRET" } },
          now,
        ),
      ).rejects.toThrow("receipt");
      await a.publish(
        claim,
        { ...config, auth: { kind: "api_key", secretRef: "mcp.fresh" } },
        status,
        now,
      );
      const live = (await connections.get(row.id))!;
      expect(live.mcp!.configRevision).toBe(claim.revision);
      expect(live.mcp!.authGeneration).toBe(claim.generation + 1);
      expect(live.mcp!.credentialIdentity).toBe("identity-new");
      expect(live.mcp!.revoked).toBe(false);
      await expect(b.publish(claim, config, status, now)).rejects.toThrow("stale");
    } finally {
      await connections.delete(row.id);
    }
  },
);
test.skipIf(!reachable)(
  "recovery replaces durable token and blocks late publication from the old owner",
  async () => {
    const connections = makeIntegrationConnectionStore(),
      a = makeCredentialMetadataStore(),
      b = makeCredentialMetadataStore();
    const row = await connections.create({
      alias: `mcp-${crypto.randomUUID()}`,
      provider: "mcp",
      displayName: "Tools",
      config,
    });
    try {
      const now = new Date();
      const old = await a.claim(row.id, 1, 1, "admin", config, now);
      await expect(
        b.recover(row.id, old.revision, old.generation, "admin", new Date(now.getTime() + 34999)),
      ).rejects.toThrow();
      const recovered = await b.recover(
        row.id,
        old.revision,
        old.generation,
        "admin",
        new Date(now.getTime() + 35000),
      );
      await expect(a.publish(old, config, undefined, now)).rejects.toThrow("stale");
      await b.publish(recovered, config, undefined, now);
      const next = await a.claim(row.id, old.revision, old.generation, "admin", config, now);
      await expect(b.stage(recovered, {}, now)).rejects.toThrow("stale");
      await a.publish(next, config, undefined, now);
      expect((await a.get(row.id)).revoked).toBe(true);
    } finally {
      await connections.delete(row.id);
    }
  },
);
