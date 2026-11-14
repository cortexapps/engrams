import { expect, test } from "bun:test";
import { eq } from "drizzle-orm";
import { drizzle } from "drizzle-orm/node-postgres";
import { Pool } from "pg";
import * as schema from "../db/schema.ts";
import { makeMcpAdmission } from "../mcp/admission.ts";
import { reviewFixture } from "./mcp-review-fixture.ts";
import { verifyMcpTestDatabases } from "./helpers/mcp-test-isolation.ts";

for (const limit of ["session", "connection"] as const) {
  test(`global ${limit} budget serializes independent clients across worker deployments`, async () => {
    const { app } = await verifyMcpTestDatabases();
    const f = await reviewFixture();
    const pool = new Pool({ connectionString: app, max: 10 });
    const other = makeMcpAdmission(drizzle(pool, { schema }));
    const local = makeMcpAdmission(f.db);
    const maximum = limit === "session" ? 4 : 8;
    const deployments = Array.from({ length: maximum + 1 }, () => crypto.randomUUID());
    try {
      const calls = [];
      for (let i = 0; i < maximum + 1; i++) {
        const toolCallId = `global-${i}`;
        const call = await f.journal.create(
          {
            ...f.input,
            toolCallId,
            requestPolicyContext: { ...f.input.requestPolicyContext, toolCallId },
          },
          { kind: "approval" },
          f.now,
        );
        if (limit === "connection") {
          const independentSessionCall = {
            ...call,
            id: crypto.randomUUID(),
            sessionId: crypto.randomUUID(),
          };
          await f.db.insert(schema.taskSession).values({
            taskId: f.taskId,
            sessionId: independentSessionCall.sessionId,
            profileId: f.profileId,
            integrationPrincipalId: f.actorId,
          });
          await f.db.insert(schema.mcpExecution).values(independentSessionCall);
          calls.push(independentSessionCall);
          continue;
        }
        calls.push(call);
      }
      const states = await Promise.all(
        calls
          .slice(0, maximum + 1)
          .map((call, i) =>
            (i % 2 ? other : local).reserve(call.id, deployments[i]!, "schema", f.now),
          ),
      );
      expect(states.filter((state) => state === "admitted")).toHaveLength(maximum);
      expect(states.filter((state) => state === "queued")).toHaveLength(1);
      // A separate session and connection can still progress while the first is saturated.
      const independent = await reviewFixture();
      try {
        const call = await independent.journal.create(
          independent.input,
          { kind: "approval" },
          f.now,
        );
        expect(await other.reserve(call.id, independent.deploymentId, "schema", f.now)).toBe(
          "admitted",
        );
      } finally {
        await independent.cleanup();
      }
    } finally {
      await f.db.delete(schema.task).where(eq(schema.task.id, f.taskId));
      for (const id of deployments)
        await f.db.delete(schema.mcpAdmissionScope).where(eq(schema.mcpAdmissionScope.id, id));
      await f.cleanup();
      await pool.end();
    }
  });
}

// Exercise the production reserve and cancellation transactions, including the
// connection FOR KEY SHARE lock PostgreSQL takes for the first admission INSERT.
test("initial admission and cancellation keep connection before execution without an FK deadlock", async () => {
  const { app } = await verifyMcpTestDatabases();
  const f = await reviewFixture();
  const pool = new Pool({ connectionString: app, max: 2 });
  const first = await pool.connect(),
    second = await pool.connect();
  const { makeMcpJournal } = await import("../mcp/journal.ts");
  function signal() {
    let resolve!: () => void;
    const promise = new Promise<void>((done) => {
      resolve = done;
    });
    return { promise, resolve };
  }
  const executionLocked = signal(),
    cancellationConnectionAttempt = signal(),
    cancellationExecutionAttempt = signal();
  let admissionHasConnectionLock = false;
  function intercept(client: typeof first, lane: "admission" | "cancel") {
    return new Proxy(client, {
      get(target, key, receiver) {
        if (key !== "query") return Reflect.get(target, key, receiver);
        return async (query: string | import("pg").QueryConfig, values?: unknown[]) => {
          const text = typeof query === "string" ? query : query.text;
          const locksConnection =
            text.includes('from "integration_connection"') && text.includes(" for ");
          const locksExecution =
            text.includes('from "mcp_execution"') && text.endsWith(" for update");
          if (lane === "cancel" && locksConnection) cancellationConnectionAttempt.resolve();
          if (lane === "cancel" && locksExecution) cancellationExecutionAttempt.resolve();
          const result = await target.query(query, values);
          if (lane === "admission" && locksConnection) admissionHasConnectionLock = true;
          if (lane === "admission" && locksExecution) {
            executionLocked.resolve();
            // With connection-first locking cancellation waits on the connection.
            // Without it, let cancellation lock the connection and wait on this
            // execution before the admission INSERT checks its connection FK.
            await (admissionHasConnectionLock
              ? cancellationConnectionAttempt.promise
              : cancellationExecutionAttempt.promise);
          }
          return result;
        };
      },
    });
  }
  try {
    await first.query("set statement_timeout = '3s'");
    await second.query("set statement_timeout = '3s'");
    const call = await f.journal.create(f.input, { kind: "approval" }, f.now);
    const admission = makeMcpAdmission(drizzle(intercept(first, "admission"), { schema }));
    const cancellation = makeMcpJournal(drizzle(intercept(second, "cancel"), { schema }));
    const reserved = admission.reserve(call.id, f.deploymentId, "schema", f.now);
    const canceled = executionLocked.promise.then(() =>
      cancellation.cancelInternal(call.id, "interrupt", f.now),
    );
    const results = await Promise.allSettled([reserved, canceled]);
    function errorCode(error: unknown): string {
      if (error && typeof error === "object") {
        if ("code" in error && typeof error.code === "string") return error.code;
        if ("cause" in error) return errorCode(error.cause);
      }
      return "unknown";
    }
    expect(
      results.map((result) =>
        result.status === "fulfilled" ? "fulfilled" : errorCode(result.reason),
      ),
    ).toEqual(["fulfilled", "fulfilled"]);
    expect(admissionHasConnectionLock).toBe(true);
    expect(await f.journal.getAdmission(call.id)).toBeNull();
    const [stored] = await f.db
      .select()
      .from(schema.mcpExecution)
      .where(eq(schema.mcpExecution.id, call.id));
    expect(stored!.state).toBe("canceled");
  } finally {
    first.release();
    second.release();
    await pool.end();
    await f.cleanup();
  }
}, 8_000);
