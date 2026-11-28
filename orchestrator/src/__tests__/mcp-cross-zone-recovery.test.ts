import { expect, test } from "bun:test";
import { eq } from "drizzle-orm";
import { drizzle } from "drizzle-orm/node-postgres";
import { Pool } from "pg";
import * as schema from "../db/schema.ts";
import { makeMcpJournal } from "../mcp/journal.ts";
import { makeMcpRecoveryScanner } from "../mcp/recovery-scanner.ts";
import { schemaVerifiedFixture } from "./mcp-review-fixture.ts";
import { verifyMcpTestDatabases } from "./helpers/mcp-test-isolation.ts";

test("sixteenth approval is global to the session across racing deployment clients", async () => {
  const { app } = await verifyMcpTestDatabases();
  const f = await schemaVerifiedFixture();
  const pool = new Pool({ connectionString: app, max: 2 });
  const other = makeMcpJournal(drizzle(pool, { schema }));
  const deployments = [f.deploymentId, crypto.randomUUID()];
  try {
    const calls = [];
    for (let i = 0; i < 17; i++) {
      const toolCallId = `approval-${i}`;
      const call = await f.journal.create(
        {
          ...f.input,
          toolCallId,
          requestPolicyContext: { ...f.input.requestPolicyContext, toolCallId },
        },
        { kind: "approval" },
        f.now,
      );
      calls.push(call);
      if (i < 15) await f.journal.requestApproval(call.id, deployments[i % 2]!, f.now);
    }
    const outcomes = await Promise.allSettled([
      f.journal.requestApproval(calls[15]!.id, deployments[0]!, f.now),
      other.requestApproval(calls[16]!.id, deployments[1]!, f.now),
    ]);
    expect(outcomes.filter((o) => o.status === "fulfilled")).toHaveLength(1);
    expect(outcomes.filter((o) => o.status === "rejected")).toHaveLength(1);
    expect(await f.journal.pending(f.sessionId, f.actorId, f.now)).toHaveLength(16);
  } finally {
    await f.cleanup();
    await f.db
      .delete(schema.mcpAdmissionScope)
      .where(eq(schema.mcpAdmissionScope.id, deployments[1]!));
    await pool.end();
  }
});

test("disabled scanner expires persisted removed deployment approvals and admissions", async () => {
  await verifyMcpTestDatabases();
  const f = await schemaVerifiedFixture();
  const currentDeployment = crypto.randomUUID();
  try {
    const call = await f.journal.create(f.input, { kind: "approval" }, f.now);
    await f.journal.requestApproval(call.id, f.deploymentId, f.now);
    const base = makeMcpJournal(f.db);
    const toolCallId = "active-schema";
    const active = await base.create(
      {
        ...f.input,
        toolCallId,
        requestPolicyContext: { ...f.input.requestPolicyContext, toolCallId },
      },
      { kind: "approval" },
      f.now,
    );
    await base.admitSchema(active.id, f.deploymentId, f.now);
    const scanner = makeMcpRecoveryScanner({
      journal: f.journal,
      deploymentId: currentDeployment,
      runtime: () => undefined,
      now: () => new Date(f.now.getTime() + 600_001),
    });
    await scanner.runOnce();
    const [approval] = await f.db
      .select()
      .from(schema.mcpApproval)
      .where(eq(schema.mcpApproval.executionId, call.id));
    expect(approval!.decision).toBe("expired");
    expect((await f.journal.get(call.id))!.state).toBe("canceled");
    expect((await f.journal.getAdmission(active.id))!.state).toBe("released");
  } finally {
    await f.cleanup();
    await f.db
      .delete(schema.mcpAdmissionScope)
      .where(eq(schema.mcpAdmissionScope.id, currentDeployment));
  }
});
