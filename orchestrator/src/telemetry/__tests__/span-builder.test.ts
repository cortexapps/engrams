/**
 * Golden tests for the pure span-builder core: parsed session events →
 * OTLP/JSON spans. These pin the exact encoding (lowerCamelCase fields,
 * nano-second DECIMAL-STRING timestamps, HEX trace/span ids) and the
 * deterministic-id contract that makes at-least-once export an upsert.
 */

import { describe, expect, test } from "bun:test";

import { buildExportRequest, strAttr } from "../otlp.ts";
import {
  buildRunSpans,
  buildSessionSpan,
  parseReplayMeta,
  parseRunEvent,
  parseRunId,
  traceIdFor,
  type CompletedRun,
  type SessionIdentity,
} from "../span-builder.ts";

const IDENTITY: SessionIdentity = {
  taskId: "task-1",
  rootTaskId: "root-1",
  createdByUserId: "user-1",
  harness: "claude",
  modelRouter: null,
  profileId: "profile-1",
  taskCreatedAtMs: Date.parse("2026-08-15T10:00:00Z"),
};

const SESSION = "11111111-2222-3333-4444-555555555555";

function fixtureRun(): CompletedRun {
  return {
    runId: "run-1",
    epoch: 0,
    startAt: "2026-08-15T10:00:10Z",
    endAt: "2026-08-15T10:01:00Z",
    promptId: "prompt-1",
    promptSummary: "fix the flaky test",
    interrupted: false,
    ok: true,
    events: [
      {
        type: "generation",
        at: "2026-08-15T10:00:20Z",
        messageId: "msg_01AAA",
        model: "claude-sonnet-5",
        inputTokens: 12,
        outputTokens: 345,
        cacheReadTokens: 6789,
        cacheCreationTokens: 42,
      },
      {
        type: "tool_started",
        at: "2026-08-15T10:00:21Z",
        toolCallId: "toolu_1",
        toolName: "Bash",
        argsSummary: "cargo test",
      },
      {
        type: "tool_completed",
        at: "2026-08-15T10:00:30Z",
        toolCallId: "toolu_1",
        toolName: "Bash",
        ok: true,
        durationMs: 9000,
        resultSummary: "42 tests passed",
      },
      {
        type: "tool_completed",
        at: "2026-08-15T10:00:40Z",
        toolCallId: "toolu_2",
        toolName: "Read",
        ok: false,
        durationMs: 100,
      },
      {
        type: "message",
        at: "2026-08-15T10:00:55Z",
        messageId: "msg_01AAA",
        role: "assistant",
        text: "done!",
      },
      { type: "run_cost", at: "2026-08-15T10:01:00Z", costMicroUsd: 1_234_567 },
    ],
  };
}

describe("span-builder", () => {
  test("golden: full run → OTLP JSON (metadata-only)", () => {
    const spans = buildRunSpans({
      sessionId: SESSION,
      identity: IDENTITY,
      run: fixtureRun(),
      captureContent: false,
      fallbackMs: 0,
    });
    const body = buildExportRequest([strAttr("service.name", "engrams")], spans);
    // Every id below is deterministic — this literal IS the contract.
    expect(JSON.parse(JSON.stringify(body))).toEqual({
      resourceSpans: [
        {
          resource: { attributes: [{ key: "service.name", value: { stringValue: "engrams" } }] },
          scopeSpans: [
            {
              scope: { name: "engrams-session-telemetry" },
              spans: [
                {
                  traceId: traceIdFor(SESSION, IDENTITY),
                  spanId: spans[0]!.spanId,
                  parentSpanId: spans[3]!.spanId,
                  name: "chat claude-sonnet-5",
                  kind: 3,
                  startTimeUnixNano: "1786788010000000000",
                  endTimeUnixNano: "1786788020000000000",
                  attributes: [
                    { key: "gen_ai.operation.name", value: { stringValue: "chat" } },
                    { key: "gen_ai.usage.input_tokens", value: { intValue: "12" } },
                    { key: "gen_ai.usage.output_tokens", value: { intValue: "345" } },
                    { key: "gen_ai.usage.cache_read_input_tokens", value: { intValue: "6789" } },
                    {
                      key: "gen_ai.usage.cache_creation_input_tokens",
                      value: { intValue: "42" },
                    },
                    { key: "gen_ai.request.model", value: { stringValue: "claude-sonnet-5" } },
                    { key: "gen_ai.response.id", value: { stringValue: "msg_01AAA" } },
                  ],
                },
                {
                  traceId: traceIdFor(SESSION, IDENTITY),
                  spanId: spans[1]!.spanId,
                  parentSpanId: spans[3]!.spanId,
                  name: "execute_tool Bash",
                  kind: 1,
                  // end 10:00:30Z minus the harness-measured 9000ms
                  startTimeUnixNano: "1786788021000000000",
                  endTimeUnixNano: "1786788030000000000",
                  attributes: [
                    { key: "gen_ai.operation.name", value: { stringValue: "execute_tool" } },
                    { key: "gen_ai.tool.name", value: { stringValue: "Bash" } },
                    { key: "gen_ai.tool.call.id", value: { stringValue: "toolu_1" } },
                  ],
                  status: { code: 1 },
                },
                {
                  traceId: traceIdFor(SESSION, IDENTITY),
                  spanId: spans[2]!.spanId,
                  parentSpanId: spans[3]!.spanId,
                  name: "execute_tool Read",
                  kind: 1,
                  startTimeUnixNano: "1786788039900000000",
                  endTimeUnixNano: "1786788040000000000",
                  attributes: [
                    { key: "gen_ai.operation.name", value: { stringValue: "execute_tool" } },
                    { key: "gen_ai.tool.name", value: { stringValue: "Read" } },
                    { key: "gen_ai.tool.call.id", value: { stringValue: "toolu_2" } },
                  ],
                  status: { code: 2 }, // ok:false → ERROR
                },
                {
                  traceId: traceIdFor(SESSION, IDENTITY),
                  spanId: spans[3]!.spanId,
                  parentSpanId: spans[3]!.parentSpanId,
                  name: "agent_turn",
                  kind: 1,
                  startTimeUnixNano: "1786788010000000000",
                  endTimeUnixNano: "1786788060000000000",
                  attributes: [
                    { key: "gen_ai.operation.name", value: { stringValue: "invoke_agent" } },
                    { key: "session.id", value: { stringValue: SESSION } },
                    { key: "engrams.run_id", value: { stringValue: "run-1" } },
                    { key: "engrams.prompt_id", value: { stringValue: "prompt-1" } },
                    { key: "engrams.cost_usd", value: { doubleValue: 1.234567 } },
                  ],
                  events: [
                    {
                      timeUnixNano: "1786788055000000000",
                      name: "agent_message",
                      attributes: [
                        { key: "gen_ai.message.role", value: { stringValue: "assistant" } },
                      ],
                    },
                  ],
                  status: { code: 1 },
                },
              ],
            },
          ],
        },
      ],
    });
  });

  test("deterministic ids: identical inputs → identical spans; epoch bump → new span ids, same trace", () => {
    const build = (epoch: number) =>
      buildRunSpans({
        sessionId: SESSION,
        identity: IDENTITY,
        run: { ...fixtureRun(), epoch },
        captureContent: false,
        fallbackMs: 0,
      });
    const a = build(0);
    const b = build(0);
    expect(a).toEqual(b);
    const c = build(1);
    expect(c[0]!.traceId).toBe(a[0]!.traceId);
    expect(c.map((s) => s.spanId)).not.toContain(a[0]!.spanId);
    expect(c.map((s) => s.spanId)).not.toContain(a[3]!.spanId);
  });

  test("content flag gates prompt, message text, and tool summaries", () => {
    const withContent = buildRunSpans({
      sessionId: SESSION,
      identity: IDENTITY,
      run: fixtureRun(),
      captureContent: true,
      fallbackMs: 0,
    });
    const flat = JSON.stringify(withContent);
    expect(flat).toContain("fix the flaky test"); // gen_ai.prompt
    expect(flat).toContain("cargo test"); // args summary (joined from tool_started)
    expect(flat).toContain("42 tests passed"); // result summary
    expect(flat).toContain("done!"); // message text

    const metadataOnly = JSON.stringify(
      buildRunSpans({
        sessionId: SESSION,
        identity: IDENTITY,
        run: fixtureRun(),
        captureContent: false,
        fallbackMs: 0,
      }),
    );
    for (const secret of ["fix the flaky test", "cargo test", "42 tests passed", "done!"]) {
      expect(metadataOnly).not.toContain(secret);
    }
    // The token counts stay either way.
    expect(metadataOnly).toContain('"345"');
  });

  test("interrupted / not-ok runs get ERROR status and the interrupted marker", () => {
    const spans = buildRunSpans({
      sessionId: SESSION,
      identity: IDENTITY,
      run: { ...fixtureRun(), interrupted: true, ok: false },
      captureContent: false,
      fallbackMs: 0,
    });
    const turn = spans.at(-1)!;
    expect(turn.status).toEqual({ code: 2 });
    expect(JSON.stringify(turn.attributes)).toContain("engrams.interrupted");
  });

  test("session span: identity attributes, outcome status, task-created start", () => {
    const span = buildSessionSpan({
      sessionId: SESSION,
      identity: IDENTITY,
      outcome: "failed",
      endMs: Date.parse("2026-08-15T11:00:00Z"),
    });
    expect(span.name).toBe("agent_session");
    expect(span.traceId).toBe(traceIdFor(SESSION, IDENTITY));
    expect(span.startTimeUnixNano).toBe("1786788000000000000");
    expect(span.endTimeUnixNano).toBe("1786791600000000000");
    expect(span.status).toEqual({ code: 2 });
    const flat = JSON.stringify(span.attributes);
    expect(flat).toContain("task-1");
    expect(flat).toContain("user-1");
    expect(flat).toContain("claude");
  });

  test("trace id seed prefers rootTaskId, then taskId, then sessionId", () => {
    const root = traceIdFor(SESSION, IDENTITY);
    const noRoot = traceIdFor(SESSION, { ...IDENTITY, rootTaskId: null });
    const noTask = traceIdFor(SESSION, null);
    expect(new Set([root, noRoot, noTask]).size).toBe(3);
    // Stable across calls.
    expect(traceIdFor(SESSION, IDENTITY)).toBe(root);
    expect(root).toMatch(/^[0-9a-f]{32}$/);
  });

  test("payload parsers: run ids, replay meta, event shapes", () => {
    expect(parseRunId(`{"run_id":"run-9","at":"2026-08-15T10:00:00Z"}`)).toBe("run-9");
    expect(parseRunId(`{}`)).toBeUndefined();
    expect(parseRunId("not json")).toBeUndefined();

    expect(parseReplayMeta(`{"_rewound":true,"_recovery_epoch":3}`)).toEqual({
      rewound: true,
      epoch: 3,
    });
    expect(parseReplayMeta(`{}`)).toEqual({ rewound: false, epoch: 0 });

    expect(
      parseRunEvent(
        "generation",
        `{"run_id":"r","message_id":"m","model":"x","input_tokens":1,"output_tokens":2,"cache_read_tokens":3,"cache_creation_tokens":4,"at":"2026-08-15T10:00:00Z"}`,
      ),
    ).toEqual({
      type: "generation",
      at: "2026-08-15T10:00:00Z",
      messageId: "m",
      model: "x",
      inputTokens: 1,
      outputTokens: 2,
      cacheReadTokens: 3,
      cacheCreationTokens: 4,
    });
    expect(parseRunEvent("run_cost", `{"run_id":"r","cost_micro_usd":50}`)).toEqual({
      type: "run_cost",
      at: undefined,
      costMicroUsd: 50,
    });
    expect(parseRunEvent("stdout", `{"bytes_start":0}`)).toBeUndefined();
  });
});
