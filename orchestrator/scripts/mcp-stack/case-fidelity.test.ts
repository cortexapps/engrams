import { expect, test } from "bun:test";

// Isolate substitutes from the other driver tests; exercise the real call,
// journal, proof selection, outbox ACK and native completion helpers.
const exercise = String.raw`
import { mock } from "bun:test";
import { strict as assert } from "node:assert";
const runtime = await import("./scenario-runtime.ts");
const bootstrap = await import("./bootstrap.ts");
const guest = await import("./guest.ts");
const observer = await import("./observer.ts");
const { sha256 } = await import("./context.ts");
const fault = process.env.FIDELITY_FAULT;
const ctx = { deadlines: { stepMs: 1 }, protocolFixtures: {
  modern: { protocol: "modern" }, legacy: { protocol: "legacy" }
} };
let boots = 0, cleanups = 0, reads = 0, configuredError = false;
const calls = [], acknowledgements = [], events = [], turns = [], completedTurns = [], proofs = [], setups = [];
let active;
const snapshot = () => ({ effects: { count: 7, executions: 0 }, reads });
mock.module("./observer.ts", () => ({ ...observer, FixtureObserver: class {
  async configure(input) {
    if (input.response === "error") {
      assert.equal(acknowledgements.at(-1), "fixture-read-ok");
      assert.equal(events.at(-1).tool_call_id, "fixture-read-ok");
      assert.equal(completedTurns.at(-1), "fixture-read-ok");
    }
    configuredError = input.response === "error";
  }
  async observe() { return snapshot(); }
} }));
mock.module("./bootstrap.ts", () => ({ ...bootstrap,
  bindingTools: async () => [{ modelToolName: active.alias }],
}));
mock.module("./guest.ts", () => ({ ...guest,
  proofPaths: async () => ["proof"],
  readGuestProofRecords: async () => proofs,
}));
mock.module("./scenario-runtime.ts", () => ({ ...runtime,
  async startScenario(ctx, principals, database, input) {
    boots++;
    const alias = "mcp_fixture_read";
    const plan = input.callPlan?.(alias, { id: "connection", alias: "fixture" });
    assert.equal(plan?.steps.length, 2, "two actual calls per boot");
    assert.notEqual(plan.steps[0].marker, plan.steps[1].marker);
    assert.notEqual(plan.steps[0].callId, plan.steps[1].callId);
    setups.push([input.harness, input.protocol, input.suffix]);
    proofs.length = 0; calls.length = 0; events.length = 0; turns.length = 0;
    active = { ctx, input, alias, connection: { id: "connection" },
      planSha256: "sha256:" + sha256(guest.canonicalJson(plan)),
      sessionId: "session-" + boots, client: {},
      catalog: { tools: [{ upstreamName: "read_counter", inputSchemaJson: "{}" }] },
      reviewClient: { integration: {
        async getSessionMcpState() { return { executions: calls, hasMoreExecutions: false }; },
        async getMcpExecutionResult({ executionId }) {
          const row = calls.find(call => call.executionId === executionId);
          return { state: row.state, resultUnavailable: false, resultJson: JSON.stringify({
            outcome: row.state, delivery: "available", output: {
              content: [{ type: "text", text: "count=7" }],
              structuredContent: { count: 7 }, isError: row.state === "failed",
            }
          }) };
        }
      }, sessions: { async listSessionEvents({ kinds }) {
        if (kinds[0] === "run_completed") {
          completedTurns.push(calls.at(-1).toolCallId);
          return { events: turns.map((event, idx) => ({ kind: "run_completed", idx: BigInt(idx * 2 + 1), payloadJson: JSON.stringify(event) })) };
        }
        return { events: events.map((event, idx) => ({ kind: "tool_call_completed", idx: BigInt(idx * 2), payloadJson: JSON.stringify(event) })) };
      } } }, plan };
    return active;
  },
  async sendScenarioPrompt(scenario, planMode, marker) {
    const step = active.plan.steps.find(step => step.marker === marker);
    assert(step, "dispatch selects a real plan marker");
    assert.equal(step.resultFormat, "mcp");
    assert.equal(step.callId, scenario.input.callId);
    assert.equal(step.tool, scenario.alias);
    assert.deepEqual(step.arguments, {});
    assert.deepEqual(step.expectedResult, guest.expectedCounterNative(scenario.input.harness, 7, configuredError));
    if (fault === "first-call" && !configuredError) throw new Error("first-call-failed");
    if (fault === "second-call" && configuredError) throw new Error("second-call-failed");
    reads++;
    calls.push({ toolCallId: step.callId, connectionId: "connection", executionId: "execution-" + reads, state: configuredError ? "failed" : "succeeded" });
    const hash = "sha256:" + sha256(guest.canonicalJson(step.expectedResult));
    const proof = { callId: step.callId, tool: step.tool, planSha256: active.planSha256,
      resultReceived: true, selectedToolName: step.tool,
      selectedSchemaSha256: "sha256:" + sha256("{}"),
      exposedMcpToolNamesSha256: "sha256:" + sha256(guest.canonicalJson([step.tool])),
      delivery: "available", resultMatchesExpected: true, resultSha256: hash, expectedResultSha256: hash };
    if (!(configuredError && fault === "old-proof"))
      proofs.push({ proof: configuredError && fault === "wrong-plan" ? { ...proof, planSha256: "sha256:" + "0".repeat(64) } : proof, sha256: sha256(guest.canonicalJson(proof)) });
    if (!(configuredError && fault === "old-completion"))
      events.push({ tool_call_id: step.callId, tool_name: scenario.input.harness === "claude" ? "mcp__engrams__" + step.tool : step.tool, run_id: step.callId, ok: !configuredError });
    if (!(configuredError && fault === "old-turn"))
      turns.push({ run_id: step.callId, ok: true });
  },
  async closeScenario() { cleanups++; }
}));
const database = {
  async outbox(sessionId, callId) {
    acknowledgements.push(callId);
    return { acknowledged: !(configuredError && fault === "old-ack"), relayed: true, attempts: 1 };
  },
  async deliveryTarget() { return { targetGone: false }; }
};
const { catalogFidelity } = await import("./case-fidelity.ts");
for (const harness of ["claude", "codex"]) {
  try {
    const rows = await catalogFidelity(ctx, {}, database, harness);
    assert.equal(fault, "none", "fault must fail the driver");
    assert.equal(rows.length, 8);
    assert.deepEqual(rows.map(row => [row.protocol, row.reply, row.isError]),
      ["modern", "legacy"].flatMap(protocol => ["json", "sse"].flatMap(reply => [false, true].map(isError => [protocol, reply, isError]))));
    assert(rows.every(row => row.proofHashes.length === 1 && row.acknowledged));
  } catch (error) {
    if (fault === "none") throw error;
    assert(error instanceof runtime.CaseBodyFailure);
    const seam = { "first-call": "first-call-failed", "second-call": "second-call-failed", "old-proof": "native_completed_proof_fixture-read-error", "wrong-plan": "native_completed_proof_fixture-read-error", "old-completion": "actual_native_completion_event", "old-ack": "actual_guest_ack", "old-turn": "native_turn_completed" }[fault];
    assert(error.failure.message.includes(seam), String(error.failure));
    assert.equal(cleanups, 1);
    console.log("fault verified");
    process.exit(0);
  }
}
assert.equal(boots, 8);
assert.equal(cleanups, 8);
assert.equal(reads, 16);
assert.equal(new Set(setups.map(([h, p, s]) => h + p + s)).size, 8);
assert.deepEqual(acknowledgements, Array.from({ length: 8 }, () => ["fixture-read-ok", "fixture-read-error"]).flat());
console.log("eight boots and sixteen real planned calls verified");
`;

for (const fault of [
  "none",
  "first-call",
  "second-call",
  "old-proof",
  "wrong-plan",
  "old-completion",
  "old-ack",
  "old-turn",
]) {
  test(`fidelity pairs retain call identity, delivery and cleanup: ${fault}`, async () => {
    const child = Bun.spawn([process.execPath, "--eval", exercise], {
      cwd: import.meta.dir,
      env: { ...process.env, FIDELITY_FAULT: fault },
      stdout: "pipe",
      stderr: "pipe",
    });
    const [stdout, stderr, exit] = await Promise.all([
      new Response(child.stdout).text(),
      new Response(child.stderr).text(),
      child.exited,
    ]);
    expect({ exit, stderr }).toEqual({ exit: 0, stderr: "" });
    expect(stdout).toContain(
      fault === "none" ? "sixteen real" : "fault verified",
    );
  });
}
