import { expect, test } from "bun:test";
import { Code, ConnectError } from "@connectrpc/connect";
import { eq } from "drizzle-orm";
import { effectFixture } from "./mcp-effect-fixture.ts";
import { mcpExecution, mcpAdmissionScope } from "../db/schema.ts";
import { makeMcpRecovery } from "../mcp/recovery.ts";

import { verifyMcpTestDatabases } from "./helpers/mcp-test-isolation.ts";
await verifyMcpTestDatabases();

test("committed result is redelivered after enqueue failure without another effect", async () => {
  const f = await effectFixture();
  try {
    let fail = true;
    const delivered: string[] = [];
    const runtime = makeMcpRecovery({
      db: f.db,
      driver: f.driver,
      deploymentId: f.deploymentId,
      now: () => new Date(),
      completer: {
        async completeToolCall(input) {
          if (fail) throw Error("transport failure");
          delivered.push(input.resultJson);
        },
      },
    });
    const marker = await runtime.accept(f.request);
    await expect(runtime.runOnce(marker)).rejects.toThrow("MCP completion unavailable");
    const committed = (await f.journal.get(marker.executionId))!;
    expect(committed.state).toBe("succeeded");
    expect(committed.completionEnqueuedVersion).toBe(0);
    fail = false;
    expect(await runtime.runOnce(marker)).toBe("done");
    expect(JSON.parse(delivered[0]!)).toEqual({ ...committed.result, resultUnavailable: false });
    expect((await f.journal.get(marker.executionId))!.completionEnqueuedVersion).toBe(
      committed.version,
    );
    expect(await runtime.runOnce(marker)).toBe("done");
    expect(delivered).toHaveLength(1);
    expect((await f.command("stats")).calls).toBe(1);
    expect((await runtime.pending()).map((x) => x.executionId)).not.toContain(marker.executionId);
    await expect(
      f.db
        .update(mcpExecution)
        .set({ completionEnqueuedVersion: 0 })
        .where(eq(mcpExecution.id, marker.executionId))
        .execute(),
    ).rejects.toThrow();
    await expect(
      f.db
        .update(mcpExecution)
        .set({ completionEnqueuedVersion: committed.version + 1 })
        .where(eq(mcpExecution.id, marker.executionId))
        .execute(),
    ).rejects.toThrow();
  } finally {
    await f.cleanup();
  }
}, 20000);

test("recovery expires unadmitted rows and delivers a local cancellation", async () => {
  const f = await effectFixture({ classification: "write" });
  try {
    let now = new Date();
    const runtime = makeMcpRecovery({
      db: f.db,
      driver: f.driver,
      deploymentId: f.deploymentId,
      now: () => now,
      completer: {
        async completeToolCall(input) {
          expect(JSON.parse(input.resultJson)).toMatchObject({
            source: "runtime",
            outcome: "canceled",
            delivery: "unavailable",
          });
        },
      },
    });
    const marker = await runtime.accept(f.request);
    const call = (await f.journal.get(marker.executionId))!;
    now = new Date(call.deadline.getTime() + 1);
    const { makeMcpRecoveryScanner } = await import("../mcp/recovery-scanner.ts");
    await makeMcpRecoveryScanner({
      deploymentId: f.deploymentId,
      journal: f.journal,
      runtime: () => undefined,
      now: () => now,
    }).runOnce();
    expect((await f.journal.get(call.id))!.state).toBe("canceled");
    expect((await runtime.pending()).map((x) => x.executionId)).toContain(call.id);
    expect(await runtime.runOnce(marker)).toBe("done");
    expect((await runtime.pending()).map((x) => x.executionId)).not.toContain(call.id);
  } finally {
    await f.cleanup();
  }
}, 20000);

for (const response of [
  "error",
  "image",
  "normalized-limit",
  "malformed",
  "oversize",
  "local_validation",
])
  test(`completion retains the committed ${response} distinction`, async () => {
    const f = await effectFixture({
      response: response === "local_validation" ? "normal" : response,
    });
    try {
      if (response === "local_validation") f.request.originalArguments = '{"x":"wrong"}';
      let result: unknown;
      const runtime = makeMcpRecovery({
        db: f.db,
        driver: f.driver,
        deploymentId: f.deploymentId,
        now: () => new Date(),
        completer: {
          async completeToolCall(request) {
            result = JSON.parse(request.resultJson);
          },
        },
      });
      const marker = await runtime.accept(f.request);
      expect(await runtime.runOnce(marker)).toBe("done");
      const call = (await f.journal.get(marker.executionId))!;
      expect(result).toEqual({ ...call.result, resultUnavailable: call.resultUnavailable });
      if (response === "local_validation") {
        expect(result).toMatchObject({
          source: "local_validation",
          delivery: "available",
          output: { isError: true },
        });
        expect(call.intentId).toBeNull();
        expect((await f.command("stats")).calls).toBe(0);
      } else {
        expect((await f.command("stats")).calls).toBe(1);
        if (response === "error")
          expect(result).toMatchObject({ outcome: "failed", output: { isError: true } });
        if (response === "image" || response === "normalized-limit")
          expect(result).toMatchObject({
            outcome: "succeeded",
            delivery: "unavailable",
            resultUnavailable: true,
          });
      }
      await runtime.runOnce(marker);
      expect(JSON.stringify(result)).not.toContain("known-literal-token");
    } finally {
      await f.cleanup();
    }
  }, 20000);

test("bounded keyset scans reach calls behind a full page and stop after shutdown", async () => {
  const f = await effectFixture();
  try {
    const runtime = makeMcpRecovery({
      db: f.db,
      driver: f.driver,
      deploymentId: f.deploymentId,
      now: () => new Date(),
      completer: {
        async completeToolCall() {
          throw Error("not run");
        },
      },
    });
    const expected = new Set<string>();
    for (let i = 0; i < 130; i++) {
      const request = { ...f.request, toolCallId: `queued-${i}` };
      f.requests.set(request.toolCallId, request);
      expected.add((await runtime.accept(request)).executionId);
    }
    const { makeMcpRecoveryScanner } = await import("../mcp/recovery-scanner.ts");
    const scheduled = new Set<string>();
    let calls = 0;
    const scanner = makeMcpRecoveryScanner({
      deploymentId: f.deploymentId,
      journal: f.journal,
      runtime: () => runtime,
      startWorkflow: async (ref) => {
        calls++;
        expect(Object.keys(ref).sort()).toEqual([
          "executionId",
          "runId",
          "sessionId",
          "toolCallId",
        ]);
        scheduled.add(ref.executionId);
      },
    });
    await scanner.runOnce();
    expect(calls).toBe(128);
    for (let i = 0; i < 4 && [...expected].some((id) => !scheduled.has(id)); i++)
      await scanner.runOnce();
    expect([...expected].every((id) => scheduled.has(id))).toBe(true);
    await scanner.start();
    await scanner.stop();
    const stopped = calls;
    await Bun.sleep(1100);
    expect(calls).toBe(stopped);
    expect((await f.command("stats")).calls).toBe(0);
  } finally {
    await f.cleanup();
  }
}, 20000);

test("already-completed guest projection cannot replace or remove journal truth", async () => {
  const f = await effectFixture();
  try {
    const call = await f.driver.acceptToolRequest(f.request);
    const committed = await f.driver.driveOnce(call.id);
    const { makePendingToolCallStore } = await import("../tools/pending-tool-calls.ts");
    const pending = makePendingToolCallStore(f.db);
    await pending.recordRequested({
      sessionId: f.sessionId,
      toolCallId: f.request.toolCallId,
      toolName: f.request.modelToolName,
      handling: "handled",
      requestedAt: new Date(),
    });
    await pending.markCompleted(f.sessionId, f.request.toolCallId, new Date());
    const runtime = makeMcpRecovery({
      db: f.db,
      driver: f.driver,
      deploymentId: f.deploymentId,
      now: () => new Date(),
      // The coordinator acknowledges and drops results for an already-completed call.
      completer: {
        async completeToolCall() {
          return {};
        },
      },
    });
    await runtime.runOnce({
      executionId: call.id,
      sessionId: f.sessionId,
      runId: f.request.runId,
      toolCallId: f.request.toolCallId,
    });
    const retained = (await f.journal.get(call.id))!;
    expect(retained.result).toEqual(committed.result);
    expect(retained.state).toBe("succeeded");
    expect(retained.completionEnqueuedVersion).toBe(retained.version);
    expect((await f.command("stats")).calls).toBe(1);
  } finally {
    await f.cleanup();
  }
}, 20000);

test("test topology rejects a missing or primary-looking DBOS URL before connection", async () => {
  const saved = process.env.MCP_TEST_DBOS_DATABASE_URL;
  try {
    delete process.env.MCP_TEST_DBOS_DATABASE_URL;
    await expect(verifyMcpTestDatabases()).rejects.toThrow(
      "explicit isolated MCP test databases required",
    );
    process.env.MCP_TEST_DBOS_DATABASE_URL = "postgresql://127.0.0.1:5435/engram_orchestrator";
    await expect(verifyMcpTestDatabases()).rejects.toThrow("MCP test database isolation mismatch");
  } finally {
    process.env.MCP_TEST_DBOS_DATABASE_URL = saved;
  }
});

test("permanent input and policy denials have explicit typed codes", async () => {
  const f = await effectFixture({ classification: "write", eligibility: "plan" });
  try {
    await expect(f.driver.acceptToolRequest(f.request)).rejects.toMatchObject({
      code: "write_not_allowed",
    });
    await expect(
      f.driver.acceptToolRequest({ ...f.request, originalArguments: "{broken" }),
    ).rejects.toMatchObject({ code: "invalid_arguments" });
  } finally {
    await f.cleanup();
  }
}, 20000);

test("request decision uses the exact int64 event and preserves first arguments and identity", async () => {
  const f = await effectFixture();
  try {
    const index = 9007199254740993n;
    let fail = true;
    let reads = 0;
    const original = '  {"x": 1}  ';
    f.request.originalArguments = original;
    const runtime = makeMcpRecovery({
      db: f.db,
      driver: f.driver,
      deploymentId: f.deploymentId,
      now: () => new Date(),
      async readRequestEvent(sessionId, idx) {
        expect(sessionId).toBe(f.sessionId);
        expect(idx).toBe(index);
        reads++;
        if (fail) throw Error("private-control-error");
        return {
          idx,
          kind: "tool_call_requested",
          payloadJson: JSON.stringify({
            tool_call_id: f.request.toolCallId,
            run_id: f.request.runId,
            name: f.request.modelToolName,
            args_json: original,
          }),
        };
      },
    });
    const ref = {
      sessionId: f.sessionId,
      runId: f.request.runId,
      toolCallId: f.request.toolCallId,
      requestEventIdx: String(index),
      deadlineMs: Date.now() + 930000,
    };
    await expect(runtime.decideRequest(ref)).rejects.toThrow("MCP request acceptance unavailable");
    fail = false;
    const first = await runtime.decideRequest(ref);
    expect(first.kind).toBe("accepted");
    if (first.kind !== "accepted") throw Error("expected accepted references");
    const call = (await f.journal.get(first.execution.executionId))!;
    expect(call.originalArguments).toBe(original);
    const duplicate = await runtime.decideRequest({
      ...ref,
      runId: "changed-run",
      requestEventIdx: "12",
      deadlineMs: 0,
    });
    expect(duplicate).toEqual(first);
    expect(reads).toBe(2);
    expect((await f.journal.get(call.id))!.deadline).toEqual(call.deadline);
    expect((await f.command("stats")).calls).toBe(0);
    expect(JSON.stringify(first)).not.toContain("originalArguments");
  } finally {
    await f.cleanup();
  }
});

test("request decision rejects mismatched event references with a fixed private error", async () => {
  const f = await effectFixture();
  try {
    let kind = "tool_call_requested",
      idx = 1n,
      run = f.request.runId,
      call = f.request.toolCallId;
    const runtime = makeMcpRecovery({
      db: f.db,
      driver: f.driver,
      deploymentId: f.deploymentId,
      now: () => new Date(),
      async readRequestEvent() {
        return {
          idx,
          kind,
          payloadJson: JSON.stringify({
            run_id: run,
            tool_call_id: call,
            name: f.request.modelToolName,
            args_json: f.request.originalArguments,
          }),
        };
      },
    });
    const ref = {
      sessionId: f.sessionId,
      runId: f.request.runId,
      toolCallId: f.request.toolCallId,
      requestEventIdx: "1",
      deadlineMs: Date.now() + 930000,
    };
    for (const bad of ["0", "-1", "1.1", "9223372036854775808"])
      await expect(runtime.decideRequest({ ...ref, requestEventIdx: bad })).rejects.toThrow(
        "MCP request acceptance unavailable",
      );
    idx = 2n;
    await expect(runtime.decideRequest(ref)).rejects.toThrow("MCP request acceptance unavailable");
    idx = 1n;
    kind = "tool_call_completed";
    await expect(runtime.decideRequest(ref)).rejects.toThrow("MCP request acceptance unavailable");
    kind = "tool_call_requested";
    run = "changed-run";
    await expect(runtime.decideRequest(ref)).rejects.toThrow("MCP request acceptance unavailable");
    run = f.request.runId;
    call = "changed-call";
    await expect(runtime.decideRequest(ref)).rejects.toThrow("MCP request acceptance unavailable");
    expect((await f.command("stats")).calls).toBe(0);
  } finally {
    await f.cleanup();
  }
});

test("transient binding authority failure is retried while a verified absent tool is denied", async () => {
  const f = await effectFixture();
  try {
    const runtime = makeMcpRecovery({
      db: f.db,
      driver: f.driver,
      deploymentId: f.deploymentId,
      now: () => new Date(),
      async readRequestEvent(_sessionId, idx) {
        return {
          idx,
          kind: "tool_call_requested",
          payloadJson: JSON.stringify({
            run_id: f.request.runId,
            tool_call_id: f.request.toolCallId,
            name: f.request.modelToolName,
            args_json: f.request.originalArguments,
          }),
        };
      },
    });
    const ref = {
      sessionId: f.sessionId,
      runId: f.request.runId,
      toolCallId: f.request.toolCallId,
      requestEventIdx: "1",
      deadlineMs: Date.now() + 930000,
    };
    f.failAuthorityReads(1);
    await expect(runtime.decideRequest(ref)).rejects.toThrow("MCP request acceptance unavailable");
    expect((await runtime.decideRequest(ref)).kind).toBe("accepted");
    f.request.toolCallId = "absent-tool";
    f.request.modelToolName = "mcp_unpublished_tool";
    expect(
      await runtime.decideRequest({
        ...ref,
        toolCallId: f.request.toolCallId,
        requestEventIdx: "2",
      }),
    ).toEqual({ kind: "denied", code: "tool_unavailable" });
    expect((await f.command("stats")).calls).toBe(0);
  } finally {
    await f.cleanup();
  }
});

for (const disposition of ["missing", "terminal", "host_lost", "conflict", "transport"] as const)
  test(`completion ${disposition} settles only authoritative target loss`, async () => {
    const f = await effectFixture();
    try {
      let fail = true;
      const error = new ConnectError(
        "private-delivery-error",
        disposition === "missing"
          ? Code.NotFound
          : disposition === "transport"
            ? Code.Unavailable
            : Code.FailedPrecondition,
        new Headers({
          "engram-error-slug": disposition === "terminal" ? "snapshot_invalidated" : disposition,
        }),
      );
      const runtime = makeMcpRecovery({
        db: f.db,
        driver: f.driver,
        deploymentId: f.deploymentId,
        now: () => new Date(),
        completer: {
          async completeToolCall() {
            if (fail) throw error;
          },
        },
      });
      const marker = await runtime.accept(f.request);
      const ref = {
        sessionId: f.sessionId,
        runId: f.request.runId,
        toolCallId: f.request.toolCallId,
        requestEventIdx: "1",
        deadlineMs: Date.now() + 930000,
      };
      if (disposition === "missing" || disposition === "terminal") {
        expect(await runtime.runOnce(marker)).toBe("done");
        await runtime.deliverDenial(ref, "write_not_allowed");
      } else {
        await expect(runtime.runOnce(marker)).rejects.toThrow("MCP completion unavailable");
        await expect(runtime.deliverDenial(ref, "write_not_allowed")).rejects.toThrow(
          "MCP denial completion unavailable",
        );
        expect((await f.journal.get(marker.executionId))!.completionEnqueuedVersion).toBe(0);
        expect((await runtime.pending()).map((x) => x.executionId)).toContain(marker.executionId);
        fail = false;
        expect(await runtime.runOnce(marker)).toBe("done");
      }
      const settled = (await f.journal.get(marker.executionId))!;
      expect(settled.state).toBe("succeeded");
      expect(settled.completionEnqueuedVersion).toBe(settled.version);
      expect((await runtime.pending()).map((x) => x.executionId)).not.toContain(marker.executionId);
      expect((await f.command("stats")).calls).toBe(1);
    } finally {
      await f.cleanup();
    }
  }, 20000);

test("disabled dispatch construction drains persisted calls without any dispatch dependency", async () => {
  const { schemaVerifiedFixture } = await import("./mcp-review-fixture.ts");
  const { makeMcpRecoveryScanner } = await import("../mcp/recovery-scanner.ts");
  const f = await schemaVerifiedFixture();
  const recoveryDeploymentId = crypto.randomUUID();
  try {
    const call = await f.journal.create(f.input, { kind: "approval" }, f.now);
    const delivered: string[] = [];
    const runtime = makeMcpRecovery({
      db: f.db,
      deploymentId: recoveryDeploymentId,
      now: () => f.now,
      completer: {
        async completeToolCall(input) {
          delivered.push(input.resultJson);
        },
      },
      listLifecycleEvents: async () => [],
    });
    const scanner = makeMcpRecoveryScanner({
      journal: f.journal,
      deploymentId: recoveryDeploymentId,
      runtime: () => runtime,
      now: () => f.now,
      startWorkflow: async (ref) => {
        if (ref.sessionId === f.sessionId) await runtime.runOnce(ref);
      },
    });
    await scanner.runLifecycleOnce();
    await scanner.runOnce();
    const retained = (await f.journal.get(call.id))!;
    expect(retained.state).toBe("canceled");
    expect(retained.intentId).toBeNull();
    expect(retained.completionEnqueuedVersion).toBe(retained.version);
    expect(JSON.parse(delivered[0]!)).toMatchObject({
      source: "runtime",
      outcome: "canceled",
      delivery: "unavailable",
    });
    await expect(
      runtime.accept({ ...f.input, modelToolName: f.input.resolvedTool.modelToolName }),
    ).rejects.toMatchObject({ code: "disabled" });
    expect(
      await runtime.decideRequest({
        sessionId: f.sessionId,
        runId: f.input.runId,
        toolCallId: "new-call",
        requestEventIdx: "1",
        deadlineMs: f.now.getTime() + 1000,
      }),
    ).toEqual({ kind: "denied", code: "disabled" });
    await expect(Promise.resolve().then(() => runtime.abortExecution(call.id))).rejects.toThrow(
      "MCP cancellation unavailable",
    );
    expect(delivered).toHaveLength(1);
  } finally {
    await f.db.delete(mcpAdmissionScope).where(eq(mcpAdmissionScope.id, recoveryDeploymentId));
    await f.cleanup();
  }
});

test("disabled recovery scanner settles committed results across deployment identities without another effect", async () => {
  const f = await effectFixture();
  const recoveryDeploymentId = crypto.randomUUID();
  try {
    const call = await f.driver.acceptToolRequest(f.request);
    const committed = await f.driver.driveOnce(call.id);
    const leases = f.leases,
      events = [...f.events];
    f.setEnabled(false);
    let attempts = 0;
    const deliveries: string[] = [];
    const { makeMcpRecoveryScanner } = await import("../mcp/recovery-scanner.ts");
    const runtime = makeMcpRecovery({
      db: f.db,
      deploymentId: recoveryDeploymentId,
      now: () => new Date(),
      completer: {
        async completeToolCall(input) {
          attempts++;
          deliveries.push(input.resultJson);
          if (attempts === 1) throw Error("private-transport-error");
        },
      },
    });
    const scanner = makeMcpRecoveryScanner({
      journal: f.journal,
      deploymentId: recoveryDeploymentId,
      runtime: () => runtime,
      startWorkflow: async (ref) => {
        if (ref.sessionId === f.sessionId) await runtime.runOnce(ref);
      },
    });
    await expect(scanner.runOnce()).rejects.toThrow("MCP completion unavailable");
    expect((await f.journal.get(call.id))!.completionEnqueuedVersion).toBe(0);
    await scanner.runOnce();
    expect((await f.journal.get(call.id))!.completionEnqueuedVersion).toBe(committed.version);
    expect((await f.journal.get(call.id))!.result).toEqual(committed.result);
    expect(f.leases).toBe(leases);
    expect(f.events).toEqual(events);
    expect((await f.command("stats")).calls).toBe(1);
    expect(attempts).toBe(2);
    expect(JSON.parse(deliveries[1]!)).toEqual({
      ...committed.result,
      resultUnavailable: committed.resultUnavailable,
    });
  } finally {
    await f.db.delete(mcpAdmissionScope).where(eq(mcpAdmissionScope.id, recoveryDeploymentId));
    await f.cleanup();
  }
}, 20000);

test("live dispatch disable drains an accepted call without driving or leasing it", async () => {
  const f = await effectFixture();
  try {
    let enabled = true,
      drives = 0,
      aborts = 0;
    const runtime = makeMcpRecovery({
      db: f.db,
      deploymentId: f.deploymentId,
      now: () => new Date(),
      dispatchEnabled: () => enabled,
      driver: {
        ...f.driver,
        async driveOnce(...args) {
          drives++;
          return f.driver.driveOnce(...args);
        },
        cancel(id) {
          aborts++;
          f.driver.cancel(id);
        },
      },
      completer: { async completeToolCall() {} },
    });
    const ref = await runtime.accept(f.request),
      leases = f.leases;
    enabled = false;
    expect(await runtime.runOnce(ref)).toBe("done");
    expect((await f.journal.get(ref.executionId))!.state).toBe("canceled");
    expect(drives).toBe(0);
    expect(aborts).toBe(1);
    expect(f.leases).toBe(leases);
    expect((await f.command("stats")).calls).toBe(0);
  } finally {
    await f.cleanup();
  }
}, 20000);
