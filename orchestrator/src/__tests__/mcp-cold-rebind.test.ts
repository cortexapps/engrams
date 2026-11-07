import { expect, test } from "bun:test";
import { eq } from "drizzle-orm";
import { integrationConnection, profile, task, taskSession } from "../db/schema.ts";
import { McpRebindDraining } from "../mcp/rebind.ts";
import { coldFixture } from "./mcp-cold-fixture.ts";
import { verifyMcpTestDatabases } from "./helpers/mcp-test-isolation.ts";
await verifyMcpTestDatabases();

const prepare = (f: Awaited<ReturnType<typeof coldFixture>>) =>
  f.service.prepare(f.sessionId, f.snapshot.bindingHash, f.actorId);
const commit = (f: Awaited<ReturnType<typeof coldFixture>>, hash: string) =>
  f.service.commit(f.sessionId, f.snapshot.bindingHash, hash, f.actorId);

test("cold rebind persists intent before capture and opens only the exact finalized new boot", async () => {
  const f = await coldFixture();
  try {
    const candidate = await prepare(f);
    f.beforeRecover(async () => {
      f.beforeRecover(async () => {});
      const row = await f.store.read(f.sessionId);
      expect(row.session.mcpManagedLifecycle?.phase).toBe("recovering");
      expect(row.session.mcpManagedLifecycle?.recovery?.candidateHash).toBe(candidate.bindingHash);
    });
    f.beforeApply(async () => {
      const row = await f.store.read(f.sessionId);
      expect(row.session.mcpManagedLifecycle?.phase).toBe("finalizing");
      expect(row.session.mcpBindingEnvelope?.bindingEpoch).toBe(2);
      expect(row.session.mcpManagedLifecycle?.pending?.combinedManifestDigest).toBeTruthy();
    });
    await expect(commit(f, candidate.bindingHash)).rejects.toBeInstanceOf(McpRebindDraining);
    const pending = (await f.store.read(f.sessionId)).session.mcpManagedLifecycle!;
    expect(pending.phase).toBe("finalizing");
    expect(f.stats()).toMatchObject({ recoveryIds: 1, applies: 1, finalizes: 1 });
    f.start();
    expect(await commit(f, candidate.bindingHash)).toEqual({ bindingHash: pending.bindingHash });
    expect((await f.store.read(f.sessionId)).session.mcpManagedLifecycle?.phase).toBe("started");
    expect(f.stats()).toMatchObject({ recoveryIds: 1, applies: 1, finalizes: 1 });
  } finally {
    await f.cleanup();
  }
});

test("lost recovery and finalization replies retain one recovery and one final manifest", async () => {
  const f = await coldFixture();
  try {
    const candidate = await prepare(f);
    f.loseRecover();
    await expect(commit(f, candidate.bindingHash)).rejects.toThrow("lost recovery");
    expect((await f.store.read(f.sessionId)).session.mcpManagedLifecycle?.phase).toBe("recovering");
    f.loseFinalize();
    await expect(commit(f, candidate.bindingHash)).rejects.toThrow("internal");
    f.start();
    await f.restartRunOnce();
    await commit(f, candidate.bindingHash);
    expect(f.stats()).toMatchObject({ recoveryIds: 1, applies: 1, finalizes: 1 });
  } finally {
    await f.cleanup();
  }
});

test("stale review, owner, connection and publication fail before capture", async () => {
  const f = await coldFixture();
  try {
    const candidate = await prepare(f);
    await expect(commit(f, "wrong")).rejects.toThrow("stale");
    await f.db.update(task).set({ createdByUserId: null }).where(eq(task.id, f.taskId));
    await expect(commit(f, candidate.bindingHash)).rejects.toThrow();
    await f.db.update(task).set({ createdByUserId: f.actorId }).where(eq(task.id, f.taskId));
    await f.db
      .update(integrationConnection)
      .set({ enabled: false })
      .where(eq(integrationConnection.id, f.connection.id));
    await expect(commit(f, candidate.bindingHash)).rejects.toThrow();
    await f.db
      .update(integrationConnection)
      .set({ enabled: true })
      .where(eq(integrationConnection.id, f.connection.id));
    await f.db
      .update(profile)
      .set({ mcpGrantPublication: null })
      .where(eq(profile.id, f.profileId));
    await expect(commit(f, candidate.bindingHash)).rejects.toThrow();
    expect(f.stats().recoverCalls).toBe(0);
  } finally {
    await f.cleanup();
  }
});

test("cold rebind rejects changed native bytes and source placement before capture", async () => {
  const f = await coldFixture();
  try {
    const candidate = await prepare(f);
    f.setNative("[{}");
    await expect(commit(f, candidate.bindingHash)).rejects.toThrow();
    f.setNative("[]");
    f.state.boot!.request.key.context.target.sandbox_id = crypto.randomUUID();
    await expect(commit(f, candidate.bindingHash)).rejects.toThrow("source changed");
    expect(f.stats().recoverCalls).toBe(0);
  } finally {
    await f.cleanup();
  }
});

test("cold rebind preserves newer retained denies and stops on authority change during apply", async () => {
  const f = await coldFixture();
  try {
    f.addRetained("retained.example.test");
    const candidate = await prepare(f);
    expect(candidate.managedDenyPolicy.hosts).toContain("retained.example.test");
    f.beforeApply(async () => {
      await f.db
        .update(profile)
        .set({ mcpGrantPublication: null })
        .where(eq(profile.id, f.profileId));
    });
    await expect(commit(f, candidate.bindingHash)).rejects.toThrow("authority changed");
    expect(f.stats().finalizes).toBe(0);
    const row = await f.store.read(f.sessionId);
    expect(row.session.mcpManagedLifecycle?.phase).toBe("finalizing");
    expect(row.session.mcpManagedLifecycle?.pending?.snapshot.managedDenyPolicy.hosts).toContain(
      "retained.example.test",
    );
  } finally {
    await f.cleanup();
  }
});

test("cold host calls do not hold session or owner database locks", async () => {
  const f = await coldFixture();
  try {
    const candidate = await prepare(f);
    f.beforeRecover(async () => {
      await f.db.transaction(async (tx) => {
        await tx.execute("SET LOCAL lock_timeout = '500ms'");
        await tx.update(task).set({ title: "Unrelated local update" }).where(eq(task.id, f.taskId));
        await tx
          .update(taskSession)
          .set({ role: "primary" })
          .where(eq(taskSession.sessionId, f.sessionId));
      });
    });
    await expect(commit(f, candidate.bindingHash)).rejects.toBeInstanceOf(McpRebindDraining);
    expect(f.stats().recoveryIds).toBe(1);
  } finally {
    await f.cleanup();
  }
});

test("retired source is deny evidence only and the new boot retains later desired hosts", async () => {
  const f = await coldFixture();
  try {
    const boot = f.state.boot!;
    boot.phase = "interrupted";
    boot.retired = {
      prepared: boot.prepared!,
      retained_policy: f.snapshot.managedDenyPolicy,
      receipt_id: `sha256:${"f".repeat(64)}`,
    };
    const candidate = await prepare(f);
    expect(f.stats().hostReads).toBe(0);
    f.addRetained("later.example.test");
    await expect(commit(f, candidate.bindingHash)).rejects.toBeInstanceOf(McpRebindDraining);
    expect(f.stats().applies).toBe(1);
    const value = (await f.store.read(f.sessionId)).session.mcpManagedLifecycle!;
    expect(value.pending!.snapshot.managedDenyPolicy.hosts).toContain("later.example.test");
    expect(value.phase).toBe("finalizing");
    f.start();
    await f.restartRunOnce();
    expect((await f.store.read(f.sessionId)).session.mcpManagedLifecycle?.phase).toBe("started");
  } finally {
    await f.cleanup();
  }
});

test("sanitized state accepts the bounded JSON expansion of a maximum-size final manifest", async () => {
  const { decodeManagedState } = await import("../mcp/managed-state.ts");
  const { bytesDigest } = await import("../mcp/managed-create-input.ts");
  const f = await coldFixture();
  try {
    const prefix = '[{"name":"native","description":"',
      suffix = '","inputSchema":{},"execution":"sync"}]';
    const bytes = new TextEncoder().encode(
      prefix + "x".repeat(1024 * 1024 - prefix.length - suffix.length) + suffix,
    );
    expect(bytes.length).toBe(1024 * 1024);
    f.state.boot!.finalization = {
      prepared: f.state.boot!.prepared!,
      native_version: 1,
      combined_manifest: [...bytes],
      combined_manifest_digest: bytesDigest(bytes),
    };
    const stateJson = new TextEncoder().encode(JSON.stringify(f.state));
    expect(stateJson.length).toBeGreaterThan(4 * 1024 * 1024);
    const value = (await f.store.read(f.sessionId)).session.mcpManagedLifecycle!;
    expect(
      decodeManagedState({ protocol: 1, stateJson }, value).boot!.finalization!
        .combined_manifest_digest,
    ).toBe(bytesDigest(bytes));
    expect(() =>
      decodeManagedState({ protocol: 1, stateJson: new Uint8Array(8 * 1024 * 1024 + 1) }, value),
    ).toThrow("unavailable");
  } finally {
    await f.cleanup();
  }
});
