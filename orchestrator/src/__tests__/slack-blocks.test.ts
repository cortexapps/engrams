/**
 * Slack Block Kit contract (ADR 0060 P2.9/P2.10) — pure, unit-tested.
 *
 * One module owns the answer round-trip: the question message's buttons carry
 * {tool_call_id, route, …} so `parseInteractivity` can turn a Slack
 * interactivity payload back into a `SourceAnswer` + the thread route, with no
 * dependence on Slack's `payload.message` shape. A multi/multi-question ask
 * opens a modal (`buildAnswerModal`); a single single-select asks inline.
 */

import { expect, test, describe } from "bun:test";
import {
  parseInteractivity,
  parseUserQuestion,
  parseQuestionAnswers,
  buildAnswerModal,
  buildQuestionBlocks,
  buildAnsweredBlocks,
  buildClosingBlocks,
  buildMessageBlocks,
  ACTION_ANSWER,
  ACTION_OPEN,
  CALLBACK_SUBMIT,
  MODAL_ACTION,
  type CompactQuestion,
} from "../integrations/slack-blocks.ts";

/** Find the first actions block and return its button elements. */
function buttons(blocks: unknown[]) {
  const actions = blocks.find((b) => (b as { type?: string }).type === "actions") as
    | { elements: { action_id: string; value: string }[] }
    | undefined;
  return actions?.elements ?? [];
}

const ROUTE = { team: "T1", channel: "C1", threadRoot: "100.0" };

const blockAction = (actionId: string, value: unknown) =>
  JSON.stringify({
    type: "block_actions",
    trigger_id: "trig123",
    actions: [{ action_id: actionId, value: JSON.stringify(value) }],
  });

describe("parseInteractivity()", () => {
  test("an inline option button → answer + route", () => {
    const out = parseInteractivity(
      blockAction(ACTION_ANSWER, { t: "tc1", q: "Pick one", a: "Option A", r: ROUTE }),
    );
    expect(out).toEqual({
      kind: "answer",
      answer: { toolCallId: "tc1", answers: { "Pick one": ["Option A"] } },
      route: ROUTE,
    });
  });

  test("an 'Answer…' button → open_modal carrying a built view", () => {
    const qs: CompactQuestion[] = [
      { q: "Multi?", h: "Multi", m: true, o: ["X", "Y", "Z"] },
    ];
    const out = parseInteractivity(blockAction(ACTION_OPEN, { t: "tc2", qs, r: ROUTE }));
    expect(out.kind).toBe("open_modal");
    if (out.kind !== "open_modal") throw new Error("unreachable");
    expect(out.triggerId).toBe("trig123");
    expect(out.view.callback_id).toBe(CALLBACK_SUBMIT);
    // The route + question texts ride private_metadata so the later
    // view_submission (which has no channel) can route + key answers.
    expect(JSON.parse(out.view.private_metadata!)).toEqual({
      t: "tc2",
      qs: ["Multi?"],
      r: ROUTE,
    });
    // A multi-select question renders checkboxes.
    expect(out.view.blocks).toHaveLength(1);
  });

  test("a single-select modal submission → answer from selected_option", () => {
    const payload = JSON.stringify({
      type: "view_submission",
      view: {
        callback_id: CALLBACK_SUBMIT,
        private_metadata: JSON.stringify({ t: "tc3", qs: ["Which?"], r: ROUTE }),
        state: { values: { q0: { [MODAL_ACTION]: { selected_option: { value: "B" } } } } },
      },
    });
    expect(parseInteractivity(payload)).toEqual({
      kind: "answer",
      answer: { toolCallId: "tc3", answers: { "Which?": ["B"] } },
      route: ROUTE,
    });
  });

  test("a multi-select modal submission → answer from selected_options", () => {
    const payload = JSON.stringify({
      type: "view_submission",
      view: {
        callback_id: CALLBACK_SUBMIT,
        private_metadata: JSON.stringify({ t: "tc4", qs: ["Pick many"], r: ROUTE }),
        state: {
          values: {
            q0: { [MODAL_ACTION]: { selected_options: [{ value: "X" }, { value: "Z" }] } },
          },
        },
      },
    });
    expect(parseInteractivity(payload)).toEqual({
      kind: "answer",
      answer: { toolCallId: "tc4", answers: { "Pick many": ["X", "Z"] } },
      route: ROUTE,
    });
  });

  test("an unrelated action_id → ignore", () => {
    expect(parseInteractivity(blockAction("some_other_button", { x: 1 }))).toEqual({ kind: "ignore" });
  });

  test("a view_submission with a foreign callback_id → ignore", () => {
    const payload = JSON.stringify({
      type: "view_submission",
      view: { callback_id: "not_ours", private_metadata: "{}", state: { values: {} } },
    });
    expect(parseInteractivity(payload)).toEqual({ kind: "ignore" });
  });

  test("malformed JSON → ignore (never throws)", () => {
    expect(parseInteractivity("not json")).toEqual({ kind: "ignore" });
    expect(parseInteractivity(blockAction(ACTION_ANSWER, "{bad"))).toEqual({ kind: "ignore" });
  });
});

describe("parseUserQuestion()", () => {
  test("maps a user_question payload to tool_call_id + questions", () => {
    const payload = JSON.stringify({
      run_id: "r",
      tool_call_id: "tc",
      questions: [
        { question: "Pick one", header: "Pick", multiSelect: false, options: [{ label: "A", description: "" }] },
      ],
    });
    expect(parseUserQuestion(payload)).toEqual({
      toolCallId: "tc",
      questions: [{ question: "Pick one", header: "Pick", multiSelect: false, options: ["A"] }],
    });
  });

  test("maps canonical questions nested in a tool_call_requested args_json", () => {
    const payload = JSON.stringify({
      run_id: "r",
      tool_call_id: "tc-generic",
      name: "ask_user_question",
      args_json: JSON.stringify({
        questions: [
          {
            question: "Pick one",
            header: "Pick",
            multiSelect: false,
            options: [{ label: "A", description: "first option" }],
          },
        ],
      }),
    });
    expect(parseUserQuestion(payload)).toEqual({
      toolCallId: "tc-generic",
      questions: [{ question: "Pick one", header: "Pick", multiSelect: false, options: ["A"] }],
    });
  });

  test("malformed payload → null (never throws)", () => {
    expect(parseUserQuestion("not json")).toBeNull();
    expect(parseUserQuestion(JSON.stringify({ tool_call_id: "tc" }))).toBeNull();
  });
});

describe("parseQuestionAnswers()", () => {
  test("reads legacy question_answered answers", () => {
    expect(
      parseQuestionAnswers(JSON.stringify({ tool_call_id: "tc", answers: { "Ship?": ["Yes"] } })),
    ).toEqual({ "Ship?": ["Yes"] });
  });

  test("reads canonical answers nested in tool_result_submitted result_json", () => {
    expect(
      parseQuestionAnswers(
        JSON.stringify({
          tool_call_id: "tc-generic",
          result_json: JSON.stringify({ "Ship?": ["Yes"] }),
        }),
      ),
    ).toEqual({ "Ship?": ["Yes"] });
  });
});

describe("buildQuestionBlocks()", () => {
  const parsed = (multiSelect: boolean, n = 1) => ({
    toolCallId: "tc",
    questions: Array.from({ length: n }, (_, i) => ({
      question: `Q${i}`,
      header: `H${i}`,
      multiSelect,
      options: ["yes", "no"],
    })),
  });

  test("a single single-select question → inline option buttons carrying the route", () => {
    const blocks = buildQuestionBlocks(ROUTE, parsed(false));
    const els = buttons(blocks);
    // Each option button gets a UNIQUE action_id (`auq_answer:<i>`) — Slack
    // rejects a message with two elements sharing an action_id (`invalid_blocks`).
    expect(els.map((e) => e.action_id)).toEqual([`${ACTION_ANSWER}:0`, `${ACTION_ANSWER}:1`]);
    expect(JSON.parse(els[0].value)).toEqual({ t: "tc", q: "Q0", a: "yes", r: ROUTE });
  });

  test("inline option buttons round-trip: each unique action_id still parses to an answer", () => {
    const els = buttons(buildQuestionBlocks(ROUTE, parsed(false)));
    const ids = els.map((e) => e.action_id);
    expect(new Set(ids).size).toBe(ids.length); // all unique within the message
    for (const el of els) {
      const out = parseInteractivity(
        JSON.stringify({
          type: "block_actions",
          trigger_id: "trig",
          actions: [{ action_id: el.action_id, value: el.value }],
        }),
      );
      expect(out.kind).toBe("answer");
    }
  });

  test("a multi-select question → a single 'Answer…' (open modal) button", () => {
    const els = buttons(buildQuestionBlocks(ROUTE, parsed(true)));
    expect(els).toHaveLength(1);
    expect(els[0].action_id).toBe(ACTION_OPEN);
    const v = JSON.parse(els[0].value);
    expect(v.t).toBe("tc");
    expect(v.r).toEqual(ROUTE);
    expect(v.qs[0]).toEqual({ q: "Q0", h: "H0", m: true, o: ["yes", "no"] });
  });

  test("more than one question → the modal path even when single-select", () => {
    const els = buttons(buildQuestionBlocks(ROUTE, parsed(false, 2)));
    expect(els).toHaveLength(1);
    expect(els[0].action_id).toBe(ACTION_OPEN);
  });
});

describe("buildAnsweredBlocks()", () => {
  test("renders the resolved question → selected labels", () => {
    const blocks = buildAnsweredBlocks({ "Pick one": ["A"], Colors: ["red", "blue"] });
    const text = JSON.stringify(blocks);
    expect(text).toContain("Pick one");
    expect(text).toContain("A");
    expect(text).toContain("red, blue");
  });
});

describe("buildClosingBlocks()", () => {
  const session = { id: "s1", webUrl: "https://engrams.dev/sessions/s1" };

  test("includes the last assistant message, asset links, and the session link", () => {
    const blocks = buildClosingBlocks(session, {
      lastMessage: "All done — shipped it.",
      assets: [{ label: "PR #1: Fix", url: "https://gh/pr/1" }, { label: "screenshot.png" }],
    });
    const text = JSON.stringify(blocks);
    expect(text).toContain("All done — shipped it.");
    expect(text).toContain("https://gh/pr/1");
    expect(text).toContain("PR #1: Fix");
    expect(text).toContain("screenshot.png");
    expect(text).toContain("https://engrams.dev/sessions/s1");
  });

  test("still renders a done line + session link with no message and no assets", () => {
    const blocks = buildClosingBlocks(session, { lastMessage: null, assets: [] });
    expect(JSON.stringify(blocks)).toContain("https://engrams.dev/sessions/s1");
    expect(blocks.length).toBeGreaterThan(0);
  });
});

describe("buildMessageBlocks()", () => {
  const sectionTexts = (blocks: unknown[]) =>
    blocks
      .filter((b) => (b as { type?: string }).type === "section")
      .map((b) => (b as { text: { text: string } }).text.text);

  test("short text → a single mrkdwn section carrying it verbatim", () => {
    const blocks = buildMessageBlocks("hello from the agent");
    expect(sectionTexts(blocks)).toEqual(["hello from the agent"]);
  });

  test("text longer than Slack's 3000-char section cap splits across sections", () => {
    const long = "x".repeat(7000);
    const blocks = buildMessageBlocks(long);
    const texts = sectionTexts(blocks);
    expect(texts.length).toBeGreaterThan(1);
    for (const t of texts) expect(t.length).toBeLessThanOrEqual(3000);
    expect(texts.join("")).toBe(long); // no content lost
  });

  test("empty text → no blocks", () => {
    expect(buildMessageBlocks("")).toEqual([]);
  });
});

describe("buildAnswerModal()", () => {
  test("renders radio_buttons for single-select and checkboxes for multi-select", () => {
    const qs: CompactQuestion[] = [
      { q: "One?", h: "One", m: false, o: ["a", "b"] },
      { q: "Many?", h: "Many", m: true, o: ["c", "d"] },
    ];
    const view = buildAnswerModal("tc", qs, ROUTE);
    expect(view.type).toBe("modal");
    expect(view.callback_id).toBe(CALLBACK_SUBMIT);
    const els = view.blocks.map((b) => (b as { element?: { type: string } }).element?.type);
    expect(els).toEqual(["radio_buttons", "checkboxes"]);
    // Block ids are positional (q0, q1) so the submission maps back to qs[i].
    expect(view.blocks.map((b) => (b as { block_id?: string }).block_id)).toEqual(["q0", "q1"]);
  });
});
