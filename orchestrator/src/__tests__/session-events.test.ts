/**
 * Reverse-channel reader + curation (ADR 0060 P1.2).
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

  // The closing summary (ADR 0060, onComplete) is enriched with the session's
  // last assistant message. agent_message is NOT a curated content kind (it's
  // noise for the thread), but the pump already walks every page to the
  // terminal — so the reader surfaces the last assistant text per page and the
  // pump tracks the most-recent across pages, riding it on session_terminal.
  describe("lastAssistantText", () => {
    const am = (idx: bigint, role: string, text: string): WireEvent => ({
      idx,
      kind: "agent_message",
      payloadJson: JSON.stringify({ run_id: "r", message_id: `m${idx}`, role, text }),
    });

    test("surfaces the text of the LAST assistant agent_message in the page", async () => {
      const page: WireEvent[] = [
        am(0n, "assistant", "first"),
        { idx: 1n, kind: "run_started", payloadJson: "{}" },
        am(2n, "assistant", "final answer"),
      ];
      const out = await readSessionEventsBounded("s", -1n, fakeList(page, 2n));
      expect(out.lastAssistantText).toBe("final answer");
    });

    test("ignores user/system roles", async () => {
      const page: WireEvent[] = [
        am(0n, "assistant", "real"),
        am(1n, "user", "the prompt echo"),
        am(2n, "system", "a system note"),
      ];
      const out = await readSessionEventsBounded("s", -1n, fakeList(page, 2n));
      expect(out.lastAssistantText).toBe("real");
    });

    test("undefined when the page has no assistant message", async () => {
      const out = await readSessionEventsBounded(
        "s",
        -1n,
        fakeList([{ idx: 0n, kind: "run_started", payloadJson: "{}" }], 0n),
      );
      expect(out.lastAssistantText).toBeUndefined();
    });

    test("a malformed agent_message payload is skipped (never throws)", async () => {
      const page: WireEvent[] = [
        am(0n, "assistant", "good"),
        { idx: 1n, kind: "agent_message", payloadJson: "not json" },
      ];
      const out = await readSessionEventsBounded("s", -1n, fakeList(page, 1n));
      expect(out.lastAssistantText).toBe("good");
    });
  });

  // Assistant text is now forwarded to the thread as content (coalesced into a
  // per-turn message downstream). agent_message stays OUT of CURATED_KINDS — it
  // is forwarded only for the assistant role, via the reader's special branch.
  describe("assistant message forwarding", () => {
    const am = (idx: bigint, role: string, text: string): WireEvent => ({
      idx,
      kind: "agent_message",
      payloadJson: JSON.stringify({ role, text }),
    });

    test("forwards assistant agent_message events as curated content (idx + payload)", async () => {
      const page: WireEvent[] = [
        { idx: 0n, kind: "run_started", payloadJson: "{}" },
        am(1n, "assistant", "working on it"),
        am(2n, "user", "the prompt echo"),
        am(3n, "system", "a note"),
        am(4n, "assistant", "done"),
      ];
      const out = await readSessionEventsBounded("s", -1n, fakeList(page, 4n));
      expect(out.events.map((e) => e.kind)).toEqual([
        "run_started",
        "agent_message",
        "agent_message",
      ]);
      expect(out.events.filter((e) => e.kind === "agent_message").map((e) => e.idx)).toEqual([1n, 4n]);
    });

    test("a malformed assistant agent_message is neither forwarded nor crashes", async () => {
      const page: WireEvent[] = [{ idx: 0n, kind: "agent_message", payloadJson: "not json" }];
      const out = await readSessionEventsBounded("s", -1n, fakeList(page, 0n));
      expect(out.events).toEqual([]);
    });
  });
});
