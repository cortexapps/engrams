// Tests for the assistant-ui adapter (buildMessages) — the successor to
// Transcript's buildBlocks. These carry the logic coverage: per-run
// grouping, parts ordering, shell-exec synthesis, the harness-register
// system markers, the run footer, and the derived isRunning flag.

import { describe, expect, test } from "vitest";
import { buildMessages, SHELL_TOOL, type RunFooter, type SystemMarker } from "./buildMessages";
import type { IndexedEvent, SessionEvent } from "../../types";

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
          rusage: { duration_ms: 4200 },
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
          rusage: { duration_ms: 3 },
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
    expect((a.metadata?.custom?.run as RunFooter).interrupted).toBe(true);
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

  test("pull_request_opened becomes a pull_request system marker", () => {
    const { messages } = buildMessages(
      indexed([
        {
          type: "pull_request_opened",
          url: "https://gh/x/pull/7",
          repo: "x/y",
          title: "Fix it",
          number: 7,
          head_branch: "fix",
          base_branch: "main",
          at: AT,
        },
      ]),
      SID,
    );
    expect(customMarker(real(messages)[0]!)).toMatchObject({
      kind: "pull_request",
      number: 7,
      repo: "x/y",
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
