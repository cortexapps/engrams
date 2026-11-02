import { beforeAll, afterAll, expect, test } from "bun:test";
import { spawn } from "node:child_process";
import { createInterface } from "node:readline";
import { once } from "node:events";
import { Pool } from "pg";
import { and, eq } from "drizzle-orm";
import { effectFixture } from "./mcp-effect-fixture.ts";
import { mcpExecution, mcpApproval } from "../db/schema.ts";
import { reviewContext } from "../mcp/journal-review.ts";
import { verifyMcpTestDatabases } from "./helpers/mcp-test-isolation.ts";

const isolated = await verifyMcpTestDatabases();
const system = new Pool({ connectionString: isolated.system });
beforeAll(async () => {
  await system.query("create schema if not exists mcp_test");
  await system.query(
    `create table if not exists mcp_test.completions (session_id text, call_id text, result_json text not null, attempts integer not null default 1, primary key(session_id,call_id))`,
  );
});
afterAll(() => system.end());
async function until(description: string, check: () => Promise<boolean>, timeout = 20000) {
  const end = Date.now() + timeout;
  while (Date.now() < end) {
    if (await check()) return;
    await Bun.sleep(20);
  }
  throw Error(`timeout: ${description}`);
}
type Fixture = Awaited<ReturnType<typeof effectFixture>>;
async function start(
  f: Fixture,
  executor: string,
  checkpoint?: string,
  resume = false,
  clockOffset = 0,
  eventIdx = "1",
  requestReadFailures = 0,
) {
  const child = spawn(
    process.execPath,
    [new URL("./helpers/mcp-dbos-child.ts", import.meta.url).pathname],
    {
      env: {
        PATH: process.env.PATH,
        ORCHESTRATOR_DATABASE_URL: isolated.app,
        MCP_TEST_DBOS_DATABASE_URL: isolated.system,
        MCP_TEST_DATABASE_NAME: process.env.MCP_TEST_DATABASE_NAME,
        NODE_ENV: "test",
        LOG_LEVEL: "silent",
      },
      stdio: ["pipe", "pipe", "pipe"],
    },
  );
  let reached = false,
    ready = false,
    stderr = "",
    failure: unknown;
  let completions = Promise.resolve();
  const lines = createInterface({ input: child.stdout });
  lines.on("line", (line) => {
    if (line === `CHECKPOINT ${checkpoint}`) reached = true;
    if (line === "READY") ready = true;
    if (line.startsWith("COMPLETE ")) {
      const data: { sessionId: string; toolCallId: string; resultJson: string } = JSON.parse(
        line.slice(9),
      );
      completions = completions
        .then(async () => {
          // Explicit coordinator fixture: fixed-key durable enqueue precedes ACK. No effect proof.
          await system.query(
            `insert into mcp_test.completions(session_id,call_id,result_json) values($1,$2,$3)
          on conflict(session_id,call_id) do update set attempts=mcp_test.completions.attempts+1`,
            [data.sessionId, data.toolCallId, data.resultJson],
          );
          if (checkpoint === "enqueue_before_ack") {
            reached = true;
            return;
          }
          child.stdin.write("ACK\n");
        })
        .catch((error) => {
          failure = error;
        });
    }
  });
  child.stderr.on("data", (value) => {
    stderr += String(value);
  });
  child.stdin.write(
    JSON.stringify({
      remote: f.remote,
      request: f.request,
      deploymentId: f.deploymentId,
      workflowId: executor,
      checkpoint,
      resume,
      clockOffset,
      eventIdx,
      requestReadFailures,
    }) + "\n",
  );
  return {
    async reached() {
      await until(`child ${checkpoint}`, async () => {
        if (failure) throw failure;
        if (child.exitCode !== null)
          throw Error(
            `driver exited ${child.exitCode}: ${stderr.replace(/postgres(?:ql)?:\/\/[^\s]+/g, "[database]")}`,
          );
        return reached;
      });
    },
    async ready() {
      await until("driver ready", async () => {
        if (child.exitCode !== null) throw Error(`driver exited: ${stderr}`);
        return ready;
      });
    },
    continue() {
      child.stdin.write("CONTINUE\n");
    },
    async stop() {
      if (child.exitCode === null && child.signalCode === null) {
        const done = once(child, "exit");
        child.kill("SIGKILL");
        await done;
      }
      await completions;
      lines.close();
    },
  };
}
async function callFor(f: Fixture) {
  return (
    await f.db
      .select()
      .from(mcpExecution)
      .where(
        and(
          eq(mcpExecution.sessionId, f.sessionId),
          eq(mcpExecution.toolCallId, f.request.toolCallId),
        ),
      )
  )[0];
}
async function approve(f: Fixture) {
  const call = await f.driver.acceptToolRequest(f.request);
  await f.driver.driveOnce(call.id);
  const approval = await f.journal.requestApproval(call.id, f.deploymentId, new Date());
  await f.journal.decide(call.id, f.actorId, true, reviewContext(call, approval), new Date());
}
const boundaries = [
  "before_insert",
  "after_insert",
  "before_admission",
  "admission_committed",
  "approval_before_consume",
  "approval_consumed_uncommitted",
  "intent_issued",
  "barrier_committed",
  "before_lease",
  "intent_leased",
  "response_verified",
  "result_committed",
  "before_enqueue",
  "enqueue_before_ack",
  "after_enqueue_ack",
  "receipt_committed",
];
for (const boundary of boundaries)
  test(`actual DBOS driver process restart at ${boundary}`, async () => {
    const insert = boundary === "before_insert" || boundary === "after_insert";
    const f = await effectFixture({ classification: "write" });
    const executor = `mcp-test-${crypto.randomUUID()}`;
    let child: Awaited<ReturnType<typeof start>> | undefined;
    try {
      if (!insert) await approve(f);
      child = await start(f, executor, boundary);
      await child.reached();
      const before = await callFor(f);
      const beforeCalls = (await f.command("stats")).calls;
      const beforeLease = before ? await f.journal.getAdmission(before.id) : null;
      await child.stop();
      const afterBarrier = [
        "barrier_committed",
        "before_lease",
        "intent_leased",
        "response_verified",
      ].includes(boundary);
      child = await start(
        f,
        executor,
        undefined,
        boundary !== "before_insert",
        afterBarrier ? 61000 : 0,
      );
      await child.ready();
      if (insert) {
        await until("recovered schema and approval", async () => {
          const call = await callFor(f);
          return (
            !!call && (await f.journal.getAdmission(call.id))?.schemaDigest === call.requestDigest
          );
        });
        const call = (await callFor(f))!;
        const approval = await f.journal.requestApproval(call.id, f.deploymentId, new Date());
        await f.journal.decide(call.id, f.actorId, true, reviewContext(call, approval), new Date());
      }
      await until("committed completion receipt", async () => {
        const call = await callFor(f);
        return !!call && call.completionEnqueuedVersion === call.version;
      });
      const final = (await callFor(f))!;
      expect(final.originalArguments).toBe(f.request.originalArguments);
      if (before) {
        expect(final.requestDigest).toBe(before.requestDigest);
        expect(final.deadline).toEqual(before.deadline);
      }
      if (beforeLease?.phase === "execute")
        expect(final.intentExpiresAt).toEqual(beforeLease.expiresAt);
      expect(final.state).toBe(afterBarrier ? "outcome_unknown" : "succeeded");
      const finalCalls = (await f.command("stats")).calls;
      expect(finalCalls).toBe(afterBarrier ? beforeCalls : 1);
      expect(finalCalls).toBeLessThanOrEqual(1);
      if (!insert) {
        const approval = (
          await f.db.select().from(mcpApproval).where(eq(mcpApproval.executionId, final.id))
        )[0]!;
        expect(approval.consumedAt).not.toBeNull();
      }
      const completion = (
        await system.query(
          "select * from mcp_test.completions where session_id=$1 and call_id=$2",
          [f.sessionId, f.request.toolCallId],
        )
      ).rows[0];
      expect(completion).toBeDefined();
      const delivered = JSON.parse(completion.result_json);
      if (afterBarrier)
        expect(delivered).toMatchObject({
          source: "runtime",
          outcome: "outcome_unknown",
          delivery: "unavailable",
        });
      else
        expect(delivered).toMatchObject({
          outcome: "succeeded",
          delivery: "available",
          output: { isError: false },
        });
      if (["enqueue_before_ack", "after_enqueue_ack"].includes(boundary))
        expect(completion.attempts).toBeGreaterThanOrEqual(2);
      // Inspect the actual DBOS database, not a serialization mock.
      const status = await system.query(
        "select inputs,output,error from dbos.workflow_status where executor_id=$1",
        [executor],
      );
      expect(status.rowCount).toBeGreaterThan(0);
      for (const row of status.rows) {
        const input = JSON.parse(row.inputs);
        expect(input.json).toHaveLength(1);
        expect(Object.keys(input.json[0]).sort()).toEqual(
          input.json[0].requestEventIdx
            ? ["deadlineMs", "requestEventIdx", "runId", "sessionId", "toolCallId"]
            : ["executionId", "runId", "sessionId", "toolCallId"],
        );
        if (row.output) expect(JSON.parse(row.output).json).toBeNull();
        expect(row.error).toBeNull();
      }
      const durable = JSON.stringify(status.rows);
      expect(durable).not.toContain("known-literal-token");
      expect(durable).not.toContain("originalArguments");
      expect(durable).not.toContain("structuredContent");
      expect(durable).not.toContain("resultJson");
      const steps = await system.query(
        "select o.* from dbos.operation_outputs o join dbos.workflow_status w using(workflow_uuid) where w.executor_id=$1",
        [executor],
      );
      expect(
        steps.rows.every((row) =>
          ["DBOS.sleep", "mcp-request-decision", "McpExecWorkflow"].includes(row.function_name),
        ),
      ).toBe(true);
      expect(JSON.stringify(steps.rows)).not.toContain("known-literal-token");
      expect(JSON.stringify(steps.rows)).not.toContain("structuredContent");
    } finally {
      await child?.stop();
      await system.query("delete from mcp_test.completions where session_id=$1", [f.sessionId]);
      await f.cleanup();
    }
  }, 45000);

for (const eligibility of ["plan", "unknown"])
  test(`actual consumer and DBOS allow reads with ${eligibility} write policy`, async () => {
    const f = await effectFixture({ eligibility });
    let child: Awaited<ReturnType<typeof start>> | undefined;
    try {
      child = await start(f, `mcp-test-${crypto.randomUUID()}`);
      await child.ready();
      await until("read completion", async () => {
        const row = await callFor(f);
        return row?.state === "succeeded" && row.completionEnqueuedVersion === row.version;
      });
      expect((await f.command("stats")).calls).toBe(1);
    } finally {
      await child?.stop();
      await system.query("delete from mcp_test.completions where session_id=$1", [f.sessionId]);
      await f.cleanup();
    }
  }, 30000);

for (const eligibility of ["plan", "unknown"])
  test(`actual consumer completes a ${eligibility} write denial and permits the next read`, async () => {
    const f = await effectFixture({ eligibility, classification: "write" });
    let child: Awaited<ReturnType<typeof start>> | undefined;
    try {
      child = await start(f, `mcp-test-${crypto.randomUUID()}`);
      await child.ready();
      await until(
        "negative completion",
        async () =>
          !!(
            await system.query(
              "select 1 from mcp_test.completions where session_id=$1 and call_id=$2",
              [f.sessionId, f.request.toolCallId],
            )
          ).rowCount,
      );
      expect(await callFor(f)).toBeUndefined();
      expect((await f.command("stats")).calls).toBe(0);
      const denied = (
        await system.query(
          "select result_json from mcp_test.completions where session_id=$1 and call_id=$2",
          [f.sessionId, f.request.toolCallId],
        )
      ).rows[0];
      expect(JSON.parse(denied.result_json)).toMatchObject({
        source: "runtime",
        denial: "write_not_allowed",
        output: { isError: true },
      });
      expect(JSON.parse(denied.result_json).outcome).toBeUndefined();
      await child.stop();
      f.request.toolCallId = "next-read";
      f.request.modelToolName = f.snapshot.immutableBindings[0]!.tools.find(
        (x) => x.tool.classification === "read",
      )!.name;
      child = await start(f, `mcp-test-${crypto.randomUUID()}`, undefined, false, 0, "2");
      await child.ready();
      await until("subsequent read", async () => {
        const row = await callFor(f);
        return row?.state === "succeeded" && row.completionEnqueuedVersion === row.version;
      });
      expect((await f.command("stats")).calls).toBe(1);
    } finally {
      await child?.stop();
      await system.query("delete from mcp_test.completions where session_id=$1", [f.sessionId]);
      await f.cleanup();
    }
  }, 30000);

test("late verified outcome after expiry gets a new completion receipt without another effect", async () => {
  const f = await effectFixture({ classification: "write" });
  let child: Awaited<ReturnType<typeof start>> | undefined;
  try {
    await approve(f);
    child = await start(f, `mcp-test-${crypto.randomUUID()}`, "response_before_persist");
    await child.reached();
    const dispatched = (await callFor(f))!;
    expect(dispatched.state).toBe("dispatching");
    expect((await f.command("stats")).calls).toBe(1);
    await f.journal.expire(f.deploymentId, new Date(dispatched.intentExpiresAt!.getTime() + 1));
    const { makeMcpRecovery } = await import("../mcp/recovery.ts");
    const deliveries: unknown[] = [];
    const runtime = makeMcpRecovery({
      db: f.db,
      driver: f.driver,
      deploymentId: f.deploymentId,
      now: () => new Date(),
      completer: {
        async completeToolCall(req) {
          deliveries.push(JSON.parse(req.resultJson));
          await system.query(
            `insert into mcp_test.completions(session_id,call_id,result_json) values($1,$2,$3) on conflict(session_id,call_id) do update set attempts=mcp_test.completions.attempts+1`,
            [req.sessionId, req.toolCallId, req.resultJson],
          );
        },
      },
    });
    const marker = {
      executionId: dispatched.id,
      sessionId: f.sessionId,
      runId: f.request.runId,
      toolCallId: f.request.toolCallId,
    };
    await runtime.runOnce(marker);
    const unknown = (await callFor(f))!;
    expect(unknown.state).toBe("outcome_unknown");
    expect(unknown.result).toBeNull();
    expect(deliveries).toMatchObject([
      { source: "runtime", outcome: "outcome_unknown", delivery: "unavailable" },
    ]);
    expect(unknown.completionEnqueuedVersion).toBe(unknown.version);
    f.requests.set("new-write", { ...f.request, toolCallId: "new-write" });
    await expect(f.driver.acceptToolRequest(f.requests.get("new-write")!)).rejects.toThrow();
    await f.journal.resolveUnknown(dispatched.id, f.actorId, "acknowledged", new Date());
    const resolved = (await callFor(f))!;
    expect(resolved.state).toBe("outcome_unknown");
    expect(resolved.result).toBeNull();
    expect(resolved.version).toBe(unknown.version);
    child.continue();
    await until("late completion", async () => {
      const row = (await callFor(f))!;
      return row.state === "succeeded" && row.completionEnqueuedVersion === row.version;
    });
    const known = (await callFor(f))!;
    expect(known.version).toBeGreaterThan(unknown.version);
    expect(known.intentId).toBe(dispatched.intentId);
    expect(known.result).toMatchObject({ outcome: "succeeded", delivery: "available" });
    const queued = (
      await system.query(
        "select result_json,attempts from mcp_test.completions where session_id=$1 and call_id=$2",
        [f.sessionId, f.request.toolCallId],
      )
    ).rows[0];
    expect(queued.attempts).toBeGreaterThanOrEqual(2);
    expect(JSON.parse(queued.result_json)).toMatchObject({
      source: "runtime",
      outcome: "outcome_unknown",
    });
    expect((await f.command("stats")).calls).toBe(1);
  } finally {
    await child?.stop();
    await system.query("delete from mcp_test.completions where session_id=$1", [f.sessionId]);
    await f.cleanup();
  }
}, 30000);

for (const classification of ["read", "write"] as const)
  test(`kill after provider send expires unknown and never repeats the ${classification}`, async () => {
    const f = await effectFixture({ classification, response: "hold" });
    const executor = `mcp-test-${crypto.randomUUID()}`;
    let child: Awaited<ReturnType<typeof start>> | undefined;
    try {
      if (classification === "write") await approve(f);
      child = await start(f, executor);
      await child.ready();
      await until("provider tools/call", async () => (await f.command("stats")).calls === 1);
      const sent = (await callFor(f))!;
      expect(sent.state).toBe("dispatching");
      await child.stop();
      await f.command("release");
      child = await start(f, executor, undefined, true, 61000);
      await child.ready();
      await until("unknown completion", async () => {
        const row = (await callFor(f))!;
        return row.state === "outcome_unknown" && row.completionEnqueuedVersion === row.version;
      });
      expect((await f.command("stats")).calls).toBe(1);
      expect((await callFor(f))!.intentId).toBe(sent.intentId);
    } finally {
      await child?.stop();
      await f.command("release");
      await system.query("delete from mcp_test.completions where session_id=$1", [f.sessionId]);
      await f.cleanup();
    }
  }, 30000);

for (const cause of ["policy", "invalid_json"] as const)
  for (const boundary of ["before_enqueue", "enqueue_before_ack"])
    test(`durable ${cause} denial survives ${boundary} and changed request policy`, async () => {
      const f = await effectFixture({ classification: "write", eligibility: "plan" });
      if (cause === "invalid_json") f.request.originalArguments = "{private-invalid-arguments";
      const executor = `mcp-test-${crypto.randomUUID()}`;
      const originalRun = f.request.runId;
      let child: Awaited<ReturnType<typeof start>> | undefined;
      try {
        child = await start(f, executor, boundary);
        await child.reached();
        expect(await callFor(f)).toBeUndefined();
        await child.stop();
        // Both policy and the duplicate event change after the negative decision commits.
        f.remote.eligibility = "write_eligible";
        f.setEligibility("write_eligible");
        f.request.originalArguments = '{"x":1}';
        f.request.runId = "changed-run";
        child = await start(f, executor, undefined, false, 0, "2");
        await child.ready();
        await until(
          "denial redelivery",
          async () =>
            (
              await system.query(
                "select attempts from mcp_test.completions where session_id=$1 and call_id=$2",
                [f.sessionId, f.request.toolCallId],
              )
            ).rows[0]?.attempts >= (boundary === "enqueue_before_ack" ? 2 : 1),
        );
        expect(await callFor(f)).toBeUndefined();
        expect((await f.command("stats")).calls).toBe(0);
        const workflowId = `mcp-request:${f.sessionId}:${f.request.toolCallId}`;
        const state = (
          await system.query("select inputs from dbos.workflow_status where workflow_uuid=$1", [
            workflowId,
          ])
        ).rows[0];
        expect(JSON.parse(state.inputs).json).toEqual([
          {
            sessionId: f.sessionId,
            runId: originalRun,
            toolCallId: f.request.toolCallId,
            requestEventIdx: "1",
            deadlineMs: expect.any(Number),
          },
        ]);
        const steps = (
          await system.query(
            "select output,error from dbos.operation_outputs where workflow_uuid=$1 and function_name='mcp-request-decision'",
            [workflowId],
          )
        ).rows;
        expect(steps).toHaveLength(1);
        expect(JSON.parse(steps[0].output).json).toEqual({
          kind: "denied",
          code: cause === "policy" ? "write_not_allowed" : "invalid_arguments",
        });
        expect(steps[0].error).toBeNull();
        expect(JSON.stringify(steps)).not.toContain("private-invalid-arguments");
        expect(JSON.stringify(steps)).not.toContain("known-literal-token");
      } finally {
        await child?.stop();
        await system.query("delete from mcp_test.completions where session_id=$1", [f.sessionId]);
        await f.cleanup();
      }
    }, 30000);

test("transient request read retries with only a fixed error in DBOS", async () => {
  const f = await effectFixture();
  const executor = `mcp-test-${crypto.randomUUID()}`;
  let child: Awaited<ReturnType<typeof start>> | undefined;
  try {
    child = await start(f, executor, undefined, false, 0, "1", 1);
    await child.ready();
    await until("retry completes", async () => {
      const row = await callFor(f);
      return row?.state === "succeeded" && row.completionEnqueuedVersion === row.version;
    });
    const steps = (
      await system.query(
        "select output,error from dbos.operation_outputs where workflow_uuid=$1 and function_name='mcp-request-decision' order by function_id",
        [`mcp-request:${f.sessionId}:${f.request.toolCallId}`],
      )
    ).rows;
    expect(steps).toHaveLength(2);
    expect(steps[0].error).toContain("MCP request acceptance unavailable");
    expect(steps[1].error).toBeNull();
    expect(JSON.stringify(steps)).not.toContain("private-control-error");
    expect(JSON.stringify(steps)).not.toContain("known-literal-token");
    expect(JSON.stringify(steps)).not.toContain("originalArguments");
    expect((await f.command("stats")).calls).toBe(1);
  } finally {
    await child?.stop();
    await system.query("delete from mcp_test.completions where session_id=$1", [f.sessionId]);
    await f.cleanup();
  }
}, 30000);

test("request decision restart keeps its fixed deadline and expires without an effect", async () => {
  const f = await effectFixture();
  const executor = `mcp-test-${crypto.randomUUID()}`;
  const workflowId = `mcp-request:${f.sessionId}:${f.request.toolCallId}`;
  let child: Awaited<ReturnType<typeof start>> | undefined;
  try {
    child = await start(f, executor, "request_read_failed", false, 0, "1", 1);
    await child.reached();
    const first = (
      await system.query("select inputs from dbos.workflow_status where workflow_uuid=$1", [
        workflowId,
      ])
    ).rows[0].inputs;
    expect(JSON.parse(first).json[0].deadlineMs).toBeNumber();
    await child.stop();
    child = await start(f, executor, undefined, false, 931000);
    await child.ready();
    await until(
      "expired denial",
      async () =>
        !!(
          await system.query(
            "select 1 from mcp_test.completions where session_id=$1 and call_id=$2",
            [f.sessionId, f.request.toolCallId],
          )
        ).rowCount,
    );
    expect(await callFor(f)).toBeUndefined();
    expect((await f.command("stats")).calls).toBe(0);
    const completion = (
      await system.query(
        "select result_json from mcp_test.completions where session_id=$1 and call_id=$2",
        [f.sessionId, f.request.toolCallId],
      )
    ).rows[0];
    expect(JSON.parse(completion.result_json).denial).toBe("deadline_expired");
    const restored = (
      await system.query("select inputs from dbos.workflow_status where workflow_uuid=$1", [
        workflowId,
      ])
    ).rows[0].inputs;
    expect(restored).toBe(first);
  } finally {
    await child?.stop();
    await system.query("delete from mcp_test.completions where session_id=$1", [f.sessionId]);
    await f.cleanup();
  }
}, 30000);

test("changed duplicate after accepted completion retains the first workflow and result", async () => {
  const f = await effectFixture();
  let child: Awaited<ReturnType<typeof start>> | undefined;
  try {
    child = await start(f, `mcp-test-${crypto.randomUUID()}`);
    await child.ready();
    await until("first receipt", async () => {
      const row = await callFor(f);
      return row?.state === "succeeded" && row.completionEnqueuedVersion === row.version;
    });
    const first = (await callFor(f))!;
    const delivered = (
      await system.query(
        "select result_json,attempts from mcp_test.completions where session_id=$1 and call_id=$2",
        [f.sessionId, f.request.toolCallId],
      )
    ).rows[0];
    await child.stop();
    f.request.runId = "changed-run";
    f.request.originalArguments = "{invalid-duplicate}";
    child = await start(f, `mcp-test-${crypto.randomUUID()}`, undefined, false, 0, "2");
    await child.ready();
    expect(await callFor(f)).toEqual(first);
    expect(
      (
        await system.query(
          "select result_json,attempts from mcp_test.completions where session_id=$1 and call_id=$2",
          [f.sessionId, f.request.toolCallId],
        )
      ).rows[0],
    ).toEqual(delivered);
    expect((await f.command("stats")).calls).toBe(1);
  } finally {
    await child?.stop();
    await system.query("delete from mcp_test.completions where session_id=$1", [f.sessionId]);
    await f.cleanup();
  }
}, 30000);
