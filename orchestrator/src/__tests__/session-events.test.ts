/**
 * Reverse-channel reader + curation (ADR 0059 P1.2).
 *
 * The SessionIngestWorkflow pump walks the coordinator's append-only log
 * forward via `readSessionEventsBounded`, forwarding only the events an
 * external surface cares about and detecting the terminal status_changed that
 * ends the loop. These tests pin the curation set, field mapping, and terminal
 * detection — pure logic, with an injected fake list fn (no live coordinator).
 */

import { expect, test, describe } from "bun:test";
import {
  curated,
  readSessionEventsBounded,
  type WireEvent,
} from "../control-plane/session-events.ts";

/** A fake ListEventsFn that ignores its args and returns a fixed page. */
const fakeList = (events: WireEvent[], nextAfterIdx: bigint) => async () => ({
  events,
  nextAfterIdx,
});

describe("curated()", () => {
  test("accepts the six forwarded content kinds", () => {
    for (const k of [
      "run_started",
      "run_completed",
      "user_question",
      "question_answered",
      "integration_asset",
      "file_shared",
    ]) {
      expect(curated(k)).toBe(true);
    }
  });

  test("rejects control + noise kinds", () => {
    for (const k of [
      "status_changed", // control signal, handled separately
      "stdout",
      "stderr",
      "agent_message",
      "agent_message_chunk",
      "tool_call_started",
      "harness_idle",
    ]) {
      expect(curated(k)).toBe(false);
    }
  });
});

describe("readSessionEventsBounded()", () => {
  test("forwards only curated content, maps fields, returns the cursor", async () => {
    const page: WireEvent[] = [
      { idx: 0n, kind: "run_started", payloadJson: "{}" },
      { idx: 1n, kind: "agent_message", payloadJson: "{}" }, // noise
      { idx: 2n, kind: "user_question", payloadJson: '{"tool_call_id":"t1"}' },
      { idx: 3n, kind: "stdout", payloadJson: "{}" }, // noise
      { idx: 4n, kind: "file_shared", payloadJson: "{}" },
    ];
    const out = await readSessionEventsBounded("sess", -1n, fakeList(page, 4n));
    expect(out.events.map((e) => e.kind)).toEqual([
      "run_started",
      "user_question",
      "file_shared",
    ]);
    expect(out.events.map((e) => e.idx)).toEqual([0n, 2n, 4n]);
    expect(out.nextAfter).toBe(4n);
    expect(out.terminal).toBeUndefined();
  });

  test("terminal status_changed (Completed → ok:true) is detected, not forwarded as content", async () => {
    const page: WireEvent[] = [
      { idx: 5n, kind: "run_completed", payloadJson: "{}" },
      {
        idx: 6n,
        kind: "status_changed",
        payloadJson: '{"from":"active","to":"completed"}',
      },
    ];
    const out = await readSessionEventsBounded("sess", 4n, fakeList(page, 6n));
    expect(out.terminal).toEqual({ ok: true });
    expect(out.events.map((e) => e.kind)).toEqual(["run_completed"]);
    expect(out.nextAfter).toBe(6n);
  });

  test("Failed / Dead terminal → ok:false", async () => {
    for (const to of ["failed", "dead"]) {
      const out = await readSessionEventsBounded(
        "s",
        0n,
        fakeList([{ idx: 1n, kind: "status_changed", payloadJson: JSON.stringify({ to }) }], 1n),
      );
      expect(out.terminal).toEqual({ ok: false });
    }
  });

  test("non-terminal status_changed (idle) is neither content nor terminal", async () => {
    const out = await readSessionEventsBounded(
      "s",
      0n,
      fakeList([{ idx: 1n, kind: "status_changed", payloadJson: '{"to":"idle"}' }], 1n),
    );
    expect(out.terminal).toBeUndefined();
    expect(out.events).toEqual([]);
  });

  test("run_completed is NOT terminal (Invariant 2: a session re-runs on follow-up)", async () => {
    const out = await readSessionEventsBounded(
      "s",
      0n,
      fakeList([{ idx: 1n, kind: "run_completed", payloadJson: "{}" }], 1n),
    );
    expect(out.terminal).toBeUndefined();
    expect(out.events.map((e) => e.kind)).toEqual(["run_completed"]);
  });
});
