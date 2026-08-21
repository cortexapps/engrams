import { describe, expect, test } from "vitest";

import { buildMessages } from "@/components/session-thread/buildMessages";
import type { IndexedEvent } from "@/lib/types";

/**
 * The working indicator is the only thing that tells a spec owner the agent is
 * still on it. A prod spec ran one Bash tool call for THIRTY minutes while the
 * pane looked idle, which reads as a dead session — the natural response is to
 * re-send the instruction, or give up on the spec.
 *
 * This replays that session's real event sequence (spec 7f9a275a, idx 143-155)
 * to pin the invariant the indicator depends on: a run stays open across a long
 * tool call, and a prompt steered INTO that run does not end it. Two prompts
 * arrived mid-call there and both were honored after the call returned, so a
 * steer must never read as "the turn is over".
 */
const SEQUENCE: Array<Record<string, unknown>> = [
  { type: "harness_parked" },
  { type: "prompt_received", prompt_id: "spec-chat:A" },
  {
    type: "agent_message",
    role: "user",
    text: "[speaker: nikhil]\nThis is the engrams repo, not brain-backend.",
    prompt_id: "spec-chat:A",
    message_id: "user-A",
  },
  { type: "run_started", run_id: "run-1", prompt_id: "spec-chat:A" },
  { type: "generation", run_id: "run-1", model: "claude-opus-5" },
  {
    type: "agent_message",
    role: "assistant",
    text: "Let me find the engrams repo.",
    run_id: "run-1",
    message_id: "msg-1",
  },
  // The thirty-minute one: `grep -rIl -i firecracker /` over the microVM root.
  {
    type: "tool_call_started",
    run_id: "run-1",
    tool_call_id: "tc-1",
    tool_name: "Bash",
    args_summary: '{"command":"grep -rIl -i firecracker /"}',
  },
  { type: "prompt_received", prompt_id: "spec-start-drafting:S" },
  {
    type: "agent_message",
    role: "user",
    text: "[start drafting — requested by nikhil]",
    prompt_id: "spec-start-drafting:S",
    message_id: "user-S",
  },
  { type: "prompt_steered", prompt_id: "spec-start-drafting:S" },
  { type: "prompt_received", prompt_id: "spec-chat:B" },
  {
    type: "agent_message",
    role: "user",
    text: "Draft §Problem now.",
    prompt_id: "spec-chat:B",
    message_id: "user-B",
  },
  { type: "prompt_steered", prompt_id: "spec-chat:B" },
];

const events: IndexedEvent[] = SEQUENCE.map(
  (event, i) => ({ idx: 143 + i, event }) as unknown as IndexedEvent,
);

describe("the spec working indicator across a long tool call", () => {
  const upTo = (count: number) => buildMessages(events.slice(0, count), "spec-1");

  test("a run stays running from run_started until it completes", () => {
    expect(upTo(4).isRunning).toBe(true);
    expect(upTo(6).isRunning).toBe(true);
    // Mid tool call — the whole thirty minutes lives here.
    expect(upTo(7).isRunning).toBe(true);
  });

  test("a prompt steered into an open run does not end the turn", () => {
    expect(upTo(10).isRunning).toBe(true);
    expect(upTo(13).isRunning).toBe(true);
  });

  test("the run ends once the harness reports it is idle again", () => {
    const completed = [
      ...events,
      { idx: 200, event: { type: "run_completed", run_id: "run-1" } } as unknown as IndexedEvent,
    ];
    // `run_completed` alone leaves the steered user turns looking unanswered —
    // they are the tail of the thread, and a trailing user turn reads as
    // awaiting a reply. `harness_idle` is what settles it.
    const idle = [
      ...completed,
      { idx: 201, event: { type: "harness_idle" } } as unknown as IndexedEvent,
    ];
    expect(buildMessages(idle, "spec-1").isRunning).toBe(false);
  });
});
