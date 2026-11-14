import { expect, test } from "bun:test";
import { effectFixture } from "./mcp-effect-fixture.ts";
import { makeCredentialMetadataStore } from "../mcp/credential-store.ts";
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
    const credentials = makeCredentialMetadataStore(f.db);
    const [current] = await f.db
      .select()
      .from(integrationConnection)
      .where(eq(integrationConnection.id, f.connection.id));
    await credentials.claim(
      f.connection.id,
      current!.configRevision,
      current!.authGeneration,
      f.actorId,
      current!.config,
      new Date(),
    );
    expect((await credentials.get(f.connection.id)).revoked).toBe(true);
    expect(calls).toBe(1);
    await expect(
      credentials.claim(
        f.connection.id,
        current!.configRevision,
        current!.authGeneration,
        f.actorId,
        current!.config,
        new Date(),
      ),
    ).rejects.toThrow();
    expect(calls).toBe(1);
  } finally {
    stop();
    await f.cleanup();
  }
}, 20000);

test("a credential transition requests durable reconciliation of active work", async () => {
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
    await makeCredentialMetadataStore(f.db).claim(
      f.connection.id,
      current!.configRevision,
      current!.authGeneration,
      f.actorId,
      current!.config,
      new Date(),
    );
    expect(scan).toBeDefined();
    await scan;
    expect((await f.journal.get(call.id))!.state).toBe("canceled");
    expect((await f.command("stats")).calls).toBe(0);
  } finally {
    stop();
    await f.cleanup();
  }
}, 20000);
