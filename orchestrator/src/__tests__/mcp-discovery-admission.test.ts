import { expect, test } from "bun:test";
import { createClient, createRouterTransport } from "@connectrpc/connect";
import { unwrapCredentialControlClient } from "../mcp/control-client.ts";
import { create } from "@bufbuild/protobuf";
import { eq } from "drizzle-orm";
import { drizzle } from "drizzle-orm/node-postgres";
import { Pool } from "pg";
import * as schema from "../db/schema.ts";
import { getDb } from "../db/client.ts";
import {
  CredentialControlService,
  CredentialKind,
  CredentialStatusSchema,
} from "../gen/engram/mcp/v1/control_pb.ts";
import { statusMetadata } from "../mcp/credential-store.ts";
import { makeMcpDiscoveryAdmission } from "../mcp/discovery-admission.ts";
import type { AdmittedCredentialOperation } from "../mcp/credential-broker.ts";
import { makeCredentialBroker } from "../mcp/credential-broker.ts";
import { canonicalDigest } from "../mcp/hash.ts";
import { verifyMcpTestDatabases } from "./helpers/mcp-test-isolation.ts";

async function fixture() {
  const { app } = await verifyMcpTestDatabases();
  const db = getDb(),
    now = new Date("2026-01-01T00:00:00Z");
  const actorId = crypto.randomUUID(),
    connectionId = crypto.randomUUID(),
    deploymentId = crypto.randomUUID();
  const identity = {
    protocol: 1 as const,
    bootId: crypto.randomUUID(),
    zoneId: "public",
    policyDigest: canonicalDigest({ policy: 1 }),
    policyRevision: "1",
  };
  const status = create(CredentialStatusSchema, {
    protocol: 1,
    status: "connected",
    version: 1n,
    context: {
      connectionId,
      configRevision: 1n,
      authorizationGeneration: 2n,
      credentialIdentity: "account",
      resource: { endpoint: "https://tools.example.test/mcp", zoneId: "public" },
      policyDigest: identity.policyDigest,
      policyRevision: "1",
    },
    reference: { kind: CredentialKind.API_KEY, connectionId, name: "mcp.org.key", generation: 1n },
  });
  await db.insert(schema.user).values({
    id: actorId,
    name: "Discovery administrator",
    email: `${actorId}@example.test`,
    role: "admin",
    createdAt: now,
    updatedAt: now,
  });
  await db.insert(schema.integrationConnection).values({
    id: connectionId,
    alias: `discovery-${connectionId}`,
    displayName: "Discovery tools",
    provider: "mcp",
    configRevision: 1,
    authGeneration: 2,
    credentialIdentity: "account",
    config: {
      endpoint: "https://tools.example.test/mcp",
      zone: "public",
      auth: { kind: "api_key", secretRef: "mcp.org.key" },
    },
    mcpCredential: { status: statusMetadata(status) },
  });
  const input = {
    requestId: crypto.randomUUID(),
    actorId,
    connectionId,
    deploymentId,
    configRevision: 1,
    authGeneration: 2,
    credentialIdentity: "account",
    identity,
    destination: "https://tools.example.test/mcp",
  };
  const admission = makeMcpDiscoveryAdmission(db);
  const deployments = new Set([deploymentId]);
  return {
    app,
    db,
    now,
    input,
    identity,
    status,
    admission,
    deployments,
    operation(intent: Awaited<ReturnType<typeof admission.reserve>>): AdmittedCredentialOperation {
      return {
        identity,
        status,
        intent,
        operation: { kind: "discover", endpoint: input.destination },
      };
    },
    async cleanup() {
      await db
        .delete(schema.mcpDiscoveryAdmission)
        .where(eq(schema.mcpDiscoveryAdmission.connectionId, connectionId));
      await db
        .delete(schema.integrationConnection)
        .where(eq(schema.integrationConnection.id, connectionId));
      await db.delete(schema.user).where(eq(schema.user.id, actorId));
      for (const id of deployments)
        await db.delete(schema.mcpAdmissionScope).where(eq(schema.mcpAdmissionScope.id, id));
    },
  };
}

test("durable discovery has a one-use dispatch barrier, immutable identity and fixed expiry", async () => {
  const f = await fixture();
  try {
    const intent = await f.admission.reserve(f.input, f.now),
      operation = f.operation(intent);
    expect(intent.scope).toEqual({
      kind: "administration",
      actorId: f.input.actorId,
      requestId: f.input.requestId,
    });
    expect(intent.expiresAt).toBe(f.now.getTime() + 30_000);
    await expect(f.admission.assertAdmitted(operation, f.now)).rejects.toThrow();
    await f.admission.beginDispatch(f.input.requestId, intent.id, f.now);
    await expect(f.admission.beginDispatch(f.input.requestId, intent.id, f.now)).rejects.toThrow();
    await f.admission.assertAdmitted(operation, f.now);
    await f.admission.assertAdmitted(operation, f.now);
    await expect(
      f.db
        .update(schema.mcpDiscoveryAdmission)
        .set({ destination: "https://other.example.test/mcp" })
        .where(eq(schema.mcpDiscoveryAdmission.requestId, f.input.requestId))
        .execute(),
    ).rejects.toThrow();
    await expect(f.admission.reserve(f.input, f.now)).rejects.toThrow("consumed");
    await expect(
      f.admission.reserve({ ...f.input, destination: "https://other.example.test/mcp" }, f.now),
    ).rejects.toThrow();
    await expect(
      f.admission.assertAdmitted(operation, new Date(intent.expiresAt)),
    ).rejects.toThrow();
    await f.admission.finish(f.input.requestId, intent.id, f.now);
    await f.admission.finish(f.input.requestId, intent.id, f.now);
    await expect(f.admission.assertAdmitted(operation, f.now)).rejects.toThrow();
    await expect(f.admission.reserve(f.input, new Date(intent.expiresAt + 1))).rejects.toThrow(
      "consumed",
    );
    await expect(
      f.db
        .update(schema.mcpDiscoveryAdmission)
        .set({ state: "reserved" })
        .where(eq(schema.mcpDiscoveryAdmission.requestId, f.input.requestId))
        .execute(),
    ).rejects.toThrow();
    const next = await f.admission.reserve({ ...f.input, requestId: crypto.randomUUID() }, f.now);
    expect(next.id).not.toBe(intent.id);
  } finally {
    await f.cleanup();
  }
});

test("discovery checks live admin, config, authorization, account and exact worker operation on both broker fences", async () => {
  const f = await fixture();
  try {
    const intent = await f.admission.reserve(f.input, f.now),
      op = f.operation(intent);
    await f.admission.beginDispatch(f.input.requestId, intent.id, f.now);
    for (const changed of [
      { ...op, identity: { ...op.identity, bootId: "other" } },
      { ...op, identity: { ...op.identity, policyRevision: "2" } },
      { ...op, intent: { ...intent, destination: "https://other.example.test/mcp" } },
      { ...op, intent: { ...intent, requestDigest: canonicalDigest({ changed: true }) } },
      {
        ...op,
        intent: {
          ...intent,
          scope: {
            kind: "administration" as const,
            requestId: f.input.requestId,
            actorId: "other",
          },
        },
      },
      {
        ...op,
        operation: { kind: "discover" as const, endpoint: "https://other.example.test/mcp" },
      },
      {
        ...op,
        status: create(CredentialStatusSchema, {
          ...f.status,
          context: { ...f.status.context!, authorizationGeneration: 3n },
        }),
      },
    ])
      await expect(f.admission.assertAdmitted(changed, f.now)).rejects.toThrow();
    for (const patch of [
      { role: "user" },
      { banned: true },
      { email: "apikey+fixture@service.local" },
    ]) {
      await f.db.update(schema.user).set(patch).where(eq(schema.user.id, f.input.actorId));
      await expect(f.admission.assertAdmitted(op, f.now)).rejects.toThrow();
      await f.db
        .update(schema.user)
        .set({ role: "admin", banned: false, email: `${f.input.actorId}@example.test` })
        .where(eq(schema.user.id, f.input.actorId));
    }
    for (const patch of [
      { configRevision: 2 },
      { authGeneration: 3 },
      { credentialIdentity: "different-account" },
      { revoked: true },
      {
        mcpCredential: {
          transition: { id: "transition", actorId: f.input.actorId, startedAt: f.now.getTime() },
        },
      },
    ]) {
      await f.db
        .update(schema.integrationConnection)
        .set(patch)
        .where(eq(schema.integrationConnection.id, f.input.connectionId));
      await expect(f.admission.assertAdmitted(op, f.now)).rejects.toThrow();
      await f.db
        .update(schema.integrationConnection)
        .set({
          configRevision: 1,
          authGeneration: 2,
          credentialIdentity: "account",
          revoked: false,
          mcpCredential: { status: statusMetadata(f.status) },
        })
        .where(eq(schema.integrationConnection.id, f.input.connectionId));
    }
    let leases = 0,
      sends = 0;
    const broker = makeCredentialBroker({
      admission: { assertAdmitted: (operation) => f.admission.assertAdmitted(operation, f.now) },
      now: () => f.now.getTime(),
      worker: {
        async identity() {
          return f.identity;
        },
        async dispatch() {
          sends++;
          throw Error("unexpected send");
        },
      },
      control: unwrapCredentialControlClient(
        createClient(
          CredentialControlService,
          createRouterTransport((router) =>
            router.service(CredentialControlService, {
              async lease() {
                leases++;
                await f.db
                  .update(schema.user)
                  .set({ role: "user" })
                  .where(eq(schema.user.id, f.input.actorId));
                return {
                  protocol: 1,
                  intentId: intent.id,
                  expiresAt: BigInt(f.now.getTime() + 10_000),
                  operationExpiresAt: BigInt(intent.expiresAt),
                  header: "authorization",
                  value: "fixture-credential",
                };
              },
            }),
          ),
        ),
      ),
    });
    await expect(broker.dispatch(op)).rejects.toThrow();
    expect(leases).toBe(1);
    expect(sends).toBe(0);
    await expect(broker.dispatch(op)).rejects.toThrow();
    expect(leases).toBe(1);
  } finally {
    await f.cleanup();
  }
});

test("discovery connection budget is global across deployments, clients and fixed death expiry", async () => {
  const f = await fixture();
  const pool = new Pool({ connectionString: f.app, max: 10 });
  try {
    const other = makeMcpDiscoveryAdmission(drizzle(pool, { schema }));
    const inputs = Array.from({ length: 6 }, () => ({
      ...f.input,
      requestId: crypto.randomUUID(),
      deploymentId: crypto.randomUUID(),
    }));
    inputs.forEach((input) => f.deployments.add(input.deploymentId));
    const results = await Promise.allSettled(
      inputs.map((input, i) => (i % 2 ? other : f.admission).reserve(input, f.now)),
    );
    expect(results.filter((r) => r.status === "fulfilled")).toHaveLength(1);
    expect(results.filter((r) => r.status === "rejected")).toHaveLength(5);
    const winner = results.find((r) => r.status === "fulfilled")!;
    if (winner.status !== "fulfilled") throw Error("missing winner");
    const later = new Date(f.now.getTime() + 30_000);
    await expect(
      other.beginDispatch(
        (winner.value.scope as { requestId: string }).requestId,
        winner.value.id,
        later,
      ),
    ).rejects.toThrow();
    expect(
      (await other.reserve({ ...f.input, requestId: crypto.randomUUID() }, later)).expiresAt,
    ).toBe(later.getTime() + 30_000);
  } finally {
    await pool.end();
    await f.cleanup();
  }
});

test("discovery deployment budget admits four independent connections with no queue", async () => {
  const fixtures = [];
  try {
    for (let i = 0; i < 5; i++) fixtures.push(await fixture());
    const first = fixtures[0]!;
    const states = await Promise.allSettled(
      fixtures.map((f) =>
        f.admission.reserve({ ...f.input, deploymentId: first.input.deploymentId }, f.now),
      ),
    );
    expect(states.filter((r) => r.status === "fulfilled")).toHaveLength(4);
    expect(states.filter((r) => r.status === "rejected")).toHaveLength(1);
    const winner = states.findIndex((r) => r.status === "fulfilled"),
      rejected = states.findIndex((r) => r.status === "rejected");
    const intent = states[winner]!;
    if (intent.status !== "fulfilled") throw Error("missing winner");
    await first.admission.finish(fixtures[winner]!.input.requestId, intent.value.id, first.now);
    await fixtures[rejected]!.admission.reserve(
      { ...fixtures[rejected]!.input, deploymentId: first.input.deploymentId },
      first.now,
    );
    // Delete exact owned rows before their shared deployment scope.
    for (const f of fixtures)
      await f.db
        .delete(schema.mcpDiscoveryAdmission)
        .where(eq(schema.mcpDiscoveryAdmission.connectionId, f.input.connectionId));
  } finally {
    for (const f of fixtures.slice().reverse()) await f.cleanup();
  }
});

test("administrative schema admission is credential-free, one-use, bounded and exact", async () => {
  const f = await fixture();
  try {
    const documents = [{ type: "object", properties: { value: { type: "integer" } } }];
    const input = { ...f.input, documents };
    const intents = [];
    for (let i = 0; i < 4; i++)
      intents.push(
        await f.admission.reserveSchema({ ...input, requestId: crypto.randomUUID() }, f.now),
      );
    await expect(
      f.admission.reserveSchema({ ...input, requestId: crypto.randomUUID() }, f.now),
    ).rejects.toThrow("capacity");
    // Schema capacity does not consume discovery capacity, including this connection.
    const discovery = await f.admission.reserve(f.input, f.now);
    const intent = intents[0]!;
    expect(intent.operation).toBe("validateSchemas");
    expect(intent.destination).toBe("");
    if (intent.scope.kind !== "administration") throw Error("scope");
    const requestId = intent.scope.requestId;
    const operation = { identity: input.identity, intent, documents };
    await expect(f.admission.assertSchemaAdmitted(operation, f.now)).rejects.toThrow();
    await f.admission.beginDispatch(requestId, intent.id, f.now);
    await f.admission.assertSchemaAdmitted(operation, f.now);
    await expect(f.admission.beginDispatch(requestId, intent.id, f.now)).rejects.toThrow();
    await expect(
      f.admission.assertSchemaAdmitted({ ...operation, documents: [{ type: "string" }] }, f.now),
    ).rejects.toThrow();
    await expect(
      f.admission.assertSchemaAdmitted(
        { ...operation, identity: { ...input.identity, bootId: "changed" } },
        f.now,
      ),
    ).rejects.toThrow();
    await expect(
      f.admission.assertSchemaAdmitted(operation, new Date(intent.expiresAt)),
    ).rejects.toThrow();
    await f.db.update(schema.user).set({ role: "user" }).where(eq(schema.user.id, input.actorId));
    await expect(f.admission.assertSchemaAdmitted(operation, f.now)).rejects.toThrow();
    await f.db.update(schema.user).set({ role: "admin" }).where(eq(schema.user.id, input.actorId));
    await f.admission.finish(requestId, intent.id, f.now);
    await expect(f.admission.reserveSchema({ ...input, requestId }, f.now)).rejects.toThrow(
      "consumed",
    );
    await expect(f.admission.assertSchemaAdmitted(operation, f.now)).rejects.toThrow();
    await f.admission.reserveSchema(
      { ...input, requestId: crypto.randomUUID() },
      new Date(intent.expiresAt),
    );
    await f.admission.finish(f.input.requestId, discovery.id, f.now);
  } finally {
    await f.cleanup();
  }
});
