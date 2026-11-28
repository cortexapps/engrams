import { expect, test } from "bun:test";
import { createClient, createRouterTransport } from "@connectrpc/connect";
import { eq, sql } from "drizzle-orm";
import { user, integrationConnection, mcpCatalog } from "../db/schema.ts";
import { IntegrationService } from "../gen/engram/app/v1/integration_pb.ts";
import { mcpHandlers } from "../mcp/contracts.ts";
import { reviewFixture } from "./mcp-review-fixture.ts";

for (const [change, patch] of [
  ["demoted", { role: "user" }],
  ["banned", { banned: true }],
  ["service account", { email: "apikey+publisher@service.local" }],
] as const) {
  test(`catalog publication rejects a ${change} publisher after schema validation`, async () => {
    const f = await reviewFixture();
    const entered = Promise.withResolvers<void>(),
      release = Promise.withResolvers<void>();
    let pending: Promise<unknown> | undefined;
    try {
      const client = createClient(
        IntegrationService,
        createRouterTransport((router) =>
          router.service(
            IntegrationService,
            mcpHandlers({
              services: {
                enabled: true,
                repository: f.repository,
                journal: f.journal,
                async validateSchemas() {
                  entered.resolve();
                  await release.promise;
                },
              },
              connections: f.connections,
              getSession: async () => ({ user: { id: f.publisherId, role: "admin" } }),
              now: () => f.now,
              connectionToProto: (connection) => ({ id: connection.id }),
            }),
          ),
        ),
      );
      const before = await f.connections.get(f.connection.id);
      const history = await f.db
        .select()
        .from(mcpCatalog)
        .where(eq(mcpCatalog.connectionId, f.connection.id));
      pending = client.publishMcp({
        connectionId: f.connection.id,
        draftId: f.draft.id,
        expectedRevision: 1,
        tools: f.catalog.tools.map((tool) => ({
          upstreamName: tool.upstreamName,
          description: tool.description,
          inputSchemaJson: JSON.stringify(tool.inputSchema),
          classification: "read",
          enabled: true,
        })),
      });
      await entered.promise;
      await f.db.update(user).set(patch).where(eq(user.id, f.publisherId));
      release.resolve();
      await expect(pending).rejects.toThrow();
      expect(await f.connections.get(f.connection.id)).toEqual(before);
      expect(
        await f.db.select().from(mcpCatalog).where(eq(mcpCatalog.connectionId, f.connection.id)),
      ).toEqual(history);
    } finally {
      release.resolve();
      await pending?.catch(() => {});
      await f.cleanup();
    }
  });
}

test("catalog publication waits for a publisher role write before locking its connection", async () => {
  const f = await reviewFixture();
  const locked = Promise.withResolvers<number>(),
    release = Promise.withResolvers<void>();
  let mutation: Promise<void> | undefined, pending: Promise<string> | undefined;
  try {
    mutation = f.db.transaction(async (tx) => {
      await tx.update(user).set({ role: "user" }).where(eq(user.id, f.publisherId));
      const result = await tx.execute<{ pid: number }>(sql`select pg_backend_pid() as pid`);
      locked.resolve(result.rows[0]!.pid);
      await release.promise;
    });
    const blocker = await locked.promise;
    pending = f.repository
      .publish(f.connection.id, f.draft.id, 1, f.catalog.tools, f.publisherId, f.now)
      .then(
        () => "published",
        () => "denied",
      );
    let blocked = false;
    const deadline = Date.now() + 2000;
    while (Date.now() < deadline) {
      const result = await f.db.execute<{ waiting: boolean }>(sql`
        select exists (
          select 1 from pg_stat_activity where ${blocker} = any(pg_blocking_pids(pid))
        ) as waiting`);
      if (result.rows[0]?.waiting) {
        blocked = true;
        break;
      }
      await Bun.sleep(5);
    }
    expect(blocked).toBe(true);
    // Actor-before-connection lock order leaves the connection available to other work.
    await f.db.transaction(async (tx) => {
      await tx.execute(sql`set local lock_timeout = '500ms'`);
      await tx
        .select()
        .from(integrationConnection)
        .where(eq(integrationConnection.id, f.connection.id))
        .for("update");
    });
    release.resolve();
    await mutation;
    expect(await pending).toBe("denied");
    expect((await f.connections.get(f.connection.id))!.mcp!.catalogRevision).toBe(1);
  } finally {
    release.resolve();
    await mutation;
    await pending;
    await f.cleanup();
  }
});
