import { unwrapCredentialControlClient } from "../mcp/control-client.ts";
import { expect, test } from "bun:test";
import { Code, ConnectError, createClient, createRouterTransport } from "@connectrpc/connect";
import { create, toJson } from "@bufbuild/protobuf";
import { checkDb, getDb } from "../db/client.ts";
import { eq, sql } from "drizzle-orm";
import { integrationConnection, mcpCatalog } from "../db/schema.ts";
import { makeIntegrationConnectionStore } from "../db/integration-connections.ts";
import { makeCredentialMetadataStore } from "../mcp/credential-store.ts";
import { makeCredentialAdmin } from "../mcp/credential-admin.ts";
import {
  CredentialControlService,
  CredentialKind,
  CredentialStatusSchema,
  type CredentialStatus,
} from "../gen/engram/mcp/v1/control_pb.ts";
const reachable = process.env.ORCHESTRATOR_DATABASE_URL ? await checkDb() : false;
const config = {
  endpoint: "https://tools.example.test/mcp",
  zone: "public",
  auth: { kind: "api_key" as const, secretRef: "" },
};
async function fixture() {
  const connections = makeIntegrationConnectionStore(),
    store = makeCredentialMetadataStore();
  const row = await connections.create({
    alias: `mcp-${crypto.randomUUID()}`,
    displayName: "Tools",
    provider: "mcp",
    config,
  });
  let current: CredentialStatus | undefined;
  let failRevoke = false,
    lostRevoke = false;
  const calls: string[] = [];
  const control = unwrapCredentialControlClient(
    createClient(
      CredentialControlService,
      createRouterTransport((router) =>
        router.service(CredentialControlService, {
          async putSecret(req) {
            calls.push("put");
            current = create(CredentialStatusSchema, {
              protocol: 1,
              context: {
                connectionId: req.context!.connectionId,
                configRevision: req.context!.configRevision,
                resource: req.context!.resource,
                authorizationGeneration: req.context!.previousAuthorizationGeneration + 1n,
                credentialIdentity: `identity-${calls.length}`,
                policyDigest: "a".repeat(64),
                policyRevision: "v1",
              },
              reference: {
                kind: req.kind,
                name: req.expected?.name || "mcp.fresh",
                connectionId: row.id,
                generation: (req.expected?.generation ?? 0n) + 1n,
              },
              version: (req.expected?.generation ?? 0n) + 1n,
              status: "connected",
            });
            return { status: current };
          },
          async getStatus() {
            if (!current) throw new Error("absent");
            return { status: current };
          },
          async revoke() {
            calls.push("revoke");
            if (failRevoke) throw new Error("refused");
            current = create(CredentialStatusSchema, {
              ...current!,
              context: {
                ...current!.context!,
                authorizationGeneration: current!.context!.authorizationGeneration + 1n,
                credentialIdentity: "revoked-identity",
              },
              reference: {
                ...current!.reference!,
                generation: current!.reference!.generation + 1n,
              },
              version: current!.version + 1n,
              status: "revoked",
            });
            if (lostRevoke) throw new Error("lost reply");
            return { status: current };
          },
        }),
      ),
    ),
  );
  let now = new Date();
  const admin = makeCredentialAdmin({ store, control, registrations: [], now: () => now });
  return {
    connections,
    store,
    row,
    admin,
    calls,
    advance(milliseconds: number) {
      now = new Date(now.getTime() + milliseconds);
    },
    setFailure(value = true) {
      failRevoke = value;
    },
    setLost() {
      lostRevoke = true;
    },
    async cleanup() {
      await getDb().delete(mcpCatalog).where(eq(mcpCatalog.connectionId, row.id));
      await connections.delete(row.id);
    },
  };
}
test.skipIf(!reachable)(
  "fresh ingestion publishes generated reference at sealed revision and never stores entered value",
  async () => {
    const f = await fixture();
    try {
      await f.admin.putSecret(
        f.row.id,
        1,
        1,
        "admin",
        CredentialKind.API_KEY,
        "x-api-key",
        "entered-secret",
      );
      const row = await f.store.get(f.row.id);
      expect(row.config).toEqual({ ...config, auth: { kind: "api_key", secretRef: "mcp.fresh" } });
      expect(row.configRevision).toBe(2);
      expect(row.authGeneration).toBe(3);
      expect(JSON.stringify(row)).not.toContain("entered-secret");
      await expect(
        f.admin.putSecret(f.row.id, 1, 1, "admin", CredentialKind.API_KEY, "x-api-key", "next"),
      ).rejects.toThrow("stale");
      expect(f.calls).toEqual(["put"]);
    } finally {
      await f.cleanup();
    }
  },
);
test.skipIf(!reachable)(
  "failed revocation prevents replacement and leaves old binding disabled",
  async () => {
    const f = await fixture();
    try {
      await f.admin.putSecret(
        f.row.id,
        1,
        1,
        "admin",
        CredentialKind.API_KEY,
        "x-api-key",
        "first",
      );
      f.setFailure();
      await expect(
        f.admin.putSecret(f.row.id, 2, 3, "admin", CredentialKind.API_KEY, "x-api-key", "second"),
      ).rejects.toThrow();
      expect(f.calls).toEqual(["put", "revoke"]);
      const row = await f.store.get(f.row.id);
      expect(row.revoked).toBe(true);
      expect(row.mcpCredential.transition).toBeDefined();
    } finally {
      await f.cleanup();
    }
  },
);
test.skipIf(!reachable)(
  "lost revoke reply is recovered from receipt without replay before rotation",
  async () => {
    const f = await fixture();
    try {
      await f.admin.putSecret(
        f.row.id,
        1,
        1,
        "admin",
        CredentialKind.API_KEY,
        "x-api-key",
        "first",
      );
      f.setLost();
      await f.admin.putSecret(
        f.row.id,
        2,
        3,
        "admin",
        CredentialKind.API_KEY,
        "x-api-key",
        "second",
      );
      expect(f.calls).toEqual(["put", "revoke", "put"]);
      const row = await f.store.get(f.row.id);
      expect(row.revoked).toBe(false);
      expect(row.authGeneration).toBe(5);
    } finally {
      await f.cleanup();
    }
  },
);

test.skipIf(!reachable)(
  "same-reference rotation changes actual identity and preserves published context",
  async () => {
    const f = await fixture();
    try {
      await f.admin.putSecret(
        f.row.id,
        1,
        1,
        "admin",
        CredentialKind.API_KEY,
        "x-api-key",
        "first",
      );
      const first = await f.store.get(f.row.id);
      await f.admin.putSecret(
        f.row.id,
        2,
        3,
        "admin",
        CredentialKind.API_KEY,
        "x-api-key",
        "second",
      );
      const next = await f.store.get(f.row.id);
      expect(next.config).toEqual(first.config);
      expect(next.credentialIdentity).not.toBe(first.credentialIdentity);
      expect(next.authGeneration).toBe(5);
      expect(next.configRevision).toBe(3);
      expect(f.calls).toEqual(["put", "revoke", "put"]);
    } finally {
      await f.cleanup();
    }
  },
);
test.skipIf(!reachable)(
  "public credential response contains status and references but no entered value",
  async () => {
    const f = await fixture();
    try {
      const { credentialHandlers } = await import("../mcp/credential-rpc.ts");
      const { makeMcpRepository } = await import("../mcp/repository.ts");
      const { makeMcpJournal } = await import("../mcp/journal.ts");
      const { IntegrationService } = await import("../gen/engram/app/v1/integration_pb.ts");
      const client = createClient(
        IntegrationService,
        createRouterTransport((router) =>
          router.service(
            IntegrationService,
            credentialHandlers({
              services: {
                enabled: true,
                credentials: f.admin,
                repository: makeMcpRepository(),
                journal: makeMcpJournal(),
              },
              connections: f.connections,
              getSession: async () => ({ user: { id: "admin", role: "admin" } }),
              connectionToProto: (row) => ({ id: row.id }),
            }),
          ),
        ),
      );
      const response = await client.putMcpCredential({
        connectionId: f.row.id,
        expectedRevision: 1,
        expectedGeneration: 1,
        kind: "api_key",
        header: "x-api-key",
        value: "browser-entered-secret",
      });
      expect(response.credential!.reference).toBe("mcp.fresh");
      expect(response.credential!.status).toBe("connected");
      expect(response.credential).not.toHaveProperty("value");
      expect(response.credential).not.toHaveProperty("header");
      const setup = await client.getMcpSetup({ connectionId: f.row.id });
      expect(setup.current!.credential!.reference).toBe("mcp.fresh");
      expect(JSON.stringify(await f.store.get(f.row.id))).not.toContain("browser-entered-secret");
    } finally {
      await f.cleanup();
    }
  },
);
test.skipIf(!reachable)(
  "named connection CRUD admits an unconfigured draft and refuses browser reference injection",
  async () => {
    const f = await fixture();
    let createdId: string | undefined;
    try {
      const { registerIntegration } = await import("../rpc/integration.ts");
      const { makeMcpRepository } = await import("../mcp/repository.ts");
      const { makeMcpJournal } = await import("../mcp/journal.ts");
      const { IntegrationService } = await import("../gen/engram/app/v1/integration_pb.ts");
      const client = createClient(
        IntegrationService,
        createRouterTransport((router) =>
          registerIntegration(router, {
            getSession: async () => ({ user: { id: "admin", role: "admin" } }),
            connections: f.connections,
            mcp: {
              enabled: true,
              credentials: f.admin,
              repository: makeMcpRepository(),
              journal: makeMcpJournal(),
              validateDestination: async () => {},
            },
          }),
        ),
      );
      const input = {
        alias: `mcp-${crypto.randomUUID()}`,
        displayName: "Tools",
        provider: "mcp",
        mcp: { endpoint: config.endpoint, zone: config.zone, authKind: "api_key" },
      };
      await expect(
        client.createConnection({ ...input, mcp: { ...input.mcp, apiKeySecretRef: "mcp.stolen" } }),
      ).rejects.toThrow("fresh credential");
      const created = await client.createConnection(input);
      createdId = created.connection!.id;
      expect(created.connection!.enabled).toBe(false);
      expect(created.connection!.credentialIdentity).toBe("");
      expect(created.connection!.mcp!.apiKeySecretRef).toBe("");
      await expect(
        client.updateConnection({
          id: createdId,
          alias: input.alias,
          displayName: "Tools",
          expectedRevision: 1,
          expectedGeneration: 1,
          mcp: { ...input.mcp, apiKeySecretRef: "mcp.stolen" },
        }),
      ).rejects.toThrow("references cannot");
      expect((await f.store.get(createdId)).configRevision).toBe(1);
      await expect(
        client.discoverMcp({ connectionId: createdId, expectedRevision: 1 }),
      ).rejects.toThrow();
    } finally {
      if (createdId) await f.connections.delete(createdId);
      await f.cleanup();
    }
  },
);

test.skipIf(!reachable)(
  "disabled dispatch preserves registered credential recovery and disconnect with exact admin context",
  async () => {
    const f = await fixture();
    try {
      const { credentialHandlers } = await import("../mcp/credential-rpc.ts");
      const { makeMcpRepository } = await import("../mcp/repository.ts");
      const { makeMcpJournal } = await import("../mcp/journal.ts");
      const { IntegrationService, GetMcpSetupResponseSchema } =
        await import("../gen/engram/app/v1/integration_pb.ts");
      await f.admin.putSecret(
        f.row.id,
        1,
        1,
        "admin",
        CredentialKind.API_KEY,
        "x-api-key",
        "entered-secret",
      );
      f.setFailure();
      await expect(f.admin.disconnect(f.row.id, 2, 3, "admin")).rejects.toThrow();
      f.advance(35001);
      const pending = await f.store.get(f.row.id);
      const services = {
        enabled: false,
        credentials: f.admin,
        repository: makeMcpRepository(),
        journal: makeMcpJournal(),
      };
      let email = "admin@example.test";
      const client = createClient(
        IntegrationService,
        createRouterTransport((router) =>
          router.service(
            IntegrationService,
            credentialHandlers({
              services,
              connections: f.connections,
              getSession: async () => ({ user: { id: "admin", role: "admin", email } }),
              connectionToProto: (row) => ({ id: row.id }),
            }),
          ),
        ),
      );
      const setup = await client.getMcpSetup({ connectionId: f.row.id });
      expect(setup.enabled).toBe(false);
      expect(setup.recoveryAvailable).toBe(true);
      expect(setup.transitionPending).toBe(true);
      expect(JSON.stringify(toJson(GetMcpSetupResponseSchema, setup))).not.toContain(
        "entered-secret",
      );
      const request = {
        connectionId: f.row.id,
        expectedRevision: pending.configRevision,
        expectedGeneration: pending.authGeneration,
      };
      await expect(
        client.putMcpCredential({
          ...request,
          kind: "api_key",
          header: "x-api-key",
          value: "forbidden",
        }),
      ).rejects.toThrow("disabled");
      await expect(client.authorizeMcp(request)).rejects.toThrow("disabled");
      await expect(client.completeMcpAuthorization(request)).rejects.toThrow("disabled");
      email = "apikey+agent@service.local";
      await expect(client.recoverMcpCredential(request)).rejects.toThrow("human administrator");
      await expect(client.disconnectMcp(request)).rejects.toThrow("human administrator");
      email = "admin@example.test";
      await expect(
        client.recoverMcpCredential({
          ...request,
          expectedGeneration: request.expectedGeneration + 1,
        }),
      ).rejects.toThrow("state changed");
      await expect(client.recoverMcpCredential(request)).rejects.toThrow("state changed");
      expect((await f.store.get(f.row.id)).mcpCredential.transition).toBeDefined();
      f.setFailure(false);
      f.advance(35001);
      const recovered = await client.recoverMcpCredential(request);
      expect(recovered.credential!.status).toBe("disconnected");
      const ready = await f.store.get(f.row.id);
      expect(ready.mcpCredential.transition).toBeUndefined();
      await client.disconnectMcp({
        connectionId: f.row.id,
        expectedRevision: ready.configRevision,
        expectedGeneration: ready.authGeneration,
      });
      expect(f.calls.filter((x) => x === "put")).toHaveLength(1);
    } finally {
      await f.cleanup();
    }
  },
);

for (const discovered of [true, false]) {
  test.skipIf(!reachable)(
    discovered
      ? "public removal preserves a discovered connection and its credential authority"
      : "public removal deletes an unused connection after credential revocation",
    async () => {
      const f = await fixture();
      try {
        const { registerIntegration } = await import("../rpc/integration.ts");
        const { makeMcpRepository } = await import("../mcp/repository.ts");
        const { makeMcpJournal } = await import("../mcp/journal.ts");
        const { IntegrationService } = await import("../gen/engram/app/v1/integration_pb.ts");
        const client = createClient(
          IntegrationService,
          createRouterTransport((router) =>
            registerIntegration(router, {
              getSession: async () => ({ user: { id: "admin", role: "admin" } }),
              connections: f.connections,
              mcp: {
                enabled: true,
                credentials: f.admin,
                repository: makeMcpRepository(),
                journal: makeMcpJournal(),
                discovery: {
                  discover: async () => [
                    {
                      upstreamName: "Read.Record",
                      description: "Read a record",
                      inputSchema: { type: "object" },
                    },
                  ],
                },
              },
            }),
          ),
        );
        await client.putMcpCredential({
          connectionId: f.row.id,
          expectedRevision: 1,
          expectedGeneration: 1,
          kind: "api_key",
          header: "x-api-key",
          value: "test-secret",
        });
        const before = await f.store.get(f.row.id);
        const catalog = discovered
          ? (await client.discoverMcp({ connectionId: f.row.id, expectedRevision: 2 })).catalog
          : undefined;
        let error: unknown;
        let deleted: boolean | undefined;
        try {
          deleted = (
            await client.deleteConnection({
              id: f.row.id,
              expectedRevision: before.configRevision,
              expectedGeneration: before.authGeneration,
            })
          ).deleted;
        } catch (caught) {
          error = caught;
        }
        if (discovered) {
          // Refusal must precede both durable transition and external revocation.
          expect(await f.store.get(f.row.id)).toEqual(before);
          expect(f.calls).toEqual(["put"]);
          expect((await f.admin.status(f.row.id)).status?.status).toBe("connected");
          expect(error).toBeInstanceOf(ConnectError);
          expect((error as ConnectError).code).toBe(Code.FailedPrecondition);
          expect((error as ConnectError).rawMessage).toContain("disable or disconnect");
          expect((await client.getMcpCatalogs({ connectionId: f.row.id })).draft?.id).toBe(
            catalog!.id,
          );
        } else {
          expect(error).toBeUndefined();
          expect(deleted).toBe(true);
          expect(await f.connections.get(f.row.id)).toBeNull();
          expect(f.calls).toEqual(["put", "revoke"]);
        }
      } finally {
        await f.cleanup();
      }
    },
  );
}

test.skipIf(!reachable)(
  "removal checks history after a concurrent catalog writer commits",
  async () => {
    const f = await fixture();
    const db = getDb();
    const locked = Promise.withResolvers<number>();
    const release = Promise.withResolvers<void>();
    let writer: Promise<void> | undefined;
    let removal: Promise<unknown> | undefined;
    try {
      await f.admin.putSecret(
        f.row.id,
        1,
        1,
        "admin",
        CredentialKind.API_KEY,
        "x-api-key",
        "test-secret",
      );
      const before = await f.store.get(f.row.id);
      writer = db.transaction(async (tx) => {
        await tx
          .select()
          .from(integrationConnection)
          .where(eq(integrationConnection.id, f.row.id))
          .for("update");
        const pid = await tx.execute<{ pid: number }>(sql`SELECT pg_backend_pid() AS pid`);
        locked.resolve(pid.rows[0]!.pid);
        await release.promise;
        // A catalog producer commits its history while it owns the connection lock.
        await tx.insert(mcpCatalog).values({
          id: crypto.randomUUID(),
          connectionId: f.row.id,
          kind: "draft",
          revision: 0,
          configRevision: before.configRevision,
          authGeneration: before.authGeneration,
          credentialIdentity: before.credentialIdentity,
          tools: [],
          fingerprint: "fixture",
          actorId: "admin",
          createdAt: new Date(),
        });
      });
      const blocker = await locked.promise;
      removal = f.admin
        .remove(f.row.id, before.configRevision, before.authGeneration, "admin")
        .then(
          () => undefined,
          (error: unknown) => error,
        );
      const deadline = Date.now() + 1000;
      let waiting = false;
      while (Date.now() < deadline) {
        const result = await db.execute<{ waiting: boolean }>(sql`
        SELECT EXISTS (SELECT 1 FROM pg_stat_activity
          WHERE ${blocker} = ANY(pg_blocking_pids(pid))) AS waiting`);
        if (result.rows[0]?.waiting) {
          waiting = true;
          break;
        }
        await Bun.sleep(5);
      }
      expect(waiting).toBe(true);
      release.resolve();
      await writer;
      expect(await removal).toBeInstanceOf(ConnectError);
      expect(await f.store.get(f.row.id)).toEqual(before);
      expect(f.calls).toEqual(["put"]);
    } finally {
      release.resolve();
      await writer;
      await removal;
      await f.cleanup();
    }
  },
);
