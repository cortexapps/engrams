// The windowed transcript read: spine + tail window, snapped to a run
// boundary, merged into ONE idx-ordered array. These drive the real paging
// logic against a fixture log through the same `ListEventsPage` seam the hook
// binds to the Connect client, so the round-trip (request shape → server
// filter → decode → merge) is what is under test, not a mock of it.

import { describe, expect, test } from "vitest";
import {
  decodePage,
  loadTranscript,
  loadWindow,
  mergeIndexed,
  runStartIdxs,
  snapTo,
  SPINE_KINDS,
  SPINE_TOOL_NAMES,
  WINDOW,
  type ListEventsPage,
  type PageRequest,
} from "./sessionWindow";
import type { IndexedEvent } from "./types";

const AT = "2026-08-10T12:00:00.000Z";

interface Wire {
  idx?: bigint;
  kind: string;
  payloadJson: string;
}

function wire(idx: number, kind: string, payload: Record<string, unknown> = {}): Wire {
  return { idx: BigInt(idx), kind, payloadJson: JSON.stringify({ at: AT, ...payload }) };
}

/** The tool name the server filters on: `name` on a request, `tool_name`
 *  elsewhere. Non-tool kinds are unaffected by `tool_names`. */
function toolNameOf(w: Wire): string | null {
  if (w.kind === "tool_call_requested") {
    return (JSON.parse(w.payloadJson) as { name?: string }).name ?? null;
  }
  if (w.kind === "tool_call_started" || w.kind === "tool_call_completed") {
    return (JSON.parse(w.payloadJson) as { tool_name?: string }).tool_name ?? null;
  }
  return null;
}

/** A stand-in for the coordinator's ListSessionEvents over a fixture log. */
function fixtureLister(log: Wire[]) {
  const calls: PageRequest[] = [];
  const list: ListEventsPage = (req) => {
    calls.push(req);
    if (req.afterIdx !== undefined && req.beforeIdx !== undefined) {
      throw new Error("after_idx and before_idx together is InvalidArgument");
    }
    let rows = log;
    if (req.kinds && req.kinds.length > 0) rows = rows.filter((r) => req.kinds!.includes(r.kind));
    if (req.toolNames && req.toolNames.length > 0) {
      rows = rows.filter((r) => {
        const name = toolNameOf(r);
        return name === null || req.toolNames!.includes(name);
      });
    }
    if (req.afterIdx !== undefined) rows = rows.filter((r) => Number(r.idx) > req.afterIdx!);
    if (req.beforeIdx !== undefined) rows = rows.filter((r) => r.idx! < req.beforeIdx!);
    // A backward page is read newest-first and re-ascended.
    const page = req.beforeIdx !== undefined ? rows.slice(-req.limit) : rows.slice(0, req.limit);
    const last = page[page.length - 1];
    return Promise.resolve({
      events: decodePage(page),
      count: page.length,
      next: last ? Number(last.idx) : (req.afterIdx ?? -1),
    });
  };
  return { list, calls };
}

/** A fixture session: runs of prose + Read tool pairs, like a real transcript
 *  (the tool kinds are the bytes the window exists to defer). With `echoes`,
 *  each turn also opens with the reader's own prompt — a `role:"user"`
 *  agent_message, which is how the real coordinator records it. */
function fixtureLog(
  toolsPerRun: number[],
  echoes = false,
): { log: Wire[]; runStarts: number[]; echoIdxs: number[] } {
  const log: Wire[] = [];
  const runStarts: number[] = [];
  const echoIdxs: number[] = [];
  toolsPerRun.forEach((tools, r) => {
    const runId = `r${r}`;
    if (echoes) {
      echoIdxs.push(log.length);
      log.push(
        wire(log.length, "agent_message", {
          run_id: "",
          message_id: `u${r}`,
          role: "user",
          text: `ask ${r}`,
          prompt_id: `p${r}`,
        }),
      );
    }
    runStarts.push(log.length);
    log.push(
      wire(log.length, "run_started", {
        run_id: runId,
        // Every harness sets this to null on purpose (it would double-render
        // the prompt beside the echo), so a fixture that pretends otherwise
        // would test a wire that does not exist.
        prompt_summary: null,
        prompt_id: echoes ? `p${r}` : null,
      }),
    );
    log.push(
      wire(log.length, "agent_message", {
        run_id: runId,
        message_id: `m${r}`,
        role: "assistant",
        text: `answer ${r}`,
      }),
    );
    for (let i = 0; i < tools; i++) {
      const toolCallId = `${runId}-t${i}`;
      log.push(
        wire(log.length, "tool_call_started", {
          run_id: runId,
          tool_call_id: toolCallId,
          tool_name: "Read",
          args_summary: null,
        }),
      );
      log.push(
        wire(log.length, "tool_call_completed", {
          run_id: runId,
          tool_call_id: toolCallId,
          tool_name: "Read",
          ok: true,
          duration_ms: 1,
          result_summary: "ok",
        }),
      );
    }
    log.push(wire(log.length, "run_completed", { run_id: runId, ok: true }));
  });
  return { log, runStarts, echoIdxs };
}

const kindsOf = (events: IndexedEvent[]) => new Set(events.map((e) => e.event.type));

describe("decodePage", () => {
  test("decodes through the SSE parse path and lifts the rewind metadata", () => {
    const events = decodePage([
      {
        idx: 7n,
        kind: "agent_message",
        payloadJson: JSON.stringify({
          at: AT,
          run_id: "r1",
          message_id: "m1",
          role: "assistant",
          text: "hi",
          _rewound: true,
          _recovery_epoch: 2,
        }),
      },
    ]);
    expect(events).toHaveLength(1);
    expect(events[0]!.idx).toBe(7);
    expect(events[0]!.rewound).toBe(true);
    expect(events[0]!.recoveryEpoch).toBe(2);
    // The metadata is lifted OFF the payload, exactly as the live feed does.
    expect(events[0]!.event).not.toHaveProperty("_rewound");
  });

  test("drops a frame with no idx and a kind the live feed never dispatches", () => {
    const events = decodePage([
      { kind: "agent_message", payloadJson: "{}" },
      { idx: 1n, kind: "title_suggested", payloadJson: JSON.stringify({ title: "x" }) },
      { idx: 2n, kind: "harness_idle", payloadJson: JSON.stringify({ at: AT }) },
    ]);
    expect(events.map((e) => e.idx)).toEqual([2]);
  });
});

describe("mergeIndexed", () => {
  const ev = (idx: number): IndexedEvent => ({
    idx,
    event: { type: "harness_idle", at: AT },
  });

  test("merges two ascending runs, de-duped on idx", () => {
    const merged = mergeIndexed([ev(0), ev(2), ev(4)], [ev(2), ev(3), ev(5)]);
    expect(merged.map((e) => e.idx)).toEqual([0, 2, 3, 4, 5]);
  });

  test("either side may be empty", () => {
    expect(mergeIndexed([], [ev(1)]).map((e) => e.idx)).toEqual([1]);
    expect(mergeIndexed([ev(1)], []).map((e) => e.idx)).toEqual([1]);
  });
});

describe("snapTo", () => {
  test("returns the run boundary at or before the idx", () => {
    expect(snapTo([0, 89, 212], 118)).toBe(89);
    expect(snapTo([0, 89, 212], 89)).toBe(89);
    expect(snapTo([0, 89, 212], 300)).toBe(212);
  });

  test("returns null when the log starts inside a turn", () => {
    expect(snapTo([89, 212], 12)).toBeNull();
  });
});

describe("loadTranscript — the session-open sequence", () => {
  const { log, runStarts } = fixtureLog([43, 60, 20, 30]);

  test("the spine reads the cheap kinds from idx 0 and names the structural tools", async () => {
    const { list, calls } = fixtureLister(log);
    await loadTranscript(list);
    const spineCall = calls.find((c) => c.kinds && c.kinds.length > 0);
    expect(spineCall).toBeDefined();
    expect(spineCall!.afterIdx).toBeUndefined();
    expect(spineCall!.kinds).toEqual(SPINE_KINDS);
    expect(spineCall!.toolNames).toEqual(SPINE_TOOL_NAMES);
    // Both spellings, because Claude exposes a registered tool as mcp__engrams__<name>.
    expect(spineCall!.toolNames).toContain("exit_plan_mode");
    expect(spineCall!.toolNames).toContain("mcp__engrams__exit_plan_mode");
    // The heavy kinds are NOT in the spine.
    expect(spineCall!.kinds).not.toContain("tool_call_completed");
    expect(spineCall!.kinds).not.toContain("tool_result_submitted");
    // Neither is conversation PROSE: an `agent_message` costs few bytes but
    // renders a full-height bubble, so a spine that held it made the page as
    // tall as an unwindowed one. Prose belongs to the window.
    expect(spineCall!.kinds).not.toContain("agent_message");
    // The run SKELETON stays — it is tiny, and the window edges snap to it.
    expect(spineCall!.kinds).toEqual(
      expect.arrayContaining([
        "run_started",
        "run_completed",
        "run_interrupted",
        "prompt_queued",
        "prompt_edited",
        "prompt_dequeued",
        "prompt_steered",
      ]),
    );
  });

  test("the tail window is the LAST page (before_idx = i64::MAX)", async () => {
    const { list, calls } = fixtureLister(log);
    await loadTranscript(list);
    const tailCall = calls.find((c) => c.beforeIdx !== undefined);
    expect(tailCall).toBeDefined();
    expect(tailCall!.beforeIdx).toBe(9223372036854775807n);
    expect(tailCall!.limit).toBe(WINDOW);
  });

  test("the window edge snaps DOWN to the run_started before it", async () => {
    const { list, calls } = fixtureLister(log);
    const loaded = await loadTranscript(list);
    const total = log.length;
    // The raw tail page starts here; the snap pulls the floor back to the
    // run boundary at or before it, so the turn is never cut in half.
    const rawOldest = total - WINDOW;
    const boundary = snapTo(runStarts, rawOldest)!;
    expect(boundary).toBeLessThan(rawOldest);
    expect(loaded.floor).toBe(boundary);
    // The fill reads FORWARD over exactly the missing span, not another page.
    const fillCall = calls.find((c) => c.afterIdx !== undefined && !c.kinds?.length);
    expect(fillCall).toEqual({ afterIdx: boundary - 1, limit: rawOldest - boundary });
  });

  test("spine and window merge into one ascending, idx-deduped array", async () => {
    const { list } = fixtureLister(log);
    const loaded = await loadTranscript(list);
    const idxs = loaded.events.map((e) => e.idx);
    expect(idxs).toEqual([...idxs].sort((a, b) => a - b));
    expect(new Set(idxs).size).toBe(idxs.length);
    // Below the floor: spine kinds only. At and above it: full fidelity.
    const below = loaded.events.filter((e) => e.idx < loaded.floor!);
    const above = loaded.events.filter((e) => e.idx >= loaded.floor!);
    expect(kindsOf(below)).toEqual(new Set(["run_started", "run_completed"]));
    expect(kindsOf(above)).toContain("tool_call_completed");
    expect(kindsOf(above)).toContain("agent_message");
    // The run boundaries are still all there — the window edges snap to them.
    expect(below.filter((e) => e.event.type === "run_started").length).toBeGreaterThan(0);
  });

  test("conversation prose is WINDOWED — an older run keeps only its boundary", async () => {
    // The point of the whole exercise: an open session must be a few screens
    // tall, not the whole log. Prose is the HEIGHT, so a run below the floor
    // arrives as its `run_started` (and its receipt) and nothing else — the
    // reader's own prompt included, since the wire filters on kind, not role.
    const { log: withEchoes } = fixtureLog([43, 60, 20, 30], true);
    const { list } = fixtureLister(withEchoes);
    const loaded = await loadTranscript(list);
    const prose = loaded.events.filter((e) => e.event.type === "agent_message");
    const proseInLog = withEchoes.filter((w) => w.kind === "agent_message");
    expect(prose.length).toBeLessThan(proseInLog.length);
    expect(prose.every((e) => e.idx >= loaded.floor!)).toBe(true);
  });

  test("`since` is the highest idx held, so the live tail continues from it", async () => {
    const { list } = fixtureLister(log);
    const loaded = await loadTranscript(list);
    expect(loaded.since).toBe(log.length - 1);
    expect(loaded.logStart).toBe(0);
    expect(loaded.hasMore).toBe(true);
    expect(runStartIdxs(loaded.events)).toEqual(runStarts);
  });

  test("a session shorter than one window loads whole, with nothing to backfill", async () => {
    const short = fixtureLog([5]);
    const { list } = fixtureLister(short.log);
    const loaded = await loadTranscript(list);
    expect(loaded.events).toHaveLength(short.log.length);
    expect(loaded.floor).toBe(0);
    expect(loaded.hasMore).toBe(false);
  });
});

describe("loadWindow — the backfill step", () => {
  test("prepends the next window below the floor and stops at the log start", async () => {
    const { log, runStarts } = fixtureLog([43, 60, 20, 30]);
    const { list } = fixtureLister(log);
    const loaded = await loadTranscript(list);

    const older = await loadWindow(list, BigInt(loaded.floor!), loaded.runStarts);
    expect(older.events.every((e) => e.idx < loaded.floor!)).toBe(true);
    expect(older.floor).toBeLessThan(loaded.floor!);
    // The fixture's first two runs are under one window, so this page reaches
    // the start of the log: the reader has everything.
    expect(older.exhausted).toBe(true);
    expect(older.floor).toBe(0);

    const merged = mergeIndexed(older.events, loaded.events);
    expect(new Set(merged.map((e) => e.idx)).size).toBe(merged.length);
    expect(merged[0]!.idx).toBe(0);
    expect(runStarts.every((idx) => merged.some((e) => e.idx === idx))).toBe(true);
  });

  test("a backfill page that is still not the start keeps hasMore alive", async () => {
    // Runs of ~120 events each: one backfill page cannot reach idx 0.
    const { log } = fixtureLog([58, 58, 58, 58, 58]);
    const { list } = fixtureLister(log);
    const loaded = await loadTranscript(list);
    const older = await loadWindow(list, BigInt(loaded.floor!), loaded.runStarts);
    expect(older.exhausted).toBe(false);
    expect(older.floor).toBeGreaterThan(0);
  });
});
