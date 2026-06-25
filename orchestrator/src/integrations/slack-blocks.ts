/**
 * Slack Block Kit contract for AskUserQuestion (ADR 0059 P2.9/P2.10) — pure.
 *
 * One module owns BOTH halves of the answer round-trip so they cannot drift:
 *   - the question message's interactive elements (built in slack-policy.ts via
 *     `buildQuestionBlocks`), and
 *   - `parseInteractivity`, which turns the Slack interactivity payload back
 *     into a `SourceAnswer` + the thread route.
 *
 * The thread route ({team, channel, threadRoot}) is carried IN the button value
 * (and copied into the modal's `private_metadata`), so routing never depends on
 * Slack's `payload.message`/`payload.channel` shape — a view_submission has no
 * channel at all. Answers are option-only (AskUserQuestion has no free-text), so
 * a single single-select question is asked inline (one-click) and anything
 * richer (multi-select, or >1 question) opens a modal that submits atomically.
 */

import type { ModalView, KnownBlock } from "@slack/types";

import type { SourceAnswer } from "../workflows/thread-inbox.ts";
import type {
  AssetSummary,
  ClosingSummary,
  StartedSession,
} from "../workflows/communication-policy.ts";

/** Inline single-select option button (one per option). The concrete buttons
 *  carry a per-option index suffix (`auq_answer:<i>`) for Slack action_id
 *  uniqueness; `isAnswerAction` matches both the bare and suffixed forms. */
export const ACTION_ANSWER = "auq_answer";

/** True for any inline answer-option button action_id — the bare `auq_answer`
 *  or the per-option `auq_answer:<i>`. Routing identity is in the button value,
 *  not the action_id, so the index is irrelevant to classification. */
function isAnswerAction(id: string | undefined): boolean {
  return id === ACTION_ANSWER || (id?.startsWith(`${ACTION_ANSWER}:`) ?? false);
}
/** "Answer…" button that opens the modal for a multi/multi-question ask. */
export const ACTION_OPEN = "auq_open";
/** The answer modal's `callback_id`. */
export const CALLBACK_SUBMIT = "auq_submit";
/** The input element's `action_id` inside each modal question block. */
export const MODAL_ACTION = "opt";

/** Where in the thread an answer must be delivered. Mirrors the routing subset
 *  of a `SourceMention`; carried in every interactive element's value so the
 *  events-handler-independent interactivity endpoint can compute the workflow. */
export interface ThreadRoute {
  team: string;
  channel: string;
  threadRoot: string;
}

/** A question, compacted for carriage in a Slack value/private_metadata (option
 *  labels only — descriptions are dropped to stay within Slack's size caps). */
export interface CompactQuestion {
  /** Full question text — ALSO the key in the answers map. */
  q: string;
  /** Short header (the input block's label). */
  h: string;
  /** true → checkboxes (multi); false → radio buttons (single). */
  m: boolean;
  /** Option labels. */
  o: string[];
}

/** The classified interactivity payload the route acts on. */
export type InteractivityAction =
  | { kind: "answer"; answer: SourceAnswer; route: ThreadRoute }
  | { kind: "open_modal"; triggerId: string; view: ModalView }
  | { kind: "ignore" };

/** The carried value of an inline single-select option button. */
interface AnswerValue {
  t: string; // tool_call_id
  q: string; // question text
  a: string; // chosen option label
  r: ThreadRoute;
}
/** The carried value of an "Answer…" (open-modal) button. */
interface OpenValue {
  t: string;
  qs: CompactQuestion[];
  r: ThreadRoute;
}
/** What rides the modal's private_metadata (no options needed on submit — the
 *  selected option values ARE the labels). */
interface ModalMeta {
  t: string;
  qs: string[]; // question texts, positionally aligned to blocks q0..qn
  r: ThreadRoute;
}

/**
 * Classify a Slack interactivity payload (the JSON from the form `payload`
 * field) into the effect the route performs. Pure; never throws.
 */
export function parseInteractivity(payloadJson: string): InteractivityAction {
  let p: {
    type?: string;
    trigger_id?: string;
    actions?: { action_id?: string; value?: string }[];
    view?: {
      callback_id?: string;
      private_metadata?: string;
      state?: { values?: Record<string, Record<string, ViewStateValue>> };
    };
  };
  try {
    p = JSON.parse(payloadJson);
  } catch {
    return { kind: "ignore" };
  }

  if (p.type === "block_actions") {
    const action = p.actions?.find(
      (a) => isAnswerAction(a.action_id) || a.action_id === ACTION_OPEN,
    );
    if (!action?.value) return { kind: "ignore" };
    try {
      if (isAnswerAction(action.action_id)) {
        const v = JSON.parse(action.value) as AnswerValue;
        if (typeof v.t !== "string" || typeof v.q !== "string" || typeof v.a !== "string" || !v.r) {
          return { kind: "ignore" };
        }
        return {
          kind: "answer",
          answer: { toolCallId: v.t, answers: { [v.q]: [v.a] } },
          route: v.r,
        };
      }
      const v = JSON.parse(action.value) as OpenValue;
      if (typeof v.t !== "string" || !Array.isArray(v.qs) || !v.r) return { kind: "ignore" };
      return {
        kind: "open_modal",
        triggerId: p.trigger_id ?? "",
        view: buildAnswerModal(v.t, v.qs, v.r),
      };
    } catch {
      return { kind: "ignore" };
    }
  }

  if (p.type === "view_submission" && p.view?.callback_id === CALLBACK_SUBMIT) {
    try {
      const meta = JSON.parse(p.view.private_metadata ?? "") as ModalMeta;
      if (typeof meta.t !== "string" || !Array.isArray(meta.qs) || !meta.r) {
        return { kind: "ignore" };
      }
      const values = p.view.state?.values ?? {};
      const answers: Record<string, string[]> = {};
      meta.qs.forEach((question, i) => {
        const state = values[`q${i}`]?.[MODAL_ACTION];
        answers[question] = selectedLabels(state);
      });
      return { kind: "answer", answer: { toolCallId: meta.t, answers }, route: meta.r };
    } catch {
      return { kind: "ignore" };
    }
  }

  return { kind: "ignore" };
}

/** A Slack view-state element (the subset we read). */
interface ViewStateValue {
  selected_option?: { value?: string } | null;
  selected_options?: { value?: string }[];
}

/** The labels a question's input element selected (1 for radio, N for checkboxes). */
function selectedLabels(state: ViewStateValue | undefined): string[] {
  if (!state) return [];
  if (state.selected_options) {
    return state.selected_options.map((o) => o.value ?? "").filter(Boolean);
  }
  const single = state.selected_option?.value;
  return single ? [single] : [];
}

/**
 * Build the answer modal: one input block per question (radio buttons for
 * single-select, checkboxes for multi), submitting atomically. Block ids are
 * positional (`q0`…`qn`) so the submission maps back to the question texts the
 * `private_metadata` carries; option `value`s ARE the labels (round-tripped
 * straight into the answers map).
 */
export function buildAnswerModal(
  toolCallId: string,
  questions: CompactQuestion[],
  route: ThreadRoute,
): ModalView {
  const meta: ModalMeta = { t: toolCallId, qs: questions.map((q) => q.q), r: route };
  const blocks: KnownBlock[] = questions.map((q, i) => ({
    type: "input",
    block_id: `q${i}`,
    label: { type: "plain_text", text: truncate(q.h || q.q, 150) },
    element: {
      type: q.m ? "checkboxes" : "radio_buttons",
      action_id: MODAL_ACTION,
      options: q.o.map((label) => ({
        text: { type: "plain_text", text: truncate(label, 75) },
        value: truncate(label, 75),
      })),
    },
  }));
  return {
    type: "modal",
    callback_id: CALLBACK_SUBMIT,
    private_metadata: JSON.stringify(meta),
    title: { type: "plain_text", text: "Answer" },
    submit: { type: "plain_text", text: "Submit" },
    close: { type: "plain_text", text: "Cancel" },
    blocks,
  };
}

const truncate = (s: string, n: number): string => (s.length > n ? s.slice(0, n - 1) + "…" : s);

// ── Outbound rendering: the question message, its resolution, the closing
//    summary. The button values produced here are exactly what
//    `parseInteractivity` reads back, which is why both live in this module.

/** One AskUserQuestion question, parsed from the `user_question` payload. */
export interface ParsedQuestion {
  question: string;
  header: string;
  multiSelect: boolean;
  options: string[];
}
export interface ParsedUserQuestion {
  toolCallId: string;
  questions: ParsedQuestion[];
}

/** The slice of a `user_question` payload we read (option descriptions dropped). */
interface RawUserQuestion {
  tool_call_id?: unknown;
  questions?: {
    question?: unknown;
    header?: unknown;
    multiSelect?: unknown;
    options?: { label?: unknown }[];
  }[];
}

/** Parse a `user_question` event payload into the rendering shape, or null if
 *  it lacks a tool_call_id or any question. Pure; never throws. */
export function parseUserQuestion(payloadJson: string): ParsedUserQuestion | null {
  let p: RawUserQuestion;
  try {
    p = JSON.parse(payloadJson) as RawUserQuestion;
  } catch {
    return null;
  }
  if (typeof p.tool_call_id !== "string" || !Array.isArray(p.questions) || p.questions.length === 0) {
    return null;
  }
  const questions: ParsedQuestion[] = p.questions.map((q) => ({
    question: typeof q.question === "string" ? q.question : "",
    header: typeof q.header === "string" ? q.header : "",
    multiSelect: q.multiSelect === true,
    options: Array.isArray(q.options)
      ? q.options.map((o) => (typeof o.label === "string" ? o.label : "")).filter(Boolean)
      : [],
  }));
  return { toolCallId: p.tool_call_id, questions };
}

const section = (text: string): KnownBlock => ({ type: "section", text: { type: "mrkdwn", text } });

/**
 * Build the posted question message. A single single-select question gets
 * inline one-click option buttons; anything richer (multi-select, or >1
 * question) gets one "Answer…" button that opens the atomic-submit modal. Every
 * interactive element carries the thread route so the answer routes back with
 * no dependence on Slack's payload shape.
 */
export function buildQuestionBlocks(route: ThreadRoute, parsed: ParsedUserQuestion): KnownBlock[] {
  const { toolCallId: t, questions } = parsed;
  const blocks: KnownBlock[] = questions.map((q) =>
    section(q.header ? `*${q.header}*\n${q.question}` : q.question),
  );

  const inline = questions.length === 1 && !questions[0].multiSelect;
  if (inline) {
    const q = questions[0];
    blocks.push({
      type: "actions",
      // Each button needs a UNIQUE action_id within the message — Slack rejects
      // a message with two elements sharing one (`invalid_blocks`). The chosen
      // option rides the button `value` (the `a` field), not the action_id, so
      // suffixing the index is purely for uniqueness; `parseInteractivity`
      // classifies any `auq_answer*` action as an answer.
      elements: q.options.map((label, i) => ({
        type: "button",
        action_id: `${ACTION_ANSWER}:${i}`,
        text: { type: "plain_text", text: truncate(label, 75) },
        value: JSON.stringify({ t, q: q.question, a: label, r: route }),
      })),
    });
  } else {
    const qs: CompactQuestion[] = questions.map((q) => ({
      q: q.question,
      h: q.header,
      m: q.multiSelect,
      o: q.options,
    }));
    blocks.push({
      type: "actions",
      elements: [
        {
          type: "button",
          action_id: ACTION_OPEN,
          text: { type: "plain_text", text: "Answer…" },
          value: JSON.stringify({ t, qs, r: route }),
        },
      ],
    });
  }
  return blocks;
}

/** The resolved question message (replaces the live one via chat.update). */
export function buildAnsweredBlocks(answers: Record<string, string[]>): KnownBlock[] {
  const lines = Object.entries(answers).map(
    ([question, labels]) => `✓ *${question}*: ${labels.join(", ")}`,
  );
  return [section(lines.join("\n") || "✓ Answered")];
}

/** A one-line render of a single asset for the live `onAsset` post, or null for
 *  a transient action that isn't worth a thread message. */
export function buildAssetLine(asset: AssetSummary): string {
  return asset.url ? `🔗 <${asset.url}|${asset.label}>` : `📎 ${asset.label}`;
}

/** The closing summary: the session's final assistant message, a recap of the
 *  durable assets it produced, and a link back to the session. */
export function buildClosingBlocks(session: StartedSession, summary: ClosingSummary): KnownBlock[] {
  const blocks: KnownBlock[] = [section("*✅ Session complete*")];
  if (summary.lastMessage) blocks.push(section(truncate(summary.lastMessage, 2900)));
  if (summary.assets.length) {
    const lines = summary.assets.map(
      (a) => `• ${a.url ? `<${a.url}|${a.label}>` : a.label}`,
    );
    blocks.push(section(lines.join("\n")));
  }
  blocks.push(section(`<${session.webUrl}|View session>`));
  return blocks;
}
