import { expect, test } from "bun:test";
import { eq } from "drizzle-orm";
import { integrationConnection, profile, task, taskSession } from "../db/schema.ts";
import type { ManagedLifecycle } from "../mcp/managed-lifecycle.ts";
import { canonicalDigest } from "../mcp/hash.ts";
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
    expect((await f.store.read(f.sessionId)).session.mcpManagedLifecycle?.phase).toBe("recovering");
    expect(f.stats().recoverCalls).toBe(0);
    await f.restartRunOnce();
    const pending = (await f.store.read(f.sessionId)).session.mcpManagedLifecycle!;
    expect(pending.phase).toBe("finalizing");
    expect(f.stats()).toMatchObject({ recoveryIds: 1, applies: 1, finalizes: 1 });
    f.start();
    await f.restartRunOnce();
    expect(await commit(f, candidate.bindingHash)).toEqual({ bindingHash: pending.bindingHash });
    expect((await f.store.read(f.sessionId)).session.mcpManagedLifecycle?.phase).toBe("started");
    expect(f.stats()).toMatchObject({ recoveryIds: 1, applies: 1, finalizes: 1 });
  } finally {
    await f.cleanup();
  }
});

test("commit leaves recovery to the scanner and accepts only its exact completed intent", async () => {
  const f = await coldFixture();
  const read = f.store.read;
  let release!: () => void;
  const blocked = new Promise<void>((resolve) => {
    release = resolve;
  });
  let enter!: () => void;
  const entered = new Promise<void>((resolve) => {
    enter = resolve;
  });
  let scanner: Promise<void> | undefined;
  let armed = true;
  let captures = 0;
  try {
    const candidate = await prepare(f);
    f.beforeRecover(async () => {
      if (++captures > 1) throw Error("concurrent recovery driver");
      enter();
      await blocked;
    });
    f.store.read = async (id) => {
      const row = await read(id);
      if (armed && row.session.mcpManagedLifecycle?.phase === "recovering") {
        armed = false;
        scanner = f.lifecycle.runOnce(id);
        await entered;
      }
      return row;
    };
    await expect(commit(f, candidate.bindingHash)).rejects.toBeInstanceOf(McpRebindDraining);
    expect((await read(f.sessionId)).session.mcpManagedLifecycle?.phase).toBe("recovering");
    expect(f.stats()).toMatchObject({ recoveryIds: 0, recoverCalls: 1 });
    release();
    await scanner;
    f.beforeRecover(async () => {});
    f.start();
    await f.restartRunOnce();
    const finished = (await read(f.sessionId)).session.mcpManagedLifecycle!;
    expect(finished.phase).toBe("started");
    expect(await commit(f, candidate.bindingHash)).toEqual({ bindingHash: finished.bindingHash });
    await expect(commit(f, "wrong")).rejects.toThrow();
    await expect(
      f.service.commit(
        f.sessionId,
        f.snapshot.bindingHash,
        candidate.bindingHash,
        crypto.randomUUID(),
      ),
    ).rejects.toThrow();
    expect(f.stats()).toMatchObject({ recoveryIds: 1, applies: 1, finalizes: 1 });
  } finally {
    release();
    await scanner;
    f.store.read = read;
    await f.cleanup();
  }
});

const changedRecoveries: Array<{
  name: string;
  replace(value: ManagedLifecycle): ManagedLifecycle;
}> = [
  {
    name: "missing intent",
    replace: ({ recovery: _recovery, ...value }) => ({ ...value, phase: "started" }),
  },
  { name: "terminal", replace: (value) => ({ ...value, phase: "terminal" }) },
  { name: "held", replace: (value) => ({ ...value, phase: "held" }) },
  {
    name: "replaced pending intent",
    replace: (value) => ({ ...value, recovery: { ...value.recovery!, id: crypto.randomUUID() } }),
  },
  {
    name: "replaced started intent",
    replace: (value) => ({
      ...value,
      phase: "started",
      recovery: { ...value.recovery!, id: crypto.randomUUID() },
    }),
  },
  {
    name: "changed actor",
    replace: (value) => ({
      ...value,
      recovery: { ...value.recovery!, actorId: crypto.randomUUID() },
    }),
  },
  {
    name: "changed source binding",
    replace: (value) => ({
      ...value,
      recovery: { ...value.recovery!, expectedBindingHash: `sha256:${"f".repeat(64)}` },
    }),
  },
  {
    name: "changed reviewed candidate",
    replace: (value) => {
      const { bindingHash: _hash, ...body } = value.recovery!.candidate;
      const changed = { ...body, profileId: crypto.randomUUID() };
      const candidate = { ...changed, bindingHash: canonicalDigest(changed) };
      return {
        ...value,
        recovery: { ...value.recovery!, candidate, candidateHash: candidate.bindingHash },
      };
    },
  },
];
for (const change of changedRecoveries) {
  test(`commit rejects a ${change.name} recovery after its authority check`, async () => {
    const f = await coldFixture();
    const read = f.store.read;
    try {
      const candidate = await prepare(f);
      await expect(commit(f, candidate.bindingHash)).rejects.toBeInstanceOf(McpRebindDraining);
      f.store.read = async (id) => {
        const row = await read(id);
        await f.db
          .update(taskSession)
          .set({ mcpManagedLifecycle: change.replace(row.session.mcpManagedLifecycle!) })
          .where(eq(taskSession.sessionId, id));
        return read(id);
      };
      await expect(commit(f, candidate.bindingHash)).rejects.toThrow(
        "MCP rebind reservation changed",
      );
    } finally {
      f.store.read = read;
      await f.cleanup();
    }
  });
}

test("lost recovery and finalization replies retain one recovery and one final manifest", async () => {
  const f = await coldFixture();
  try {
    const candidate = await prepare(f);
    await expect(commit(f, candidate.bindingHash)).rejects.toBeInstanceOf(McpRebindDraining);
    f.loseRecover();
    await expect(f.restartRunOnce()).rejects.toThrow("lost recovery");
    expect((await f.store.read(f.sessionId)).session.mcpManagedLifecycle?.phase).toBe("recovering");
    f.loseFinalize();
    await expect(f.restartRunOnce()).rejects.toThrow("internal");
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
    await expect(commit(f, candidate.bindingHash)).rejects.toBeInstanceOf(McpRebindDraining);
    await expect(f.restartRunOnce()).rejects.toThrow("authority changed");
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
    await f.restartRunOnce();
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
    await f.restartRunOnce();
    expect(f.stats().applies).toBe(1);
    const value = (await f.store.read(f.sessionId)).session.mcpManagedLifecycle!;
    expect(value.pending!.snapshot.managedDenyPolicy.hosts).toContain("later.example.test");
    expect(value.phase).toBe("finalizing");
    f.start();
    await f.restartRunOnce();
    expect((await f.store.read(f.sessionId)).session.mcpManagedLifecycle?.phase).toBe("started");
    expect(await commit(f, candidate.bindingHash)).toEqual({ bindingHash: value.bindingHash });
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
