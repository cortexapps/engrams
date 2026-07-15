/**
 * Contract test: orchestrator SSE envelope → IndexedEvent parse.
 *
 * Fixture lines were captured from the LIVE orchestrator wire on
 * 2026-06-12 via:
 *   curl -sN -b <session-cookie> -H "Accept: text/event-stream" \
 *     http://127.0.0.1:8787/api/v1/sessions/<id>/events?since=-1
 *
 * Each fixture is the raw SSE `data:` string (the JSON envelope) plus
 * the SSE `event:` kind string — the two arguments parseOrchestratorFrame
 * receives from its EventSource listener.
 *
 * Fixture provenance:
 *   LIVE   — captured directly from the running orchestrator SSE feed.
 *   SYNTH  — constructed manually to exercise a code path not reachable
 *             from the idle test session (lagged, rewound).
 */

import { describe, expect, test } from "vitest";
import { parseOrchestratorFrame, type IndexedEvent, type SessionEvent } from "./events";
import { SESSION_EVENT_KINDS } from "./sse";

// ---------------------------------------------------------------------------
// LIVE fixtures — captured from the orchestrator, session 0c0929d3
// ---------------------------------------------------------------------------

/** LIVE: status_changed pending→created (idx 0, first event in the log) */
const LIVE_STATUS_CHANGED_0 = {
  kind: "status_changed",
  data: '{"idx":0,"kind":"status_changed","payload_json":"{\\"_recovery_epoch\\":0,\\"_rewound\\":false,\\"at\\":\\"2026-06-12T05:36:25.464780Z\\",\\"from\\":\\"pending\\",\\"to\\":\\"created\\",\\"type\\":\\"status_changed\\"}"}',
};

/** LIVE: status_changed created→active (idx 1) */
const LIVE_STATUS_CHANGED_1 = {
  kind: "status_changed",
  data: '{"idx":1,"kind":"status_changed","payload_json":"{\\"_recovery_epoch\\":0,\\"_rewound\\":false,\\"at\\":\\"2026-06-12T05:36:25.472382Z\\",\\"from\\":\\"created\\",\\"to\\":\\"active\\",\\"type\\":\\"status_changed\\"}"}',
};

/** LIVE: evicted (idx 3) */
const LIVE_EVICTED = {
  kind: "evicted",
  data: '{"idx":3,"kind":"evicted","payload_json":"{\\"_recovery_epoch\\":0,\\"_rewound\\":false,\\"at\\":\\"2026-06-12T06:07:11.010337Z\\",\\"type\\":\\"evicted\\"}"}',
};

/** LIVE: snapshot_taken (idx 5) */
const LIVE_SNAPSHOT_TAKEN = {
  kind: "snapshot_taken",
  data: '{"idx":5,"kind":"snapshot_taken","payload_json":"{\\"_recovery_epoch\\":0,\\"_rewound\\":false,\\"at\\":\\"2026-06-12T06:07:11.233275Z\\",\\"size_bytes\\":405693509,\\"snapshot_id\\":\\"90f52ca1-3503-48be-a216-92cd594dd7f1\\",\\"type\\":\\"snapshot_taken\\"}"}',
};

// ---------------------------------------------------------------------------
// SYNTH fixtures — not reachable from the idle test session
// ---------------------------------------------------------------------------

/** SYNTH: lagged frame — idx null, no SSE id: line emitted by orchestrator */
const SYNTH_LAGGED = {
  kind: "lagged",
  data: '{"idx":null,"kind":"lagged","payload_json":"{\\"missed\\":3}"}',
};

/** SYNTH: ping keepalive — orchestrator sends every 15 s */
const SYNTH_PING = {
  kind: "ping",
  data: "",
};

/**
 * SYNTH: rewound event — a replayed event that was tombstoned by a
 * rung-1 recovery. Folded in by with_rewind_meta with _rewound=true and
 * a non-zero _recovery_epoch. In practice this comes from a
 * recovered_from_checkpoint session; the idle test session has no
 * recovery history.
 */
const SYNTH_REWOUND_STATUS = {
  kind: "status_changed",
  data: '{"idx":7,"kind":"status_changed","payload_json":"{\\"_recovery_epoch\\":1,\\"_rewound\\":true,\\"at\\":\\"2026-06-12T01:00:00.000Z\\",\\"from\\":\\"active\\",\\"to\\":\\"idle\\",\\"type\\":\\"status_changed\\"}"}',
};

/**
 * SYNTH: recovered_from_checkpoint — the ADR 0028 rung-1 boundary
 * marker; carries its own recovery_epoch in the payload body.
 */
const SYNTH_RECOVERED = {
  kind: "recovered_from_checkpoint",
  data: '{"idx":8,"kind":"recovered_from_checkpoint","payload_json":"{\\"_recovery_epoch\\":1,\\"_rewound\\":false,\\"at\\":\\"2026-06-12T01:01:00.000Z\\",\\"cause\\":\\"host_failure_recovery\\",\\"recovery_epoch\\":1,\\"rolled_back\\":2,\\"surviving_side_effects\\":[],\\"through_idx\\":6,\\"type\\":\\"recovered_from_checkpoint\\"}"}',
};

/**
 * SYNTH: integration_asset (ADR 0056) for a provider the UI has no
 * hand-crafted renderer for (datadog). Proves the generic semantic envelope
 * parses regardless of provider; rendering falls back to the generic card.
 */
const SYNTH_INTEGRATION_ASSET = {
  kind: "integration_asset",
  data: '{"idx":9,"kind":"integration_asset","payload_json":"{\\"_recovery_epoch\\":0,\\"_rewound\\":false,\\"asset_kind\\":\\"query_result\\",\\"at\\":\\"2026-06-21T00:00:00.000Z\\",\\"data\\":{\\"p99\\":\\"812ms\\"},\\"fetchable\\":null,\\"provider\\":\\"datadog\\",\\"surface\\":\\"action\\",\\"type\\":\\"integration_asset\\"}"}',
};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

function parse(f: { kind: string; data: string }): IndexedEvent | null {
  return parseOrchestratorFrame(f.data, f.kind);
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

describe("parseOrchestratorFrame — LIVE fixtures", () => {
  test("status_changed idx=0: parses idx, strips rewind meta, reconstructs payload", () => {
    const result = parse(LIVE_STATUS_CHANGED_0);
    expect(result).not.toBeNull();
    const r = result!;
    expect(r.idx).toBe(0);
    expect(r.rewound).toBe(false);
    expect(r.recoveryEpoch).toBe(0);
    const ev = r.event as Extract<SessionEvent, { type: "status_changed" }>;
    expect(ev.type).toBe("status_changed");
    expect(ev.from).toBe("pending");
    expect(ev.to).toBe("created");
    expect((ev as Record<string, unknown>)._rewound).toBeUndefined();
    expect((ev as Record<string, unknown>)._recovery_epoch).toBeUndefined();
  });

  test("status_changed idx=1: correct idx, active state", () => {
    const result = parse(LIVE_STATUS_CHANGED_1);
    expect(result).not.toBeNull();
    expect(result!.idx).toBe(1);
    const ev = result!.event as Extract<SessionEvent, { type: "status_changed" }>;
    expect(ev.from).toBe("created");
    expect(ev.to).toBe("active");
  });

  test("evicted idx=3: parsed correctly", () => {
    const result = parse(LIVE_EVICTED);
    expect(result).not.toBeNull();
    expect(result!.idx).toBe(3);
    expect(result!.event.type).toBe("evicted");
  });

  test("snapshot_taken idx=5: snapshot_id and size_bytes present", () => {
    const result = parse(LIVE_SNAPSHOT_TAKEN);
    expect(result).not.toBeNull();
    expect(result!.idx).toBe(5);
    const ev = result!.event as Extract<SessionEvent, { type: "snapshot_taken" }>;
    expect(ev.snapshot_id).toBe("90f52ca1-3503-48be-a216-92cd594dd7f1");
    expect(ev.size_bytes).toBe(405693509);
  });
});

describe("parseOrchestratorFrame — SYNTH fixtures", () => {
  test("generic tool request/result payload fields survive the two-level SSE parse", () => {
    const requestPayload = {
      type: "tool_call_requested",
      run_id: "r1",
      tool_call_id: "call-1",
      name: "ask_user_question",
      args_json: JSON.stringify({ questions: [] }),
      at: "2026-07-14T00:00:00Z",
    };
    const requested = parseOrchestratorFrame(
      JSON.stringify({
        idx: 10,
        kind: "tool_call_requested",
        payload_json: JSON.stringify(requestPayload),
      }),
      "tool_call_requested",
    );
    expect(requested?.event).toEqual(requestPayload);

    const resultPayload = {
      type: "tool_result_submitted",
      tool_call_id: "call-1",
      result_json: JSON.stringify({ "Ship?": ["Yes"] }),
      at: "2026-07-14T00:00:01Z",
    };
    const submitted = parseOrchestratorFrame(
      JSON.stringify({
        idx: 11,
        kind: "tool_result_submitted",
        payload_json: JSON.stringify(resultPayload),
      }),
      "tool_result_submitted",
    );
    expect(submitted?.event).toEqual(resultPayload);
  });

  test("the EventSource allowlist includes both generic lifecycle kinds", () => {
    expect(SESSION_EVENT_KINDS).toContain("tool_call_requested");
    expect(SESSION_EVENT_KINDS).toContain("tool_result_submitted");
  });

  test("lagged returns null (not an IndexedEvent)", () => {
    expect(parse(SYNTH_LAGGED)).toBeNull();
  });

  test("ping returns null (keepalive ignored)", () => {
    expect(parse(SYNTH_PING)).toBeNull();
  });

  test("rewound event: rewound=true, recoveryEpoch=1, rewind meta stripped from payload", () => {
    const result = parse(SYNTH_REWOUND_STATUS);
    expect(result).not.toBeNull();
    expect(result!.idx).toBe(7);
    expect(result!.rewound).toBe(true);
    expect(result!.recoveryEpoch).toBe(1);
    const ev = result!.event as Record<string, unknown>;
    expect(ev._rewound).toBeUndefined();
    expect(ev._recovery_epoch).toBeUndefined();
  });

  test("recovered_from_checkpoint: payload fields intact, rewind meta stripped", () => {
    const result = parse(SYNTH_RECOVERED);
    expect(result).not.toBeNull();
    expect(result!.idx).toBe(8);
    expect(result!.rewound).toBe(false);
    expect(result!.recoveryEpoch).toBe(1);
    const ev = result!.event as Extract<SessionEvent, { type: "recovered_from_checkpoint" }>;
    expect(ev.type).toBe("recovered_from_checkpoint");
    expect(ev.recovery_epoch).toBe(1);
    expect(ev.through_idx).toBe(6);
    expect(ev.rolled_back).toBe(2);
    expect(ev.cause).toBe("host_failure_recovery");
    expect((ev as Record<string, unknown>)._rewound).toBeUndefined();
    expect((ev as Record<string, unknown>)._recovery_epoch).toBeUndefined();
  });

  test("integration_asset: a generic provider's envelope + payload parse intact", () => {
    const result = parse(SYNTH_INTEGRATION_ASSET);
    expect(result).not.toBeNull();
    expect(result!.idx).toBe(9);
    const ev = result!.event as Extract<SessionEvent, { type: "integration_asset" }>;
    expect(ev.type).toBe("integration_asset");
    expect(ev.provider).toBe("datadog");
    expect(ev.asset_kind).toBe("query_result");
    expect(ev.surface).toBe("action");
    expect(ev.fetchable).toBeNull();
    expect((ev as Record<string, unknown>)._rewound).toBeUndefined();
  });

  test("unknown kind still parses (EventSource only fires listeners for registered kinds)", () => {
    // parseOrchestratorFrame does not filter by kind — unknown event types
    // never reach the parser because subscribeSession only registers
    // addEventListener for the known SessionEventKind list. Future kinds
    // added to the server but not yet to the UI are silently ignored at
    // the EventSource layer, not here.
    const result = parseOrchestratorFrame(
      '{"idx":99,"kind":"future_event","payload_json":"{\\"type\\":\\"future_event\\"}"}',
      "future_event",
    );
    expect(result).not.toBeNull();
    expect(result!.idx).toBe(99);
  });

  test("malformed envelope JSON returns null", () => {
    expect(parseOrchestratorFrame("not-json", "status_changed")).toBeNull();
  });

  test("malformed payload_json returns null", () => {
    expect(
      parseOrchestratorFrame(
        '{"idx":1,"kind":"status_changed","payload_json":"not-json"}',
        "status_changed",
      ),
    ).toBeNull();
  });
});
