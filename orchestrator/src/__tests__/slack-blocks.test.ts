/**
 * Slack Block Kit contract (ADR 0059 P2.9/P2.10) — pure, unit-tested.
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
  buildAnswerModal,
  ACTION_ANSWER,
  ACTION_OPEN,
  CALLBACK_SUBMIT,
  MODAL_ACTION,
  type CompactQuestion,
} from "../integrations/slack-blocks.ts";

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
