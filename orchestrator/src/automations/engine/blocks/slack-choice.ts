/** The slack_choice block: ask the person in a Slack thread to pick one
 * option, and wait for the click.
 *
 * The data half posts a card (one button per option) through the relay's
 * Slack policy, with this run's id in every button's route, so the
 * interactivity endpoint delivers the click to this run as a `slack_answer`
 * signal. The wait half consumes the answer whose id is this block's.
 * Nothing time-based fails the run: at the deadline the block records
 * `outcome: "deadline"` and the graph takes its own fallback. The card is
 * the graph's to close (an `update_message` on `message_ts`), so the thread
 * shows what was decided either way.
 *
 * Uses: smart profile routing when the model is not confident, an approval
 * ("deploy now?") in a thread, any one-of-N a person must decide.
 */

import { z } from "zod";

import { MAX_WAIT_DEADLINE_S } from "../definition.ts";
import type { RunContext } from "../context.ts";
import { CHOICE_ID_PREFIX, slackRelayPolicy, SLACK_ANSWER_SIGNAL } from "./relay.ts";
import { registerBlock } from "./registry.ts";

export const SLACK_CHOICE_TYPE = "slack_choice";

/** Slack allows 25 buttons in one actions block; a choice past ten is a
 * dropdown's job, not a card's. */
export const MAX_CHOICE_BUTTONS = 10;
/** Every button's value carries the question, its label, the answer id
 * and the thread route, and Slack caps a button value at 2,000
 * characters. These caps keep the worst case near 1,000. A label is also
 * the button's text, which Slack shows up to 75 characters of. */
export const SLACK_CHOICE_QUESTION_MAX = 500;
export const SLACK_CHOICE_LABEL_MAX = 75;

export const slackChoiceConfigSchema = z.object({
  provider: z.enum(["slack"]).default("slack"),
  team: z.string().min(1),
  channel: z.string().min(1),
  threadTs: z.string().min(1),
  /** The question on the card. */
  question: z.string().min(1).max(SLACK_CHOICE_QUESTION_MAX),
  /** value = what the block outputs; label = the button text (default:
   * value). From a `$ref` (a code block shaping the candidates) or inline. */
  options: z
    .array(
      z.object({
        value: z.string().min(1),
        label: z.string().min(1).max(SLACK_CHOICE_LABEL_MAX).optional(),
      }),
    )
    .min(1)
    .max(MAX_CHOICE_BUTTONS),
  deadlineSeconds: z.number().int().min(1).max(MAX_WAIT_DEADLINE_S).optional(),
  /** A deadline is a normal outcome, never a failed run (fixed). */
  onDeadline: z.literal("continue").default("continue"),
});
export type SlackChoiceConfig = z.infer<typeof slackChoiceConfigSchema>;

const DEFAULT_CHOICE_DEADLINE_S = 600;

/** This execution's answer id. Derived, never stored: the frame path is the
 * run-durable identity of the block (a choice inside a loop gets one id per
 * iteration), and the execute and wait halves derive the same value. */
function choiceId(ctx: RunContext): string {
  return `${CHOICE_ID_PREFIX}${ctx.runId}:${ctx.currentPath ?? ctx.currentBlockId ?? ""}`;
}

const labelOf = (option: { value: string; label?: string }) => option.label ?? option.value;

export function registerSlackChoiceBlock(): void {
  registerBlock<SlackChoiceConfig>({
    type: SLACK_CHOICE_TYPE,
    outputs: ["outcome", "value", "label", "message_ts"],
    configSchema: slackChoiceConfigSchema,
    async execute(config, ctx) {
      const labels = config.options.map(labelOf);
      if (labels.some((label) => label.length > SLACK_CHOICE_LABEL_MAX)) {
        return {
          kind: "error",
          code: "slack_choice_config_invalid",
          message: `an option label (or a value without a label) is longer than ${SLACK_CHOICE_LABEL_MAX} characters`,
          retryable: false,
        };
      }
      if (new Set(labels).size !== labels.length) {
        return {
          kind: "error",
          code: "slack_choice_config_invalid",
          message: "two options share a label; the answer could not tell them apart",
          retryable: false,
        };
      }
      if (ctx.dryRun) {
        return {
          kind: "ok",
          outputs: { dry_run: true, would_ask: { question: config.question, options: labels } },
        };
      }
      const ts = await slackRelayPolicy(ctx.runId).onChoice(
        {
          team: config.team,
          channel: config.channel,
          threadRoot: config.threadTs,
          user: "",
          ts: config.threadTs,
          eventId: "",
        },
        { id: choiceId(ctx), question: config.question, options: labels },
      );
      return { kind: "ok", outputs: { message_ts: ts } };
    },
    wait: {
      deadlineSeconds: (config) => config.deadlineSeconds ?? DEFAULT_CHOICE_DEADLINE_S,
      matches(msg, config, ctx) {
        if (msg.kind !== "signal" || msg.name !== SLACK_ANSWER_SIGNAL) return null;
        if (msg.payload?.["toolCallId"] !== choiceId(ctx)) return null;
        const answers = msg.payload["answers"];
        const picked =
          typeof answers === "object" && answers !== null && !Array.isArray(answers)
            ? (answers as Record<string, unknown>)[config.question]
            : undefined;
        const label = Array.isArray(picked) && typeof picked[0] === "string" ? picked[0] : undefined;
        const option = config.options.find((o) => labelOf(o) === label);
        // A malformed answer for our id is dropped; the wait continues.
        if (!option) return "ignore";
        return { outcome: "answered", value: option.value, label: labelOf(option) };
      },
      onDeadline: () => ({ outcome: "deadline" }),
      dryRunOutcome: () => ({ outcome: "deadline" }),
    },
  });
}
