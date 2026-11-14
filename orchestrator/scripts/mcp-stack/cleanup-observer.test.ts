import { afterAll, beforeAll, expect, spyOn, test } from "bun:test";
import { mkdtemp, realpath, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import pg from "pg";
import { DatabaseObserver } from "./observer.ts";
import type { Context } from "./context.ts";

const sessionId = "11111111-1111-4111-8111-111111111111";
const hostId = "22222222-2222-4222-8222-222222222222";
const sandboxId = "33333333-3333-4333-8333-333333333333";
const otherId = "44444444-4444-4444-8444-444444444444";
let root: string;
beforeAll(async () => {
  root = await mkdtemp(join(await realpath(tmpdir()), "mcp-cleanup-observer-"));
  await writeFile(
    join(root, "observer.dsn"),
    "postgres://viewer@database.fixture.example/mcp_unit",
    { mode: 0o600 },
  );
});
afterAll(async () => {
  if (root) await rm(root, { recursive: true });
});

async function withObserver(
  run: (fixture: {
    database: DatabaseObserver;
    target: Record<string, unknown>;
    settled: Record<string, unknown>;
    queries: { sql: string; parameters: unknown[] }[];
    failQuery: () => void;
  }) => Promise<void>,
) {
  // Connection setup reads only these fields; the pool query is the external boundary.
  const ctx = {
    runId: "unit",
    runRoot: root,
    deadlines: { requestMs: 1000 },
    observer: {
      coordinatorDsnFile: join(root, "observer.dsn"),
      orchestratorDsnFile: join(root, "observer.dsn"),
    },
  } as Context;
  const database = await DatabaseObserver.connect(ctx);
  const target: Record<string, unknown> = {
    host_id: hostId,
    sandbox_id: sandboxId,
    binding_epoch: "7",
    managed_task_id: "fixture-task",
    managed_host_id: hostId,
    managed_sandbox_id: sandboxId,
    managed_binding_epoch: "7",
  };
  const settled: Record<string, unknown> = {
    ...target,
    host_id: null,
    sandbox_id: null,
    status: "completed",
    pending_operations: false,
    pending_boots: false,
    pending_tombstone: false,
  };
  const queries: { sql: string; parameters: unknown[] }[] = [];
  let fail = false;
  const query = spyOn(pg.Pool.prototype, "query").mockImplementation(
    async (...args: unknown[]) => {
      const sql = String(args[0]);
      const parameters = args[1] as unknown[];
      queries.push({ sql, parameters });
      if (fail) throw new Error("observer unavailable");
      return {
        rows: [sql.includes("pending_operations") ? settled : target],
        rowCount: 1,
        command: "SELECT",
        oid: 0,
        fields: [],
      };
    },
  );
  database.own("fixture-task", sessionId);
  try {
    await run({
      database,
      target,
      settled,
      queries,
      failQuery: () => {
        fail = true;
      },
    });
  } finally {
    query.mockRestore();
    await database.close();
  }
}

test("cleanup observer retains exact ownership before reading settled obligations", async () => {
  await withObserver(async ({ database, queries }) => {
    await expect(database.cleanupSettled(sessionId)).rejects.toThrow(
      "cleanup_target_not_retained",
    );
    expect(queries).toHaveLength(0);
    await database.retainCleanupTarget(sessionId, hostId);
    expect(await database.cleanupSettled(sessionId)).toBe(true);
    expect(queries[0]!.parameters).toEqual([sessionId]);
    expect(queries[1]!.parameters).toEqual([sessionId, hostId, sandboxId]);
    expect(queries[1]!.sql).toContain("state IN ('queued','running')");
    expect(queries[1]!.sql).toContain(
      "session_managed_boot_intents WHERE session_id=$1 AND NOT completed",
    );
    expect(queries[1]!.sql).toContain(
      "sandbox_tombstones WHERE host_id=$2 AND sandbox_id=$3",
    );
    await expect(database.retainCleanupTarget(otherId, hostId)).rejects.toThrow(
      "observer_unowned_session",
    );
    await expect(database.cleanupSettled(otherId)).rejects.toThrow(
      "observer_unowned_session",
    );
    expect(queries).toHaveLength(2);
  });
});

for (const pending of [
  "pending_operations",
  "pending_boots",
  "pending_tombstone",
])
  test(`destroy completion cannot settle cleanup with ${pending}`, async () => {
    await withObserver(async ({ database, settled }) => {
      await database.retainCleanupTarget(sessionId, hostId);
      settled[pending] = true;
      expect(await database.cleanupSettled(sessionId)).toBe(false);
      settled[pending] = false;
      expect(await database.cleanupSettled(sessionId)).toBe(true);
    });
  });

for (const [field, value] of [
  ["host_id", otherId],
  ["sandbox_id", otherId],
  ["binding_epoch", "8"],
  ["managed_task_id", "foreign-task"],
  ["managed_host_id", otherId],
  ["managed_sandbox_id", otherId],
  ["managed_binding_epoch", "8"],
  ["pending_tombstone", undefined],
] as const)
  test(`cleanup fails closed when the retained ${field} identity or observation changes`, async () => {
    await withObserver(async ({ database, settled }) => {
      await database.retainCleanupTarget(sessionId, hostId);
      settled[field] = value;
      await expect(database.cleanupSettled(sessionId)).rejects.toThrow();
    });
  });

test("terminal status with a retained sandbox is not settled cleanup", async () => {
  await withObserver(async ({ database, settled }) => {
    await database.retainCleanupTarget(sessionId, hostId);
    settled.sandbox_id = sandboxId;
    expect(await database.cleanupSettled(sessionId)).toBe(false);
    settled.sandbox_id = null;
    settled.status = "active";
    expect(await database.cleanupSettled(sessionId)).toBe(false);
  });
});

test("ordinary and already-terminal managed targets retain their exact identity", async () => {
  for (const managed of [false, true])
    await withObserver(async ({ database, target, settled }) => {
      if (managed) {
        target.host_id = null;
        target.sandbox_id = null;
      } else
        for (const row of [target, settled])
          for (const field of [
            "managed_task_id",
            "managed_host_id",
            "managed_sandbox_id",
            "managed_binding_epoch",
          ])
            row[field] = null;
      await database.retainCleanupTarget(sessionId, hostId);
      expect(await database.cleanupSettled(sessionId)).toBe(true);
    });
});

test("unknown target, wrong owner, changed target and query failure do not prove cleanup", async () => {
  await withObserver(async ({ database, target, failQuery }) => {
    target.managed_task_id = "foreign-task";
    await expect(
      database.retainCleanupTarget(sessionId, hostId),
    ).rejects.toThrow("cleanup_exact_owned_target");
    target.managed_task_id = "fixture-task";
    await expect(
      database.retainCleanupTarget(sessionId, otherId),
    ).rejects.toThrow("cleanup_exact_owned_target");
    await database.retainCleanupTarget(sessionId, hostId);
    target.sandbox_id = otherId;
    target.managed_sandbox_id = otherId;
    await expect(
      database.retainCleanupTarget(sessionId, hostId),
    ).rejects.toThrow("cleanup_target_changed");
    failQuery();
    await expect(database.cleanupSettled(sessionId)).rejects.toThrow(
      "observer unavailable",
    );
  });
});
