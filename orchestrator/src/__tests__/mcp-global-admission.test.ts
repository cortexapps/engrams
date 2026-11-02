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
          await f.db
            .insert(schema.taskSession)
            .values({
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
