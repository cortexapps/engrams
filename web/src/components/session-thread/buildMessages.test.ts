// Tests for the assistant-ui adapter (buildMessages) — the successor to
// Transcript's buildBlocks. These carry the logic coverage: per-run
// grouping, parts ordering, shell-exec synthesis, the harness-register
// system markers, the run footer, and the derived isRunning flag.

import { describe, expect, test } from "vitest";
import {
  BROWSER_ACTIVITY_TOOL,
  buildMessages,
  FILE_CHANGE_TOOL,
  SHELL_TOOL,
  type FileChangeArgs,
  type RunFooter,
  type SystemMarker,
} from "./buildMessages";
import type { IndexedEvent, SessionEvent, UserQuestion } from "../../lib/types";

const AT = "2026-06-02T12:00:00.000Z";
const AT2 = "2026-06-02T12:00:18.000Z";
const SID = "s1";

function indexed(events: SessionEvent[]): IndexedEvent[] {
  return events.map((event, idx) => ({ idx, event }));
}

/** Strip the trailing synthetic running placeholder, when present, so a
 *  test can assert on the "real" messages regardless of busy state. */
function real(messages: ReturnType<typeof buildMessages>["messages"]) {
  return messages.filter((m) => m.id !== "pending");
}

function customMarker(m: { metadata?: { custom?: Record<string, unknown> } }) {
  return m.metadata?.custom?.marker as SystemMarker | undefined;
}

describe("buildMessages — message/part shaping", () => {
  test("a user-role agent_message becomes a user message with one text part", () => {
    const { messages } = buildMessages(
      indexed([
        {
          type: "agent_message",
          run_id: "",
          message_id: "u1",
          role: "user",
          text: "fix the flaky test",
          at: AT,
        },
      ]),
      SID,
    );
    const msgs = real(messages);
    expect(msgs).toHaveLength(1);
    expect(msgs[0]).toMatchObject({
      role: "user",
      content: [{ type: "text", text: "fix the flaky test" }],
    });
  });

  test("a run's assistant text + tool calls collapse into one assistant message, parts in order", () => {
    const { messages } = buildMessages(
      indexed([
        { type: "run_started", run_id: "r1", prompt_summary: null, at: AT },
        {
          type: "agent_message",
          run_id: "r1",
          message_id: "a1",
          role: "assistant",
          text: "looking",
          at: AT,
        },
        {
          type: "tool_call_started",
          run_id: "r1",
          tool_call_id: "t1",
          tool_name: "Read",
          args_summary: '{"file_path":"a.rs"}',
          at: AT,
        },
        {
          type: "tool_call_completed",
          run_id: "r1",
          tool_call_id: "t1",
          tool_name: "Read",
          ok: true,
          duration_ms: 11,
          result_summary: "212 lines",
          at: AT,
        },
        { type: "run_completed", run_id: "r1", ok: true, at: AT2 },
      ]),
      SID,
    );
    const msgs = real(messages);
    expect(msgs).toHaveLength(1);
    const a = msgs[0]!;
    expect(a.role).toBe("assistant");
    expect(a.content).toHaveLength(2);
    expect(a.content[0]).toMatchObject({ type: "text", text: "looking" });
    expect(a.content[1]).toMatchObject({
      type: "tool-call",
      toolName: "Read",
      args: { file_path: "a.rs" },
      result: "212 lines",
      isError: false,
    });
    expect(a.status).toEqual({ type: "complete", reason: "stop" });
  });

  test("consecutive assistant messages coalesce into one prose text part", () => {
    const { messages } = buildMessages(
      indexed([
        { type: "run_started", run_id: "r1", prompt_summary: null, at: AT },
        {
          type: "agent_message",
          run_id: "r1",
          message_id: "a1",
          role: "assistant",
          text: "one",
          at: AT,
        },
        {
          type: "agent_message",
          run_id: "r1",
          message_id: "a2",
          role: "assistant",
          text: "two",
          at: AT,
        },
        { type: "run_completed", run_id: "r1", ok: true, at: AT2 },
      ]),
      SID,
    );
    const a = real(messages)[0]!;
    expect(a.content).toEqual([{ type: "text", text: "one\n\ntwo" }]);
  });

  test("a shell exec becomes a synthetic engram.shell tool part, output streamed into result", () => {
    const { messages } = buildMessages(
      indexed([
        { type: "run_started", run_id: "r1", prompt_summary: null, at: AT },
        { type: "exec_started", exec_id: "x1", command: ["cargo", "check"], at: AT },
        { type: "stdout", exec_id: "x1", chunk: "Compiling…\n" },
        { type: "stderr", exec_id: "x1", chunk: "warning: unused\n" },
        {
          type: "exec_completed",
          exec_id: "x1",
          exit_status: 0,
          rusage: { wall_ms: 4200 },
          at: AT2,
        },
        { type: "run_completed", run_id: "r1", ok: true, at: AT2 },
      ]),
      SID,
    );
    const a = real(messages)[0]!;
    const content = a.content as ReadonlyArray<{ type: string }>;
    const part = content.find((p) => p.type === "tool-call") as Record<string, unknown>;
    expect(part).toMatchObject({
      toolName: SHELL_TOOL,
      argsText: "cargo check",
      isError: false,
    });
    expect(part.args).toMatchObject({ command: "cargo check", exit: 0, durationMs: 4200 });
    expect(part.result).toContain("Compiling");
    expect(part.result).toContain("warning: unused");
  });

  test("a nonzero exit marks the shell part as an error", () => {
    const { messages } = buildMessages(
      indexed([
        { type: "exec_started", exec_id: "x1", command: ["false"], at: AT },
        {
          type: "exec_completed",
          exec_id: "x1",
          exit_status: 1,
          rusage: { wall_ms: 3 },
          at: AT2,
        },
        { type: "harness_idle", at: AT2 },
      ]),
      SID,
    );
    const part = real(messages)[0]!.content[0] as Record<string, unknown>;
    expect(part).toMatchObject({ toolName: SHELL_TOOL, isError: true });
  });

  test("a completed run carries the read/edit/ran tally as a footer", () => {
    const { messages } = buildMessages(
      indexed([
        { type: "run_started", run_id: "r1", prompt_summary: null, at: AT },
        {
          type: "tool_call_started",
          run_id: "r1",
          tool_call_id: "t1",
          tool_name: "Read",
          args_summary: '{"file_path":"a.rs"}',
          at: AT,
        },
        {
          type: "tool_call_started",
          run_id: "r1",
          tool_call_id: "t2",
          tool_name: "Edit",
          args_summary: '{"file_path":"a.rs"}',
          at: AT,
        },
        { type: "exec_started", exec_id: "x1", command: ["cargo", "test"], at: AT },
        { type: "run_completed", run_id: "r1", ok: true, at: AT2 },
      ]),
      SID,
    );
    const footer = real(messages)[0]!.metadata?.custom?.run as RunFooter;
    expect(footer).toMatchObject({
      reads: 1,
      edits: 1,
      ran: 1,
      other: 0,
      ok: true,
      interrupted: false,
    });
  });

  test("run_interrupted closes the assistant message as incomplete/cancelled", () => {
    const { messages } = buildMessages(
      indexed([
        { type: "run_started", run_id: "r1", prompt_summary: null, at: AT },
        { type: "exec_started", exec_id: "x1", command: ["cargo", "test"], at: AT },
        { type: "run_interrupted", run_id: "r1", at: AT2 },
      ]),
      SID,
    );
    const a = real(messages)[0]!;
    expect(a.status).toEqual({ type: "incomplete", reason: "cancelled" });
    const run = a.metadata?.custom?.run as RunFooter | undefined;
    expect(run?.interrupted).toBe(true);
  });

  test("an open run on an inactive session (idle-evicted mid-run) is not running", () => {
    // run_started with no terminal run event — the wedge case. The
    // authoritative session.status closes it.
    const open: SessionEvent[] = [
      { type: "run_started", run_id: "r1", prompt_summary: null, at: AT },
      { type: "exec_started", exec_id: "x1", command: ["cargo", "test"], at: AT },
    ];
    // Without a status the event stream alone keeps it "running" (today's bug
    // surface) — and idle wins.
    expect(buildMessages(indexed(open), SID).isRunning).toBe(true);
    expect(buildMessages(indexed(open), SID, "active").isRunning).toBe(true);

    const evicted = buildMessages(indexed(open), SID, "idle");
    expect(evicted.isRunning).toBe(false);
    // The open assistant message's spinner is finalized as cut-short, and no
    // trailing "pending" running placeholder is appended.
    const a = real(evicted.messages)[0]!;
    expect(a.status).toEqual({ type: "incomplete", reason: "cancelled" });
    expect(evicted.messages.some((m) => m.id === "pending")).toBe(false);
  });

  test("snapshot / resumed become durability system markers", () => {
    const { messages } = buildMessages(
      indexed([
        { type: "snapshot_taken", snapshot_id: "s", size_bytes: 1_287_000_000, at: AT },
        { type: "resumed", snapshot_id: "s", at: AT2 },
      ]),
      SID,
    );
    const msgs = real(messages);
    expect(msgs[0]!.role).toBe("system");
    expect(customMarker(msgs[0]!)).toMatchObject({
      kind: "durability",
      mark: "snapshot",
      sizeBytes: 1_287_000_000,
    });
    expect(customMarker(msgs[1]!)).toMatchObject({ kind: "durability", mark: "resumed" });
    // System messages keep a single text-part fallback (shape constraint).
    expect(msgs[0]!.content).toHaveLength(1);
    expect((msgs[0]!.content[0] as { type: string }).type).toBe("text");
  });

  test("the 'waking up' resume marker is transient: repeats collapse and activity clears it", () => {
    const markOf = (m: { metadata?: { custom?: Record<string, unknown> } }) => {
      const mk = customMarker(m);
      return mk?.kind === "durability" ? mk.mark : undefined;
    };
    const waking = (evs: SessionEvent[]) =>
      real(buildMessages(indexed(evs), SID).messages).filter((m) => markOf(m) === "waking");

    // A retrying resume emits several ResumeStarted events; while still
    // waking (no activity after), exactly ONE marker survives.
    expect(
      waking([
        { type: "resume_started", at: AT },
        { type: "resume_started", at: AT2 },
      ]),
    ).toHaveLength(1);

    // run_started supersedes it — the session is producing output.
    expect(
      waking([
        { type: "resume_started", at: AT },
        { type: "resume_started", at: AT2 },
        { type: "run_started", run_id: "r1", prompt_summary: null, at: AT2 },
      ]),
    ).toHaveLength(0);

    // resumed supersedes it too (a resume that completes before any run).
    const afterResumed = buildMessages(
      indexed([
        { type: "resume_started", at: AT },
        { type: "resumed", snapshot_id: "s", at: AT2 },
      ]),
      SID,
    ).messages;
    expect(real(afterResumed).filter((m) => markOf(m) === "waking")).toHaveLength(0);
    // ...and the durable 'resumed' marker still renders.
    expect(real(afterResumed).filter((m) => markOf(m) === "resumed")).toHaveLength(1);
  });

  test("integration_asset(forge/pull_request) becomes an integration_asset marker", () => {
    const { messages } = buildMessages(
      indexed([
        {
          type: "integration_asset",
          provider: "forge",
          asset_kind: "pull_request",
          surface: "asset",
          data: {
            repo: "x/y",
            title: "Fix it",
            number: 7,
            head_branch: "fix",
            base_branch: "main",
          },
          fetchable: { kind: "external", url: "https://gh/x/pull/7" },
          at: AT,
        },
      ]),
      SID,
    );
    expect(customMarker(real(messages)[0]!)).toMatchObject({
      kind: "integration_asset",
      provider: "forge",
      assetKind: "pull_request",
      surface: "asset",
      data: { repo: "x/y", number: 7 },
      fetchable: { kind: "external", url: "https://gh/x/pull/7" },
    });
  });

  // ADR 0056: a provider the UI has no hand-crafted renderer for still
  // surfaces as a generic integration_asset marker (rendered by the fallback
  // card) with zero new web code — the "PRs aren't special" property.
  test("a never-seen integration_asset provider still becomes a generic marker", () => {
    const { messages } = buildMessages(
      indexed([
        {
          type: "integration_asset",
          provider: "datadog",
          asset_kind: "query_result",
          surface: "action",
          data: { query: "avg:system.cpu", p99: "812ms" },
          fetchable: null,
          at: AT,
        },
      ]),
      SID,
    );
    expect(customMarker(real(messages)[0]!)).toMatchObject({
      kind: "integration_asset",
      provider: "datadog",
      assetKind: "query_result",
      surface: "action",
      fetchable: null,
    });
  });

  test("file_shared becomes an artifact system marker carrying the session id", () => {
    const { messages } = buildMessages(
      indexed([
        {
          type: "file_shared",
          artifact_id: "art1",
          media_type: "image/png",
          size_bytes: 9000,
          caption: "a shot",
          at: AT,
        },
      ]),
      SID,
    );
    expect(customMarker(real(messages)[0]!)).toMatchObject({
      kind: "artifact",
      sessionId: SID,
      artifactId: "art1",
      mediaType: "image/png",
      caption: "a shot",
    });
  });

  // ADR 0090: the durability-rollback warning marker carries the manifest the
  // resume rewound to (flattened to `<id>@v<n>`) plus the reason.
  test("durability_rollback becomes a durability_rollback marker with the restored manifest", () => {
    const { messages } = buildMessages(
      indexed([
        {
          type: "durability_rollback",
          sandbox_id: "sb-9",
          rewind_disk_manifest: { manifest_id: "abc", version: 4 },
          reason: "quarantined-survivor evict budget exhausted; VM destroyed",
          at: AT,
        },
      ]),
      SID,
    );
    expect(customMarker(real(messages)[0]!)).toMatchObject({
      kind: "durability_rollback",
      manifest: "abc@v4",
      reason: "quarantined-survivor evict budget exhausted; VM destroyed",
    });
  });

  test("durability_rollback with no live publish carries a null manifest", () => {
    const { messages } = buildMessages(
      indexed([
        {
          type: "durability_rollback",
          sandbox_id: "sb-9",
          rewind_disk_manifest: null,
          reason: "budget exhausted",
          at: AT,
        },
      ]),
      SID,
    );
    expect(customMarker(real(messages)[0]!)).toMatchObject({
      kind: "durability_rollback",
      manifest: null,
    });
  });

  test("a system-role agent_message becomes a note system marker", () => {
    const { messages } = buildMessages(
      indexed([
        {
          type: "agent_message",
          run_id: "r1",
          message_id: "s1",
          role: "system",
          text: "context compacted",
          at: AT,
        },
      ]),
      SID,
    );
    const m = real(messages)[0]!;
    expect(m.role).toBe("system");
    expect(customMarker(m)).toMatchObject({ kind: "note" });
    expect((m.content[0] as { text: string }).text).toBe("context compacted");
  });
});

describe("buildMessages — isRunning", () => {
  test("an open tool call (no completion) is running", () => {
    const { isRunning } = buildMessages(
      indexed([
        { type: "run_started", run_id: "r1", prompt_summary: null, at: AT },
        {
          type: "tool_call_started",
          run_id: "r1",
          tool_call_id: "t1",
          tool_name: "Read",
          args_summary: null,
          at: AT,
        },
      ]),
      SID,
    );
    expect(isRunning).toBe(true);
  });

  test("a trailing user message (prompt sent, harness not started) is running", () => {
    const { isRunning } = buildMessages(
      indexed([
        { type: "agent_message", run_id: "", message_id: "u1", role: "user", text: "go", at: AT },
      ]),
      SID,
    );
    expect(isRunning).toBe(true);
  });

  test("a completed run ending in an assistant message is not running", () => {
    const { isRunning } = buildMessages(
      indexed([
        { type: "run_started", run_id: "r1", prompt_summary: null, at: AT },
        {
          type: "agent_message",
          run_id: "r1",
          message_id: "a1",
          role: "assistant",
          text: "done",
          at: AT,
        },
        { type: "run_completed", run_id: "r1", ok: true, at: AT2 },
      ]),
      SID,
    );
    expect(isRunning).toBe(false);
  });

  test("a trailing durability marker after a completed run stays not-running", () => {
    const { isRunning } = buildMessages(
      indexed([
        { type: "run_started", run_id: "r1", prompt_summary: null, at: AT },
        {
          type: "agent_message",
          run_id: "r1",
          message_id: "a1",
          role: "assistant",
          text: "done",
          at: AT,
        },
        { type: "run_completed", run_id: "r1", ok: true, at: AT2 },
        { type: "snapshot_taken", snapshot_id: "s", size_bytes: 1, at: AT2 },
      ]),
      SID,
    );
    expect(isRunning).toBe(false);
  });

  test("when running, a trailing placeholder assistant message is appended for the indicator", () => {
    const { messages } = buildMessages(
      indexed([
        { type: "agent_message", run_id: "", message_id: "u1", role: "user", text: "go", at: AT },
      ]),
      SID,
    );
    const last = messages[messages.length - 1]!;
    expect(last.id).toBe("pending");
    expect(last.role).toBe("assistant");
    expect(last.status).toEqual({ type: "running" });
  });
});

describe("buildMessages — Phase 1b queued/optimistic greying", () => {
  test("a user echo with a prompt_id is keyed by it and ungreys once its run_started consumes it", () => {
    const { messages } = buildMessages(
      indexed([
        {
          type: "agent_message",
          run_id: "",
          message_id: "u1",
          role: "user",
          text: "do it",
          prompt_id: "p1",
          at: AT,
        },
        { type: "run_started", run_id: "r1", prompt_summary: null, prompt_id: "p1", at: AT },
        { type: "run_completed", run_id: "r1", ok: true, at: AT2 },
      ]),
      SID,
      "idle",
    );
    const user = real(messages).find((m) => m.role === "user")!;
    // Keyed by prompt_id so the optimistic bubble dedupes against it in place.
    expect(user.id).toBe("p1");
    // Consumed by run_started{p1} → solid (no longer greyed).
    expect(user.metadata?.custom?.pending).toBeUndefined();
  });

  test("a still-queued user echo (no run_started yet) is held OUT of the transcript", () => {
    const { messages, queue } = buildMessages(
      indexed([
        // A run is in flight; a type-ahead prompt is echoed + queued, not consumed.
        { type: "run_started", run_id: "r1", prompt_summary: null, at: AT },
        {
          type: "agent_message",
          run_id: "",
          message_id: "u2",
          role: "user",
          text: "and then deploy",
          prompt_id: "p2",
          at: AT2,
        },
        { type: "prompt_queued", prompt_id: "p2", summary: "and then deploy", at: AT2 },
      ]),
      SID,
      "idle",
    );
    // Held: NOT in the thread — it lives in the composer rail until its run
    // starts (then it joins the conversation at the consumption point).
    expect(messages.some((m) => m.id === "p2")).toBe(false);
    // Surfaced via `queue` for the rail.
    expect(queue.map((q) => q.promptId)).toContain("p2");
  });

  // prod session 68c70a65: the coordinator's SendPrompt path can append the
  // harness's run_started BEFORE the user echo — it forwards the prompt (which
  // starts the run) and only then emits the echo, so for an idle follow-up the
  // two invert. The held echo is then never consumed (its run_started already
  // passed) and the user turn used to vanish. The prompt_id pre-scan keeps it.
  test("a prompt_id user echo logged AFTER its run_started still renders (forward-before-echo inversion, 68c70a65)", () => {
    const { messages } = buildMessages(
      indexed([
        // INVERTED: run_started lands first (lower idx)…
        { type: "run_started", run_id: "r1", prompt_summary: null, prompt_id: "p1", at: AT },
        // …then the user echo it consumes.
        {
          type: "agent_message",
          run_id: "",
          message_id: "u1",
          role: "user",
          text: "are you there?",
          prompt_id: "p1",
          at: AT,
        },
        {
          type: "agent_message",
          run_id: "r1",
          message_id: "a1",
          role: "assistant",
          text: "yes!",
          at: AT2,
        },
        { type: "run_completed", run_id: "r1", ok: true, at: AT2 },
      ]),
      SID,
      "idle",
    );
    // The follow-up turn must NOT vanish: it renders, keyed by its prompt_id.
    const user = real(messages).find((m) => m.role === "user");
    expect(user).toMatchObject({
      role: "user",
      id: "p1",
      content: [{ type: "text", text: "are you there?" }],
    });
  });
});

describe("buildMessages — ADR 0052 queue (type-ahead recall/cancel)", () => {
  test("prompt_queued enters the queue; run_started{prompt_id} consumes it", () => {
    expect(
      buildMessages(
        indexed([{ type: "prompt_queued", prompt_id: "p1", summary: "do the thing", at: AT }]),
        SID,
      ).queue,
    ).toEqual([{ promptId: "p1", summary: "do the thing" }]);

    expect(
      buildMessages(
        indexed([
          { type: "prompt_queued", prompt_id: "p1", summary: "do the thing", at: AT },
          { type: "run_started", run_id: "r1", prompt_summary: null, prompt_id: "p1", at: AT2 },
        ]),
        SID,
      ).queue,
    ).toEqual([]);
  });

  test("prompt_edited updates the queued summary", () => {
    expect(
      buildMessages(
        indexed([
          { type: "prompt_queued", prompt_id: "p1", summary: "v1", at: AT },
          { type: "prompt_edited", prompt_id: "p1", summary: "v2", at: AT2 },
        ]),
        SID,
      ).queue,
    ).toEqual([{ promptId: "p1", summary: "v2" }]);
  });

  test("prompt_dequeued removes the queue entry; the message never enters the transcript", () => {
    const result = buildMessages(
      indexed([
        {
          type: "agent_message",
          run_id: "",
          message_id: "u1",
          role: "user",
          text: "hi",
          prompt_id: "p1",
          at: AT,
        },
        { type: "prompt_queued", prompt_id: "p1", summary: "hi", at: AT },
        { type: "prompt_dequeued", prompt_id: "p1", at: AT2 },
      ]),
      SID,
    );
    expect(result.queue).toEqual([]);
    // Held while queued, then recalled/cancelled — it never joined the conversation.
    expect(real(result.messages).some((m) => m.id === "p1")).toBe(false);
  });

  test("multiple queued prompts stay oldest→newest (↑ recalls the newest)", () => {
    expect(
      buildMessages(
        indexed([
          { type: "prompt_queued", prompt_id: "p1", summary: "first", at: AT },
          { type: "prompt_queued", prompt_id: "p2", summary: "second", at: AT2 },
        ]),
        SID,
      ).queue.map((q) => q.promptId),
    ).toEqual(["p1", "p2"]);
  });

  test("a message queued mid-run lands at its consumption point, below the prior response (59cb8557)", () => {
    const { messages } = buildMessages(
      indexed([
        {
          type: "agent_message",
          run_id: "",
          message_id: "u0",
          role: "user",
          text: "Count 1-60",
          prompt_id: "p0",
          at: AT,
        },
        { type: "run_started", run_id: "r1", prompt_summary: null, prompt_id: "p0", at: AT },
        // "Hi" queued mid-1-60-run: its echo is logged HERE, before the response.
        {
          type: "agent_message",
          run_id: "",
          message_id: "u1",
          role: "user",
          text: "Hi",
          prompt_id: "p1",
          at: AT,
        },
        { type: "prompt_queued", prompt_id: "p1", summary: "Hi", at: AT },
        {
          type: "agent_message",
          run_id: "r1",
          message_id: "a1",
          role: "assistant",
          text: "1. one…",
          at: AT,
        },
        { type: "run_interrupted", run_id: "r1", at: AT },
        // consumed AFTER the interrupt
        { type: "run_started", run_id: "r2", prompt_summary: null, prompt_id: "p1", at: AT },
        {
          type: "agent_message",
          run_id: "r2",
          message_id: "a2",
          role: "assistant",
          text: "Hi! How can I help?",
          at: AT,
        },
        { type: "run_completed", run_id: "r2", ok: true, at: AT },
      ]),
      SID,
    );
    const seq = real(messages).map((m) => {
      const c = m.content;
      const text = Array.isArray(c)
        ? ((c.find((p) => p.type === "text") as { text?: string } | undefined)?.text ?? "")
        : "";
      return { role: m.role, text };
    });
    // Count(user) → 1-60(assistant) → Hi(user) → Hi-response(assistant): the
    // queued "Hi" is BELOW the interrupted 1-60 response, not above it.
    expect(seq.map((s) => s.role)).toEqual(["user", "assistant", "user", "assistant"]);
    expect(seq.map((s) => s.text)).toEqual(["Count 1-60", "1. one…", "Hi", "Hi! How can I help?"]);
  });
});

describe("buildMessages — Phase 1c live token streaming", () => {
  const asstOf = (messages: ReturnType<typeof buildMessages>["messages"]) =>
    real(messages).find((m) => m.role === "assistant");

  test("streamingText renders into the in-flight assistant turn while a run is open", () => {
    const { messages, isRunning } = buildMessages(
      indexed([{ type: "run_started", run_id: "r1", prompt_summary: null, at: AT }]),
      SID,
      undefined,
      "hello wor",
    );
    expect(isRunning).toBe(true);
    const a = asstOf(messages);
    expect(a?.content).toEqual([{ type: "text", text: "hello wor" }]);
    expect(a?.status?.type).toBe("running");
  });

  test("the durable agent_message supersedes the tail — no double-render", () => {
    // The hook empties streamingText the instant the durable message lands,
    // so buildMessages sees the durable event + an EMPTY tail; the rendered
    // text comes wholly from the durable event (never doubled).
    const { messages } = buildMessages(
      indexed([
        { type: "run_started", run_id: "r1", prompt_summary: null, at: AT },
        {
          type: "agent_message",
          run_id: "r1",
          message_id: "a1",
          role: "assistant",
          text: "hello world",
          at: AT,
        },
        { type: "run_completed", run_id: "r1", ok: true, at: AT },
      ]),
      SID,
      undefined,
      "",
    );
    expect(asstOf(messages)?.content).toEqual([{ type: "text", text: "hello world" }]);
  });

  test("a stale tail does NOT resurrect a finished run (gated on runOpen)", () => {
    // Crash/teleport edge: even if a straggler tail were handed in after the
    // run closed, it must not append — the run is done.
    const { messages } = buildMessages(
      indexed([
        { type: "run_started", run_id: "r1", prompt_summary: null, at: AT },
        {
          type: "agent_message",
          run_id: "r1",
          message_id: "a1",
          role: "assistant",
          text: "done",
          at: AT,
        },
        { type: "run_completed", run_id: "r1", ok: true, at: AT },
      ]),
      SID,
      undefined,
      "ghost tokens",
    );
    expect(asstOf(messages)?.content).toEqual([{ type: "text", text: "done" }]);
  });

  test("tail appends after durable text + a tool boundary in the same run", () => {
    const { messages } = buildMessages(
      indexed([
        { type: "run_started", run_id: "r1", prompt_summary: null, at: AT },
        {
          type: "agent_message",
          run_id: "r1",
          message_id: "a1",
          role: "assistant",
          text: "block one",
          at: AT,
        },
        {
          type: "tool_call_started",
          run_id: "r1",
          tool_call_id: "t1",
          tool_name: "Read",
          args_summary: null,
          at: AT,
        },
        {
          type: "tool_call_completed",
          run_id: "r1",
          tool_call_id: "t1",
          tool_name: "Read",
          ok: true,
          duration_ms: 5,
          result_summary: null,
          at: AT,
        },
      ]),
      SID,
      undefined,
      "block two streaming",
    );
    const content = asstOf(messages)?.content;
    const textParts = (Array.isArray(content) ? content : [])
      .filter((p) => p.type === "text")
      .map((p) => (p as { text: string }).text);
    expect(textParts).toContain("block one");
    expect(textParts).toContain("block two streaming");
  });
});

describe("buildMessages — stable assistant ids (crash regression, session 59cb8557)", () => {
  // A queued+dequeued user message before a run's assistant turn used to shift the
  // position-based `a:${out.length}` id between the streaming-tail render and the
  // durable-message render — assistant-ui keys by id, so the turn silently changing
  // id threw "a message with the same id already exists in the parent tree".
  const base: SessionEvent[] = [
    {
      type: "agent_message",
      run_id: "",
      message_id: "u0",
      role: "user",
      text: "Count 1-60",
      prompt_id: "p0",
      at: AT,
    },
    { type: "run_started", run_id: "r1", prompt_summary: null, prompt_id: "p0", at: AT },
    {
      type: "agent_message",
      run_id: "",
      message_id: "u1",
      role: "user",
      text: "Hi",
      prompt_id: "p1",
      at: AT,
    },
    { type: "prompt_queued", prompt_id: "p1", summary: "Hi", at: AT },
    { type: "prompt_dequeued", prompt_id: "p1", at: AT }, // the ↑-recall — shrinks out post-loop
    {
      type: "agent_message",
      run_id: "",
      message_id: "u2",
      role: "user",
      text: "Hi",
      prompt_id: "p2",
      at: AT,
    },
    { type: "prompt_queued", prompt_id: "p2", summary: "Hi", at: AT },
  ];

  test("the in-flight turn keeps its id from streaming-tail render to durable render", () => {
    // Mid-stream: the 1-60 turn lives in the ephemeral overlay, no durable assistant yet.
    const streaming = buildMessages(indexed(base), SID, undefined, "1. one…");
    // Durable: the complete 1-60 assistant message has landed; overlay empty.
    const durable = buildMessages(
      indexed([
        ...base,
        {
          type: "agent_message",
          run_id: "r1",
          message_id: "a1",
          role: "assistant",
          text: "1. one…",
          at: AT,
        },
      ]),
      SID,
    );
    const sId = real(streaming.messages).find((m) => m.role === "assistant")?.id;
    const dId = real(durable.messages).find((m) => m.role === "assistant")?.id;
    expect(sId).toBeDefined();
    expect(sId).toBe(dId); // was a:2 (streaming) vs a:3 (durable) with the position-based id
  });

  test("no build produces duplicate message ids", () => {
    for (const streamingText of ["", "1. one…"]) {
      const ids = buildMessages(indexed(base), SID, undefined, streamingText).messages.map(
        (m) => m.id,
      );
      expect(new Set(ids).size).toBe(ids.length);
    }
  });
});

describe("buildMessages — ADR 0054 interactive AskUserQuestion", () => {
  const Q: UserQuestion = {
    question: "Which database?",
    header: "Database",
    multiSelect: false,
    options: [
      { label: "Postgres", description: "Relational, default" },
      { label: "MySQL", description: "Also relational" },
    ],
  };

  const toolParts = (messages: ReturnType<typeof buildMessages>["messages"]) =>
    real(messages).flatMap((m) =>
      ((m.content as ReadonlyArray<{ type: string }>) ?? []).filter((p) => p.type === "tool-call"),
    ) as Array<{ toolName?: string; toolCallId?: string }>;

  test("a deferred question becomes a user_question system card (unanswered)", () => {
    const { messages, isRunning } = buildMessages(
      indexed([
        { type: "run_started", run_id: "r1", prompt_summary: null, at: AT },
        {
          type: "agent_message",
          run_id: "r1",
          message_id: "a1",
          role: "assistant",
          text: "I need to confirm a detail",
          at: AT,
        },
        {
          type: "tool_call_started",
          run_id: "r1",
          tool_call_id: "t1",
          tool_name: "AskUserQuestion",
          args_summary: '{"questions":[]}',
          at: AT,
        },
        { type: "user_question", run_id: "r1", tool_call_id: "t1", questions: [Q], at: AT },
        { type: "run_completed", run_id: "r1", ok: false, at: AT2 },
      ]),
      SID,
      "idle",
    );
    const card = real(messages).find((m) => customMarker(m)?.kind === "user_question")!;
    expect(card.role).toBe("system");
    const marker = customMarker(card) as Extract<SystemMarker, { kind: "user_question" }>;
    expect(marker).toMatchObject({
      kind: "user_question",
      toolCallId: "t1",
      answers: null,
      via: "legacy",
    });
    expect(marker.questions).toEqual([Q]);
    // Awaiting input is NOT "working" — the composer must not show a spinner.
    expect(isRunning).toBe(false);
  });

  test("the generic AskUserQuestion tool part is suppressed (deduped against the card)", () => {
    const { messages } = buildMessages(
      indexed([
        { type: "run_started", run_id: "r1", prompt_summary: null, at: AT },
        {
          type: "agent_message",
          run_id: "r1",
          message_id: "a1",
          role: "assistant",
          text: "thinking",
          at: AT,
        },
        // A real tool call in the same run must still render…
        {
          type: "tool_call_started",
          run_id: "r1",
          tool_call_id: "tr",
          tool_name: "Read",
          args_summary: '{"file_path":"a.rs"}',
          at: AT,
        },
        {
          type: "tool_call_completed",
          run_id: "r1",
          tool_call_id: "tr",
          tool_name: "Read",
          ok: true,
          duration_ms: 3,
          result_summary: "ok",
          at: AT,
        },
        // …but the AskUserQuestion tool call must NOT.
        {
          type: "tool_call_started",
          run_id: "r1",
          tool_call_id: "t1",
          tool_name: "AskUserQuestion",
          args_summary: '{"questions":[]}',
          at: AT,
        },
        { type: "user_question", run_id: "r1", tool_call_id: "t1", questions: [Q], at: AT },
        { type: "run_completed", run_id: "r1", ok: false, at: AT2 },
      ]),
      SID,
      "idle",
    );
    const parts = toolParts(messages);
    expect(parts.some((p) => p.toolName === "AskUserQuestion")).toBe(false);
    expect(parts.some((p) => p.toolCallId === "t1")).toBe(false);
    // The real Read call survives.
    expect(parts.some((p) => p.toolName === "Read")).toBe(true);
    // The question isn't tallied as a tool run (only the Read is).
    const footer = real(messages).find((m) => m.role === "assistant")!.metadata?.custom
      ?.run as RunFooter;
    expect(footer).toMatchObject({ reads: 1, other: 0, ok: false });
  });

  test("a run that ONLY deferred a question leaves no stray empty assistant bubble", () => {
    const { messages } = buildMessages(
      indexed([
        { type: "run_started", run_id: "r1", prompt_summary: null, at: AT },
        {
          type: "tool_call_started",
          run_id: "r1",
          tool_call_id: "t1",
          tool_name: "AskUserQuestion",
          args_summary: null,
          at: AT,
        },
        { type: "user_question", run_id: "r1", tool_call_id: "t1", questions: [Q], at: AT },
        { type: "run_completed", run_id: "r1", ok: false, at: AT2 },
      ]),
      SID,
      "idle",
    );
    const msgs = real(messages);
    // No empty assistant message synthesized just to hold a footer.
    expect(msgs.filter((m) => m.role === "assistant")).toHaveLength(0);
    expect(msgs.filter((m) => customMarker(m)?.kind === "user_question")).toHaveLength(1);
  });

  test("the answer (arriving in a later resume run) folds onto the card and suppresses its tool_result", () => {
    const { messages } = buildMessages(
      indexed([
        { type: "run_started", run_id: "r1", prompt_summary: null, at: AT },
        {
          type: "tool_call_started",
          run_id: "r1",
          tool_call_id: "t1",
          tool_name: "AskUserQuestion",
          args_summary: null,
          at: AT,
        },
        { type: "user_question", run_id: "r1", tool_call_id: "t1", questions: [Q], at: AT },
        { type: "run_completed", run_id: "r1", ok: false, at: AT2 },
        // The deferred tool re-fires on `--resume` in a fresh run; its
        // tool_result + the QuestionAnswered land here.
        { type: "run_started", run_id: "r2", prompt_summary: null, at: AT2 },
        {
          type: "tool_call_completed",
          run_id: "r2",
          tool_call_id: "t1",
          tool_name: "",
          ok: true,
          duration_ms: 0,
          result_summary: "Postgres",
          at: AT2,
        },
        {
          type: "question_answered",
          run_id: "r2",
          tool_call_id: "t1",
          answers: { "Which database?": ["Postgres"] },
          at: AT2,
        },
        {
          type: "agent_message",
          run_id: "r2",
          message_id: "a2",
          role: "assistant",
          text: "Great — using Postgres.",
          at: AT2,
        },
        { type: "run_completed", run_id: "r2", ok: true, at: AT2 },
      ]),
      SID,
      "idle",
    );
    const card = real(messages).find((m) => customMarker(m)?.kind === "user_question")!;
    const marker = customMarker(card) as Extract<SystemMarker, { kind: "user_question" }>;
    expect(marker.answers).toEqual({ "Which database?": ["Postgres"] });
    // The re-fired tool_result for the question is NOT rendered as a tool part.
    expect(toolParts(messages).some((p) => p.toolCallId === "t1")).toBe(false);
    // The resume run's own assistant reply still renders.
    expect(real(messages).some((m) => m.role === "assistant")).toBe(true);
  });

  test("a generic ask_user_question request renders the same unanswered card", () => {
    const { messages, isRunning } = buildMessages(
      indexed([
        { type: "run_started", run_id: "r1", prompt_summary: null, at: AT },
        {
          type: "tool_call_requested",
          run_id: "r1",
          tool_call_id: "t-generic",
          name: "ask_user_question",
          args_json: JSON.stringify({ questions: [Q] }),
          at: AT,
        },
        { type: "run_completed", run_id: "r1", ok: false, at: AT2 },
      ]),
      SID,
      "idle",
    );

    const card = real(messages).find((m) => customMarker(m)?.kind === "user_question")!;
    expect(customMarker(card)).toMatchObject({
      kind: "user_question",
      toolCallId: "t-generic",
      questions: [Q],
      answers: null,
      via: "generic",
    });
    expect(toolParts(messages)).toHaveLength(0);
    expect(isRunning).toBe(false);
  });

  test("tool_result_submitted folds canonical answers onto the generic card", () => {
    const { messages } = buildMessages(
      indexed([
        {
          type: "tool_call_requested",
          run_id: "r1",
          tool_call_id: "t-generic",
          name: "ask_user_question",
          args_json: JSON.stringify({ questions: [Q] }),
          at: AT,
        },
        {
          type: "tool_result_submitted",
          tool_call_id: "t-generic",
          result_json: JSON.stringify({ "Which database?": ["Postgres"] }),
          at: AT2,
        },
      ]),
      SID,
      "idle",
    );

    const card = real(messages).find((m) => customMarker(m)?.kind === "user_question")!;
    expect(customMarker(card)).toMatchObject({
      toolCallId: "t-generic",
      answers: { "Which database?": ["Postgres"] },
      via: "generic",
    });
  });

  test("#64389 multi-fire suppresses phantom AskUserQuestion starts but keeps the one real card", () => {
    const { messages } = buildMessages(
      indexed([
        { type: "run_started", run_id: "r1", prompt_summary: null, at: AT },
        {
          type: "tool_call_started",
          run_id: "r1",
          tool_call_id: "phantom-1",
          tool_name: "AskUserQuestion",
          args_summary: JSON.stringify({ questions: [Q] }),
          at: AT,
        },
        {
          type: "tool_call_started",
          run_id: "r1",
          tool_call_id: "phantom-2",
          tool_name: "AskUserQuestion",
          args_summary: JSON.stringify({ questions: [Q] }),
          at: AT,
        },
        {
          type: "tool_call_started",
          run_id: "r1",
          tool_call_id: "real-call",
          tool_name: "AskUserQuestion",
          args_summary: JSON.stringify({ questions: [Q] }),
          at: AT,
        },
        {
          type: "tool_call_requested",
          run_id: "r1",
          tool_call_id: "real-call",
          name: "ask_user_question",
          args_json: JSON.stringify({ questions: [Q] }),
          at: AT,
        },
        { type: "run_completed", run_id: "r1", ok: false, at: AT2 },
      ]),
      SID,
      "idle",
    );

    expect(toolParts(messages)).toHaveLength(0);
    expect(real(messages).filter((m) => customMarker(m)?.kind === "user_question")).toHaveLength(1);
  });

  test("phantom suppression does not hide a legitimate in-flight ordinary tool", () => {
    const { messages } = buildMessages(
      indexed([
        { type: "run_started", run_id: "r1", prompt_summary: null, at: AT },
        {
          type: "tool_call_started",
          run_id: "r1",
          tool_call_id: "read-live",
          tool_name: "Read",
          args_summary: JSON.stringify({ file_path: "README.md" }),
          at: AT,
        },
      ]),
      SID,
      "active",
    );

    expect(toolParts(messages)).toEqual([
      expect.objectContaining({ toolCallId: "read-live", toolName: "Read" }),
    ]);
  });

  test("a prior generic request of the same name does not hide a later in-flight sync start", () => {
    const { messages } = buildMessages(
      indexed([
        { type: "run_started", run_id: "r1", prompt_summary: null, at: AT },
        {
          type: "tool_call_started",
          run_id: "r1",
          tool_call_id: "echo-old",
          tool_name: "dev_echo",
          args_summary: JSON.stringify({ text: "old" }),
          at: AT,
        },
        {
          type: "tool_call_requested",
          run_id: "r1",
          tool_call_id: "echo-old",
          name: "dev_echo",
          args_json: JSON.stringify({ text: "old" }),
          at: AT,
        },
        {
          type: "tool_result_submitted",
          tool_call_id: "echo-old",
          result_json: JSON.stringify({ text: "old" }),
          at: AT,
        },
        {
          type: "tool_call_completed",
          run_id: "r1",
          tool_call_id: "echo-old",
          tool_name: "dev_echo",
          ok: true,
          duration_ms: 1,
          result_summary: "old",
          at: AT,
        },
        { type: "run_completed", run_id: "r1", ok: true, at: AT },
        { type: "run_started", run_id: "r2", prompt_summary: null, at: AT2 },
        {
          type: "tool_call_started",
          run_id: "r2",
          tool_call_id: "echo-live",
          tool_name: "dev_echo",
          args_summary: JSON.stringify({ text: "live" }),
          at: AT2,
        },
      ]),
      SID,
      "active",
    );

    expect(toolParts(messages)).toEqual(
      expect.arrayContaining([
        expect.objectContaining({ toolCallId: "echo-live", toolName: "dev_echo" }),
      ]),
    );
  });
});

describe("buildMessages — generic deferred tool waiting state", () => {
  test("an unsubmitted generic call renders a pending tool row", () => {
    const { messages } = buildMessages(
      indexed([
        {
          type: "tool_call_requested",
          run_id: "r1",
          tool_call_id: "approval-1",
          name: "approve_deploy",
          args_json: JSON.stringify({ environment: "production" }),
          at: AT,
        },
      ]),
      SID,
      "idle",
    );
    const parts = real(messages).flatMap((message) =>
      typeof message.content === "string"
        ? []
        : message.content.filter((part) => part.type === "tool-call"),
    );
    expect(parts).toEqual([
      expect.objectContaining({
        type: "tool-call",
        toolCallId: "approval-1",
        toolName: "approve_deploy",
        args: { environment: "production" },
      }),
    ]);
    expect(parts[0]).not.toHaveProperty("result");
  });
});

describe("buildMessages — ADR 0054 Flavor A file changes", () => {
  // All tool-call parts across the assistant messages.
  const parts = (messages: ReturnType<typeof buildMessages>["messages"]) =>
    real(messages).flatMap((m) =>
      ((m.content as ReadonlyArray<{ type: string }>) ?? []).filter((p) => p.type === "tool-call"),
    ) as Array<{
      toolName?: string;
      toolCallId?: string;
      args?: FileChangeArgs;
      isError?: boolean;
    }>;

  test("a successful edit renders a rich file-change part in place of the generic card", () => {
    const { messages } = buildMessages(
      indexed([
        { type: "run_started", run_id: "r1", prompt_summary: null, at: AT },
        {
          type: "tool_call_started",
          run_id: "r1",
          tool_call_id: "te",
          tool_name: "Edit",
          args_summary: '{"file_path":"src/a.rs"}',
          at: AT,
        },
        {
          type: "tool_call_completed",
          run_id: "r1",
          tool_call_id: "te",
          tool_name: "",
          ok: true,
          duration_ms: 5,
          result_summary: "ok",
          at: AT,
        },
        {
          type: "file_changed",
          run_id: "r1",
          tool_call_id: "te",
          path: "src/a.rs",
          change: { edit: { hunks: [{ old: "let x = 1;", new: "let x = 2;" }] } },
          at: AT,
        },
        { type: "run_completed", run_id: "r1", ok: true, at: AT2 },
      ]),
      SID,
      "idle",
    );
    const tps = parts(messages);
    // No generic "Edit" card — it was swapped for the rich diff.
    expect(tps.some((p) => p.toolName === "Edit")).toBe(false);
    const fc = tps.find((p) => p.toolName === FILE_CHANGE_TOOL)!;
    expect(fc.toolCallId).toBe("te");
    expect(fc.args?.path).toBe("src/a.rs");
    expect(fc.args?.change.edit?.hunks).toEqual([{ old: "let x = 1;", new: "let x = 2;" }]);
    // Still tallied as an edit in the run footer.
    const footer = real(messages).find((m) => m.role === "assistant")!.metadata?.custom
      ?.run as RunFooter;
    expect(footer.edits).toBe(1);
  });

  test("browser activity replaces its correlated Shell card and keeps the real outcome", () => {
    const { messages } = buildMessages(
      indexed([
        { type: "run_started", run_id: "r1", prompt_summary: null, at: AT },
        {
          type: "browser_activity",
          run_id: "r1",
          tool_call_id: "tb",
          intent: "Clicking Sign in",
          at: AT,
        },
        {
          type: "tool_call_started",
          run_id: "r1",
          tool_call_id: "tb",
          tool_name: "Shell",
          args_summary: 'ENGRAM_BROWSER_INTENT="Clicking Sign in" playwright-cli click e7',
          at: AT,
        },
        {
          type: "tool_call_completed",
          run_id: "r1",
          tool_call_id: "tb",
          tool_name: "Shell",
          ok: false,
          duration_ms: 5,
          result_summary: "element not found",
          at: AT2,
        },
        { type: "run_completed", run_id: "r1", ok: false, at: AT2 },
      ]),
      SID,
      "idle",
    );
    const tps = parts(messages);
    expect(tps.some((p) => p.toolName === "Shell")).toBe(false);
    const browser = tps.find((p) => p.toolName === BROWSER_ACTIVITY_TOOL)!;
    expect(browser).toMatchObject({
      toolCallId: "tb",
      args: { intent: "Clicking Sign in" },
      isError: true,
    });
  });

  test("a write renders a file-change part carrying the content", () => {
    const { messages } = buildMessages(
      indexed([
        { type: "run_started", run_id: "r1", prompt_summary: null, at: AT },
        {
          type: "tool_call_started",
          run_id: "r1",
          tool_call_id: "tw",
          tool_name: "Write",
          args_summary: null,
          at: AT,
        },
        {
          type: "tool_call_completed",
          run_id: "r1",
          tool_call_id: "tw",
          tool_name: "",
          ok: true,
          duration_ms: 1,
          result_summary: "ok",
          at: AT,
        },
        {
          type: "file_changed",
          run_id: "r1",
          tool_call_id: "tw",
          path: "new.txt",
          change: { write: { content: "hello\nworld\n" } },
          at: AT,
        },
        { type: "run_completed", run_id: "r1", ok: true, at: AT2 },
      ]),
      SID,
      "idle",
    );
    const fc = parts(messages).find((p) => p.toolName === FILE_CHANGE_TOOL)!;
    expect(fc.args?.path).toBe("new.txt");
    expect(fc.args?.change.write?.content).toBe("hello\nworld\n");
  });

  test("a FAILED edit (no file_changed) keeps its generic error card", () => {
    const { messages } = buildMessages(
      indexed([
        { type: "run_started", run_id: "r1", prompt_summary: null, at: AT },
        {
          type: "tool_call_started",
          run_id: "r1",
          tool_call_id: "tf",
          tool_name: "Edit",
          args_summary: '{"file_path":"a.rs"}',
          at: AT,
        },
        {
          type: "tool_call_completed",
          run_id: "r1",
          tool_call_id: "tf",
          tool_name: "",
          ok: false,
          duration_ms: 1,
          result_summary: "String to replace not found",
          at: AT,
        },
        { type: "run_completed", run_id: "r1", ok: true, at: AT2 },
      ]),
      SID,
      "idle",
    );
    const tps = parts(messages);
    // No rich diff (no file_changed was emitted); the generic Edit card shows
    // the failure.
    expect(tps.some((p) => p.toolName === FILE_CHANGE_TOOL)).toBe(false);
    const edit = tps.find((p) => p.toolCallId === "tf")!;
    expect(edit.toolName).toBe("Edit");
    expect(edit.isError).toBe(true);
  });
});

// ---------------------------------------------------------------------------
// ADR 0107: plan cards, mode markers, and the derived mode/pending state.
// ---------------------------------------------------------------------------

describe("buildMessages — ADR 0107 plan mode", () => {
  const planRequested = (toolCallId: string, plan = "# The plan"): SessionEvent => ({
    type: "tool_call_requested",
    run_id: "r1",
    tool_call_id: toolCallId,
    name: "exit_plan_mode",
    args_json: JSON.stringify({ plan }),
    at: AT,
  });

  test("an unresolved plan renders a plan marker and reports pendingPlan", () => {
    const { messages, pendingPlan, currentMode } = buildMessages(
      indexed([
        { type: "harness_mode_changed", mode: "plan", at: AT },
        { type: "run_started", run_id: "r1", prompt_summary: null, at: AT },
        planRequested("t-plan"),
        { type: "run_completed", run_id: "r1", ok: true, at: AT2 },
      ]),
      SID,
      "idle",
    );
    const plan = messages
      .map((m) => customMarker(m))
      .find((mk): mk is Extract<SystemMarker, { kind: "plan" }> => mk?.kind === "plan");
    expect(plan).toBeTruthy();
    expect(plan!.plan).toBe("# The plan");
    expect(plan!.revision).toBe(1);
    expect(plan!.resolution).toBeNull();
    expect(pendingPlan).toEqual({ toolCallId: "t-plan" });
    expect(currentMode).toBe("plan");
    // The mode directive renders its faint marker.
    expect(messages.map((m) => customMarker(m)).some((mk) => mk?.kind === "mode")).toBe(true);
  });

  test("a resolved plan folds its decision, clears pendingPlan, and an approval flips the mode", () => {
    const { messages, pendingPlan, currentMode } = buildMessages(
      indexed([
        { type: "harness_mode_changed", mode: "plan", at: AT },
        planRequested("t-plan"),
        {
          type: "tool_result_submitted",
          tool_call_id: "t-plan",
          result_json: JSON.stringify({ decision: "approve" }),
          at: AT2,
        },
        // The re-fire's completion is the card's receipt, not a tool row.
        {
          type: "tool_call_completed",
          run_id: "r2",
          tool_call_id: "t-plan",
          tool_name: "exit_plan_mode",
          ok: true,
          duration_ms: 0,
          result_summary: "approved",
          at: AT2,
        },
      ]),
      SID,
      "idle",
    );
    const plan = messages
      .map((m) => customMarker(m))
      .find((mk): mk is Extract<SystemMarker, { kind: "plan" }> => mk?.kind === "plan");
    expect(plan!.resolution).toEqual({ approved: true, feedback: null, at: AT2 });
    expect(pendingPlan).toBeNull();
    expect(currentMode).toBe("default");
    // No stray completed tool row for the plan id.
    const toolRows = messages.flatMap((m) =>
      typeof m.content === "string"
        ? []
        : m.content.filter(
            (p) => p.type === "tool-call" && "toolCallId" in p && p.toolCallId === "t-plan",
          ),
    );
    expect(toolRows).toHaveLength(0);
  });

  test("reject keeps plan mode and revisions number sequentially", () => {
    const { messages, pendingPlan, currentMode } = buildMessages(
      [
        { idx: 0, event: { type: "harness_mode_changed", mode: "plan", at: AT } },
        { idx: 1, event: planRequested("t-plan-1", "# v1") },
        {
          idx: 2,
          event: {
            type: "tool_result_submitted",
            tool_call_id: "t-plan-1",
            result_json: JSON.stringify({ decision: "reject", feedback: "add tests" }),
            at: AT2,
          },
        },
        { idx: 3, event: planRequested("t-plan-2", "# v2") },
      ],
      SID,
      "idle",
    );
    const plans = messages
      .map((m) => customMarker(m))
      .filter((mk): mk is Extract<SystemMarker, { kind: "plan" }> => mk?.kind === "plan");
    expect(plans).toHaveLength(2);
    expect(plans[0]!.revision).toBe(1);
    expect(plans[0]!.resolution).toEqual({ approved: false, feedback: "add tests", at: AT2 });
    expect(plans[1]!.revision).toBe(2);
    expect(plans[1]!.resolution).toBeNull();
    expect(pendingPlan).toEqual({ toolCallId: "t-plan-2" });
    expect(currentMode).toBe("plan");
  });

  test("a terminal session never reports a pending plan", () => {
    const { pendingPlan } = buildMessages(indexed([planRequested("t-plan")]), SID, "dead");
    expect(pendingPlan).toBeNull();
  });
});

describe("buildMessages — ADR 0107 out-of-mode plan attempt", () => {
  test("an exit_plan_mode start with no generic request becomes a hint marker, not a tool row", () => {
    const { messages } = buildMessages(
      indexed([
        { type: "run_started", run_id: "r1", prompt_summary: null, at: AT },
        {
          type: "tool_call_started",
          run_id: "r1",
          tool_call_id: "t-attempt",
          tool_name: "exit_plan_mode",
          args_summary: '{"plan":"a plan"}',
          at: AT,
        },
        {
          type: "tool_call_completed",
          run_id: "r1",
          tool_call_id: "t-attempt",
          tool_name: "exit_plan_mode",
          ok: false,
          duration_ms: 1,
          result_summary: null,
          at: AT,
        },
        { type: "run_completed", run_id: "r1", ok: true, at: AT2 },
      ]),
      SID,
      "idle",
    );
    const attempt = messages.map((m) => customMarker(m)).find((mk) => mk?.kind === "plan_attempt");
    expect(attempt).toBeTruthy();
    const toolRows = messages.flatMap((m) =>
      typeof m.content === "string"
        ? []
        : m.content.filter(
            (p) => p.type === "tool-call" && "toolCallId" in p && p.toolCallId === "t-attempt",
          ),
    );
    expect(toolRows).toHaveLength(0);
  });

  test("a REAL plan request still renders the card, never the attempt hint", () => {
    const { messages } = buildMessages(
      indexed([
        {
          type: "tool_call_requested",
          run_id: "r1",
          tool_call_id: "t-plan",
          name: "exit_plan_mode",
          args_json: JSON.stringify({ plan: "# P" }),
          at: AT,
        },
        {
          type: "tool_call_started",
          run_id: "r1",
          tool_call_id: "t-plan",
          tool_name: "exit_plan_mode",
          args_summary: null,
          at: AT,
        },
      ]),
      SID,
      "idle",
    );
    const kinds = messages.map((m) => customMarker(m)?.kind).filter(Boolean);
    expect(kinds).toContain("plan");
    expect(kinds).not.toContain("plan_attempt");
  });
});
