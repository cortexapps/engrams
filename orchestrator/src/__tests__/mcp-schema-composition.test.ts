import { expect, test } from "bun:test";
import { eq } from "drizzle-orm";
import { effectFixture } from "./mcp-effect-fixture.ts";
import { verifyMcpTestDatabases } from "./helpers/mcp-test-isolation.ts";
import { mcpDiscoveryAdmission, user } from "../db/schema.ts";
import { makeMcpDiscoveryAdmission } from "../mcp/discovery-admission.ts";
import { makeMcpOperatorRuntime } from "../mcp/operator-runtime.ts";
import { makeMcpDiscovery } from "../mcp/discovery.ts";
import { makeCredentialMetadataStore } from "../mcp/credential-store.ts";
import { makeMcpSchemaValidator } from "../mcp/schema-validator.ts";

await verifyMcpTestDatabases();
test("actual isolated worker validates admin schemas without credential acquisition", async () => {
  const f = await effectFixture();
  try {
    await f.db.update(user).set({ role: "admin" }).where(eq(user.id, f.actorId));
    let enabled = true;
    const publication = f.operatorPolicies[0]!.publication;
    const operator = makeMcpOperatorRuntime({
      enabled: () => enabled,
      configuration: {
        infrastructure: publication,
        control: f.remote.workerTls,
        registrations: [],
        workers: new Map([
          ["public", { enabled: true, deploymentId: f.deploymentId, tls: f.remote.workerTls }],
        ]),
      },
      read: async () => ({
        generation: publication.generation,
        identity: publication.identity,
        policies: f.operatorPolicies,
      }),
    });
    const admission = makeMcpDiscoveryAdmission(f.db);
    const validate = makeMcpSchemaValidator({ admission, operator, now: () => new Date() });
    const connection = (await f.connections.get(f.connection.id))!;
    const tools = f.snapshot.immutableBindings[0]!.tools.map((t) => t.tool);
    await validate(tools, connection, f.actorId);
    expect(f.leases).toBe(0);
    expect(await f.command("stats")).toEqual({ calls: 0, credentialRequests: 0 });
    await expect(
      validate(
        [{ ...tools[0]!, inputSchema: { type: "not-a-schema-type" } }],
        connection,
        f.actorId,
      ),
    ).rejects.toThrow();
    const rows = await f.db
      .select()
      .from(mcpDiscoveryAdmission)
      .where(eq(mcpDiscoveryAdmission.connectionId, f.connection.id));
    expect(rows).toHaveLength(2);
    expect(
      rows.every((row) => row.state === "finished" && row.operation.kind === "validateSchemas"),
    ).toBe(true);
    enabled = false;
    await expect(validate(tools, connection, f.actorId)).rejects.toThrow("disabled");
    enabled = true;
    await f.db.update(user).set({ role: "user" }).where(eq(user.id, f.actorId));
    await expect(validate(tools, connection, f.actorId)).rejects.toThrow("administrator");
    expect(f.leases).toBe(0);
  } finally {
    await f.db
      .delete(mcpDiscoveryAdmission)
      .where(eq(mcpDiscoveryAdmission.connectionId, f.connection.id));
    await f.cleanup();
  }
}, 30000);

test("actual discovery uses one durable intent and fences role loss after credential delivery", async () => {
  const f = await effectFixture();
  try {
    await f.db.update(user).set({ role: "admin" }).where(eq(user.id, f.actorId));
    const publication = f.operatorPolicies[0]!.publication;
    const operator = makeMcpOperatorRuntime({
      enabled: () => true,
      configuration: {
        infrastructure: publication,
        control: f.remote.workerTls,
        registrations: [],
        workers: new Map([
          ["public", { enabled: true, deploymentId: f.deploymentId, tls: f.remote.workerTls }],
        ]),
      },
      read: async () => ({
        generation: publication.generation,
        identity: publication.identity,
        policies: f.operatorPolicies,
      }),
    });
    const discovery = makeMcpDiscovery({
      admission: makeMcpDiscoveryAdmission(f.db),
      credentials: makeCredentialMetadataStore(f.db),
      control: f.control,
      operator,
      now: () => new Date(),
    });
    const connection = (await f.connections.get(f.connection.id))!;
    expect(await discovery.discover(connection, f.actorId)).toEqual([
      {
        upstreamName: "Lookup.Record",
        description: "Find a fixture record",
        inputSchema: { type: "object" },
      },
    ]);
    expect(f.leases).toBe(1);
    const before = await f.command("stats");
    f.afterLease(async () => {
      await f.db.update(user).set({ role: "user" }).where(eq(user.id, f.actorId));
    });
    await expect(discovery.discover(connection, f.actorId)).rejects.toThrow("administrator");
    expect(f.leases).toBe(2);
    expect(await f.command("stats")).toEqual(before);
    const rows = await f.db
      .select()
      .from(mcpDiscoveryAdmission)
      .where(eq(mcpDiscoveryAdmission.connectionId, f.connection.id));
    expect(rows).toHaveLength(2);
    expect(rows.every((row) => row.state === "finished" && row.operation.kind === "discover")).toBe(
      true,
    );
  } finally {
    await f.db
      .delete(mcpDiscoveryAdmission)
      .where(eq(mcpDiscoveryAdmission.connectionId, f.connection.id));
    await f.cleanup();
  }
}, 30000);
