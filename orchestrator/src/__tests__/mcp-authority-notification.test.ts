import { expect, test } from "bun:test";
import { effectFixture } from "./mcp-effect-fixture.ts";
import { makeMcpRepository } from "../mcp/repository.ts";
import { observeMcpAuthorityChanges } from "../mcp/authority-notification.ts";

test("a committed authority mutation requests reconciliation and notification failure does not undo it", async () => {
  const { eq } = await import("drizzle-orm");
  const { integrationConnection } = await import("../db/schema.ts");
  const f = await effectFixture({ classification: "read" });
  let calls = 0;
  const stop = observeMcpAuthorityChanges(() => {
    calls++;
    throw Error("unavailable");
  });
  try {
    const repository = makeMcpRepository(f.db);
    const [current] = await f.db
      .select()
      .from(integrationConnection)
      .where(eq(integrationConnection.id, f.connection.id));
    const updated = await repository.disconnect(
      f.connection.id,
      current!.authGeneration,
      new Date(),
    );
    expect(updated.revoked).toBe(true);
    expect(calls).toBe(1);
    await expect(
      repository.disconnect(f.connection.id, current!.authGeneration, new Date()),
    ).rejects.toThrow();
    expect(calls).toBe(1);
  } finally {
    stop();
    await f.cleanup();
  }
}, 20000);

test("a committed disconnect requests durable reconciliation of active work", async () => {
  const { eq } = await import("drizzle-orm");
  const { integrationConnection } = await import("../db/schema.ts");
  const { makeMcpLifecycleReconciler } = await import("../mcp/lifecycle-reconciler.ts");
  const f = await effectFixture({ classification: "read" });
  const scanner = makeMcpLifecycleReconciler({ db: f.db, driver: f.driver });
  let scan: Promise<void> | undefined;
  const stop = observeMcpAuthorityChanges(() => {
    scan = scanner.reconcileAuthorityChange();
  });
  try {
    const call = await f.driver.acceptToolRequest(f.request);
    const [current] = await f.db
      .select()
      .from(integrationConnection)
      .where(eq(integrationConnection.id, f.connection.id));
    await makeMcpRepository(f.db).disconnect(f.connection.id, current!.authGeneration, new Date());
    expect(scan).toBeDefined();
    await scan;
    expect((await f.journal.get(call.id))!.state).toBe("canceled");
    expect((await f.command("stats")).calls).toBe(0);
  } finally {
    stop();
    await f.cleanup();
  }
}, 20000);
