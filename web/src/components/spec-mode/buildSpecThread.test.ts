import { describe, expect, test } from "vitest";

import { buildMessages } from "@/components/session-thread/buildMessages";
import type { IndexedEvent, SessionEvent } from "@/lib/types";
import type { SpecMessage } from "@/hooks/useSpecMessages";
import { buildSpecThread } from "./buildSpecThread";
import { selectionActionPrompt } from "./selection-prompt";

const AT = "2026-08-13T10:00:00.000Z";

describe("buildSpecThread", () => {
  test("whitelists stored human text, agent prose, and coalesced document activity", () => {
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
        // The raw markdown, citation spans included: the thread renders it
        // with the shared Markdown component instead of stripping chips out.
        text: "The limiter is per user. `gateway/limits.rs @ 8f2c1a4`",
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

  test("shows the founding problem statement, which has no stored row", () => {
    // The statement that creates a spec is sent with the session, not through
    // the messages route, so it never gets a row. Hiding it made the first turn
    // of every spec read "This message is not available".
    const messages = buildMessages(
      indexed([
        {
          type: "agent_message",
          run_id: "",
          message_id: "founding-prompt",
          role: "user",
          text: "jq ignores a duplicate --arg name and keeps the last value.",
          at: AT,
        },
      ]),
      "session-1",
    ).messages;

    expect(buildSpecThread(messages, new Map())).toMatchObject([
      {
        kind: "human",
        author: null,
        text: "jq ignores a duplicate --arg name and keeps the last value.",
      },
    ]);
  });

  test("preserves the snapshot name when a stored author's id is null", () => {
    const messages = buildMessages(
      indexed([
        {
          type: "agent_message",
          run_id: "",
          message_id: "human-echo",
          role: "user",
          prompt_id: "prompt-deleted-author",
          text: "[speaker: Grace Hopper]\nRaw text",
          at: AT,
        },
      ]),
      "session-1",
    ).messages;
    const stored = new Map<string, SpecMessage>([
      [
        "prompt-deleted-author",
        {
          promptId: "prompt-deleted-author",
          author: { id: null, name: "Grace Hopper" },
          text: "Keep the snapshot.",
          createdAt: AT,
        },
      ],
    ]);

    expect(buildSpecThread(messages, stored)).toMatchObject([
      {
        kind: "human",
        author: { id: null, name: "Grace Hopper" },
        text: "Keep the snapshot.",
      },
    ]);
  });
});

describe("phase changes and attribution", () => {
  test("renders the start-drafting frame as a system chip, not a speech bubble", () => {
    const messages = buildMessages(
      indexed([
        {
          type: "agent_message",
          run_id: "",
          message_id: "seed-prompt",
          role: "user",
          text: "[start drafting — requested by Priya]",
          at: AT,
        },
      ]),
      "session-1",
    ).messages;

    expect(buildSpecThread(messages, new Map())).toEqual([
      {
        kind: "phase_change",
        id: expect.stringMatching(/^phase:/),
        requestedBy: "Priya",
        createdAt: AT,
      },
    ]);
  });

  test("attributes the founding row-less turn to the owner, and only that one", () => {
    const messages = buildMessages(
      indexed([
        {
          type: "agent_message",
          run_id: "",
          message_id: "founding-prompt",
          role: "user",
          text: "jq ignores a duplicate --arg name and keeps the last value.",
          at: AT,
        },
        {
          type: "agent_message",
          run_id: "",
          message_id: "later-rowless",
          role: "user",
          text: "A later turn without a stored row.",
          at: AT,
        },
      ]),
      "session-1",
    ).messages;
    const owner = { id: "nikhil", name: "Nikhil" };

    expect(buildSpecThread(messages, new Map(), new Map(), owner)).toMatchObject([
      { kind: "human", author: owner },
      { kind: "human", author: null },
    ]);
  });

  // A selection action reaches the agent as a prompt full of Yjs anchors and a
  // fingerprint. That prompt is the person's turn, so it is also what the
  // transcript renders - and it used to render the plumbing verbatim.
  test("renders a selection turn as the passage and the request", () => {
    const prompt = selectionActionPrompt({
      specId: "spec-1",
      action: "ask",
      instruction: "Answer in chat about this passage.",
      span: {
        specId: "spec-1",
        sectionId: "section-1",
        revision: "56",
        startAnchor: "yjs-section://section-1/00a1bedfb2010100",
        endAnchor: "yjs-section://section-1/02a1bedfb2010000",
        selectedText: "Rendering is repeated on every read.",
        sliceFingerprint: "b115155dd60c95af34e990ae4f06fc846b4c518d0177fab94306dc69cc3067da",
      },
    });
    const messages = buildMessages(
      indexed([
        {
          type: "agent_message",
          run_id: "",
          message_id: "sel",
          role: "user",
          prompt_id: "prompt-sel",
          text: prompt,
          at: AT,
        },
      ]),
      "session-1",
    ).messages;
    const stored = new Map<string, SpecMessage>([
      [
        "prompt-sel",
        {
          promptId: "prompt-sel",
          text: prompt,
          author: { id: "u1", name: "Nikhil" },
          createdAt: AT,
        },
      ],
    ]);

    const entries = buildSpecThread(messages, stored, new Map([["section-1", "Problem"]]));

    expect(entries).toMatchObject([
      {
        kind: "human",
        text: "Asked about a passage in \u00a7Problem\n\n> Rendering is repeated on every read.",
      },
    ]);
    const rendered = JSON.stringify(entries);
    expect(rendered).not.toContain("yjs-section");
    expect(rendered).not.toContain("selection_fingerprint");
  });
});

function indexed(events: SessionEvent[]): IndexedEvent[] {
  return events.map((event, idx) => ({ idx, event }));
}
