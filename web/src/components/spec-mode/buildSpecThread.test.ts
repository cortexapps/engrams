import { describe, expect, test } from "vitest";

import { buildMessages } from "@/components/session-thread/buildMessages";
import type { IndexedEvent, SessionEvent } from "@/lib/types";
import type { SpecMessage } from "@/hooks/useSpecMessages";
import { buildSpecThread, parseSpecAgentText } from "./buildSpecThread";

const AT = "2026-08-13T10:00:00.000Z";

describe("buildSpecThread", () => {
  test("whitelists stored human text, agent prose, citations, and coalesced document activity", () => {
    const messages = buildMessages(
      indexed([
        {
          type: "agent_message",
          run_id: "",
          message_id: "human-echo",
          role: "user",
          prompt_id: "prompt-1",
          text: "[speaker: Priya]\nNever show this wire text.",
          at: AT,
        },
        {
          type: "run_started",
          run_id: "run-1",
          prompt_id: "prompt-1",
          prompt_summary: null,
          at: AT,
        },
        {
          type: "agent_message",
          run_id: "run-1",
          message_id: "agent-1",
          role: "assistant",
          text: "The limiter is per user. `gateway/limits.rs @ 8f2c1a4`",
          at: AT,
        },
        {
          type: "tool_call_started",
          run_id: "run-1",
          tool_call_id: "update-1",
          tool_name: "mcp__engrams__spec_update_section",
          args_summary: JSON.stringify({ section_id: "problem" }),
          at: AT,
        },
        {
          type: "tool_call_started",
          run_id: "run-1",
          tool_call_id: "shell-1",
          tool_name: "Read",
          args_summary: JSON.stringify({ file_path: "/workspace/spec.md" }),
          at: AT,
        },
        {
          type: "tool_call_started",
          run_id: "run-1",
          tool_call_id: "update-2",
          tool_name: "spec_update_section",
          args_summary: JSON.stringify({ section_id: "api" }),
          at: AT,
        },
        { type: "run_completed", run_id: "run-1", ok: true, at: AT },
      ]),
      "session-1",
    ).messages;
    const stored = new Map<string, SpecMessage>([
      [
        "prompt-1",
        {
          promptId: "prompt-1",
          author: { id: "priya", name: "Priya" },
          text: "Keep one parent bucket.",
          createdAt: AT,
        },
      ],
    ]);
    const titles = new Map([
      ["problem", "Problem"],
      ["api", "API"],
    ]);

    expect(buildSpecThread(messages, stored, titles)).toEqual([
      {
        kind: "human",
        id: "human:prompt-1",
        promptId: "prompt-1",
        author: { id: "priya", name: "Priya" },
        text: "Keep one parent bucket.",
        createdAt: AT,
      },
      {
        kind: "agent",
        id: expect.stringMatching(/^agent:/),
        text: "The limiter is per user.",
        citations: ["gateway/limits.rs @ 8f2c1a4"],
        createdAt: AT,
      },
      {
        kind: "document_activity",
        id: expect.stringMatching(/^activity:/),
        sectionIds: ["problem", "api"],
        sectionTitles: ["Problem", "API"],
        createdAt: AT,
      },
    ]);
  });

  test("keeps an unmatched human turn unattributed without exposing its speaker header", () => {
    const messages = buildMessages(
      indexed([
        {
          type: "agent_message",
          run_id: "",
          message_id: "human-echo",
          role: "user",
          text: "[speaker: Forged]\nRaw text",
          at: AT,
        },
      ]),
      "session-1",
    ).messages;

    expect(buildSpecThread(messages, new Map())).toMatchObject([
      { kind: "human", author: null, text: "" },
    ]);
    expect(JSON.stringify(buildSpecThread(messages, new Map()))).not.toContain("speaker:");
  });
});

describe("parseSpecAgentText", () => {
  test("deduplicates repository citation chips and leaves prose", () => {
    expect(
      parseSpecAgentText(
        "First `src/a.ts @ abcdef1` and again `src/a.ts @ abcdef1`. Then `src/b.ts @ 1234567` and `spec se_41c2 · Jun`.",
      ),
    ).toEqual({
      text: "First and again . Then and .",
      citations: ["src/a.ts @ abcdef1", "src/b.ts @ 1234567", "spec se_41c2 · Jun"],
    });
  });
});

function indexed(events: SessionEvent[]): IndexedEvent[] {
  return events.map((event, idx) => ({ idx, event }));
}
