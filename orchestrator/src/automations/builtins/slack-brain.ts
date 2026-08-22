/** The Slack thread-brain built-in automation (ADR 0119 D7, phase 4.6).
 *
 * ADR 0060's per-thread DBOS workflow (`slack-thread.ts`), expressed as data
 * on the engine. It is the conversation-shaped built-in: one run per Slack
 * thread, kept alive across turns by `join` concurrency (a later message in
 * the thread is delivered into the active run's mailbox instead of starting
 * a new run) and a loop of wait_event → send_prompt until the thread goes
 * quiet. The session is KEPT when the run ends — a thread's session lives on
 * for a human to pick up.
 *
 * The relay (`system.slack_thread_relay`, phase 4.5) is the product logic
 * that is not a generic primitive: it streams agent messages into the thread
 * as coalesced bubbles, round-trips AskUserQuestion as Block Kit, sets the
 * ⏳/✅ reactions, and answers arrive as the `slack_answer` signal it consumes
 * itself. Everything else is a visible generic block.
 *
 * The first block is a Code block for the same reason as the review
 * built-in: admission reads a MAP input keyed by a dynamic payload value
 * (`inputs.channels[event.channel]`), and the same block derives the facts
 * every later block templates from (`steps.facts.value.*`).
 *
 * Divergences from the legacy loop, deliberate for v1:
 *   - the initial/turn prompt is the triggering message's text with the bot
 *     mention stripped, NOT the legacy `<thread context>` fold of the whole
 *     thread fetched from the Slack API (that fold needs an I/O block; a
 *     follow-up can add `system.slack_thread_context`);
 *   - the LLM profile picker + the "which profile?" dropdown are retired:
 *     the channel → profile map input decides, with `default_profile` as the
 *     fallback.
 */

import {
  MAX_LOOP_ITERATIONS,
  MAX_WAIT_DEADLINE_S,
  type AutomationDefinition,
  type BlockDef,
} from "../engine/definition.ts";
import type { BuiltinAutomation } from "../engine/builtins.ts";
import { DEFAULT_CONNECTION_PLACEHOLDER } from "./pr-review.ts";

export const SLACK_BRAIN_BUILTIN_KEY = "slack_brain";

/** Bump on any graph or inputs-schema change. */
export const SLACK_BRAIN_DEFINITION_VERSION = 1;

export const SLACK_BRAIN_DEFAULT_IDLE_TIMEOUT_S = 3600;
export const SLACK_BRAIN_DEFAULT_MAX_TURNS = 50;
/** One turn's harness run. */
const TURN_DEADLINE_S = 3600;
/** Generous run ceiling; the per-wait idle timeout is the real end-of-thread
 * signal (settings cannot reference inputs). */
const RUN_DEADLINE_S = 48 * 3600;

const F = "steps.facts.value";

/** Admission + fact derivation. Returns null to reject; else the facts. Pure,
 * runs in the QuickJS cage. */
export const SLACK_FACTS_SOURCE = `
export default ({ event, inputs, trigger }) => {
  const raw = event.raw ?? {};
  const ev = raw.event ?? {};
  const key = trigger.event ?? "";
  const channel = String(ev.channel ?? "");
  if (!channel) return null;
  // Bots (including ourselves) never open or continue a brain thread.
  if (ev.bot_id || ev.subtype === "bot_message") return null;
  if (key === "message") {
    // Only thread replies: a top-level channel message is not ours.
    if (!ev.thread_ts || ev.thread_ts === ev.ts) return null;
    if (ev.subtype) return null;
  } else if (key !== "app_mention") {
    return null;
  }
  const channels = inputs.channels ?? {};
  const profile = channels[channel] ?? inputs.default_profile ?? "";
  if (!profile) return null;
  const text = String(ev.text ?? "")
    .replace(/<@[^>]+>/g, " ")
    .replace(/[^\\S\\n]+/g, " ")
    .trim();
  const str = (v) => (v === undefined || v === null ? "" : String(v));
  return {
    admit: true,
    profile_id: String(profile),
    team: str(raw.team_id),
    channel,
    thread_ts: str(ev.thread_ts || ev.ts),
    mention_ts: str(ev.ts),
    text,
    title: text.slice(0, 80) || "Slack thread",
    user_id: str(ev.user),
    event_id: str(raw.event_id),
  };
};
`.trim();

/** The joined follow-up message, as wait_event leaves it in steps.next.event.
 * `has_text` is the turn gate: a bare `@bot` (nothing left once the mention
 * is stripped) is not a prompt — send_prompt refuses an empty prompt, and
 * that refusal would fail the whole run. The bot guard is repeated here
 * (the `next` wait already refuses bot-authored events) so the gate stays
 * correct even if the wait's conditions are ever tuned away: the relay's
 * own bubbles arrive as `app_mention`/`message` events with a `bot_id`,
 * and answering them would make the brain talk to itself. */
const NEXT_TEXT_SOURCE = `
export default ({ steps }) => {
  const ev = steps.next?.event?.event ?? {};
  const fromBot = Boolean(ev.bot_id) || ev.subtype === "bot_message";
  const text = String(ev.text ?? "")
    .replace(/<@[^>]+>/g, " ")
    .replace(/[^\\S\\n]+/g, " ")
    .trim();
  return {
    text,
    has_text: !fromBot && text.length > 0,
    mention_ts: String(ev.ts ?? ""),
    user_id: String(ev.user ?? ""),
  };
};
`.trim();

const admit: BlockDef[] = [
  {
    id: "facts",
    type: "code",
    config: { source: SLACK_FACTS_SOURCE, mode: "value" },
  },
  {
    id: "admit",
    type: "filter",
    config: {
      conditions: {
        mode: "all",
        conditions: [{ path: `${F}.admit`, op: "is_true" }],
      },
    },
  },
  // The identity gate (legacy resolveUser): the author must be an engrams
  // user, and the session runs AS that user. Unlinked → "log in first" in
  // the thread and the run ends `filtered` before any session exists.
  {
    id: "identity",
    type: "system.slack_resolve_user",
    config: {
      userId: `\${{ ${F}.user_id }}`,
      team: `\${{ ${F}.team }}`,
      channel: `\${{ ${F}.channel }}`,
      threadTs: `\${{ ${F}.thread_ts }}`,
      mentionTs: `\${{ ${F}.mention_ts }}`,
      eventId: `\${{ ${F}.event_id }}`,
    },
  },
];

const session: BlockDef = {
  id: "session",
  type: "create_session",
  tunable: ["promptTemplate"],
  config: {
    profileId: `\${{ ${F}.profile_id }}`,
    promptTemplate: `\${{ ${F}.text }}`,
    titleTemplate: `\${{ ${F}.title }}`,
    role: "primary",
    // The session is the asking user's: their credentials and attribution
    // (never the programmatic org credential a stranger could borrow).
    ownerUserId: "${{ steps.identity.user_id }}",
    // D8 + the thread model: the session outlives the run.
    keepOnFinish: true,
  },
};

const relay: BlockDef = {
  id: "relay",
  type: "system.slack_thread_relay",
  config: {
    session: { blockId: "session" },
    team: `\${{ ${F}.team }}`,
    channel: `\${{ ${F}.channel }}`,
    threadTs: `\${{ ${F}.thread_ts }}`,
    mentionTs: `\${{ ${F}.mention_ts }}`,
    userId: `\${{ ${F}.user_id }}`,
    eventId: `\${{ ${F}.event_id }}`,
  },
};

/** The first turn: the mention's text is the session's initial prompt, so
 * the loop only waits for that run to end before listening for more. */
const firstTurn: BlockDef = {
  id: "first_turn",
  type: "wait_session",
  tunable: ["deadlineSeconds"],
  config: { session: { blockId: "session" }, until: "idle", deadlineSeconds: TURN_DEADLINE_S },
};

const conversation: BlockDef = {
  id: "thread",
  type: "loop",
  config: {
    maxIterations: { $ref: "inputs.max_turns" },
    // The thread went quiet: the last wait_event hit its idle deadline.
    until: {
      mode: "all",
      conditions: [{ path: "steps.next.outcome", op: "equals", value: "deadline" }],
    },
  },
  body: [
    {
      id: "next",
      type: "wait_event",
      config: {
        eventKeys: ["message", "app_mention"],
        // Bots never continue a thread either (the opening admission has the
        // same rule). The ingress route drops bot/subtype `message`s, but an
        // `app_mention` authored by a bot — including our own bubbles when
        // they quote the handle — reaches the mailbox, so the wait refuses
        // it here and keeps listening.
        conditions: {
          mode: "all",
          conditions: [
            { path: "event.event.bot_id", op: "is_empty" },
            { path: "event.event.subtype", op: "is_empty" },
          ],
        },
        // Typed-through at run time: the wait half reads the execute step's
        // resolved config (see BlockOutcome.resolvedConfig).
        deadlineSeconds: { $ref: "inputs.idle_timeout" },
        // The thread going quiet is how a conversation ENDS, not a failure:
        // the loop's `until` reads outcome=deadline and exits; the run
        // completes and the recap posts ✅.
        onDeadline: "continue",
      },
    },
    {
      id: "turn_text",
      type: "code",
      config: { source: NEXT_TEXT_SOURCE, mode: "value" },
    },
    // The turn gate: wait_event leaves outcome=deadline and no event on a
    // quiet thread, and a bare mention leaves no text; send_prompt must not
    // fire on either (an empty prompt is a render failure that would end the
    // run), so the branch requires an event WITH text.
    {
      id: "has_turn",
      type: "branch",
      config: {
        conditions: {
          mode: "all",
          conditions: [
            { path: "steps.next.outcome", op: "equals", value: "event" },
            { path: "steps.turn_text.value.has_text", op: "is_true" },
          ],
        },
      },
      then: [
        // Re-point the relay at the accepted follow-up BEFORE its prompt is
        // sent: ⏳ lands on the reply, its responses open a fresh bubble and
        // ✅ seals on the reply — the legacy per-turn mention ownership.
        {
          id: "repoint",
          type: "system.slack_thread_relay",
          config: {
            session: { blockId: "session" },
            team: `\${{ ${F}.team }}`,
            channel: `\${{ ${F}.channel }}`,
            threadTs: `\${{ ${F}.thread_ts }}`,
            mentionTs: "${{ steps.turn_text.value.mention_ts }}",
            userId: "${{ steps.turn_text.value.user_id }}",
            eventId: "${{ steps.next.delivery_key }}",
          },
        },
        {
          id: "turn",
          type: "send_prompt",
          tunable: ["promptTemplate", "deadlineSeconds"],
          config: {
            session: { blockId: "session" },
            promptTemplate: "${{ steps.turn_text.value.text }}",
            waitFor: { kind: "run_end" },
            deadlineSeconds: TURN_DEADLINE_S,
          },
        },
      ],
      else: [],
    },
  ],
};

export const SLACK_BRAIN_DEFINITION: AutomationDefinition = {
  engine: 1,
  trigger: {
    kind: "integration",
    provider: "slack",
    connectionId: DEFAULT_CONNECTION_PLACEHOLDER,
    eventKeys: ["app_mention", "message"],
    // The channels map's keys ARE the scope: a channel not in the map never
    // matches, so nothing reaches the run for channels an org never flagged.
    scope: { fromInput: "channels" },
  },
  blocks: [...admit, session, relay, firstTurn, conversation],
  inputsSchema: [
    {
      key: "channels",
      label: "Channels",
      type: "map",
      keyNoun: "channel",
      help: "Channels the brain answers in, each mapped to the session profile it uses.",
      required: true,
      default: {},
      valueShape: { type: "string", label: "Profile id" },
    },
    {
      key: "default_profile",
      label: "Default profile",
      type: "string",
      help: "Profile for a flagged channel with no explicit mapping.",
      default: "",
    },
    {
      key: "idle_timeout",
      label: "Idle timeout (seconds)",
      type: "number",
      help: "How long a thread may go quiet before the run ends (the session is kept). 60 s to 24 h.",
      default: SLACK_BRAIN_DEFAULT_IDLE_TIMEOUT_S,
      // The wait_event that consumes this through `$ref` has a 24 h schema
      // ceiling; the bound here keeps a saved value always runnable (the
      // interpreter also clamps, as a second line).
      min: 60,
      max: MAX_WAIT_DEADLINE_S,
    },
    {
      key: "max_turns",
      label: "Max turns",
      type: "number",
      help: "Upper bound on follow-up messages one run answers.",
      default: SLACK_BRAIN_DEFAULT_MAX_TURNS,
      min: 1,
      max: MAX_LOOP_ITERATIONS,
    },
  ],
  settings: {
    concurrency: {
      // One run per thread. Rendered at admission from the raw event: a
      // thread reply carries thread_ts; the opening mention only has ts.
      keyTemplate:
        "${{ event.raw.team_id }}:${{ event.raw.event.channel }}:${{ event.raw.event.thread_ts | default: event.raw.event.ts }}",
      policy: "join",
    },
    runDeadlineSeconds: RUN_DEADLINE_S,
    endSessionsOnFinish: false,
    onFinalize: [
      {
        when: ["completed", "deadline", "failed", "halted", "superseded", "filtered"],
        block: {
          id: "recap",
          type: "system.slack_thread_recap",
          config: { status: "${{ run.status }}" },
        },
      },
    ],
  },
};

export const SLACK_BRAIN_BUILTIN: BuiltinAutomation = {
  key: SLACK_BRAIN_BUILTIN_KEY,
  name: "Slack thread brain",
  description:
    "Answers @-mentions in Slack threads with a session per thread, relaying the conversation both ways.",
  definitionVersion: SLACK_BRAIN_DEFINITION_VERSION,
  definition: SLACK_BRAIN_DEFINITION,
  async defaultInputs() {
    return {
      channels: {},
      default_profile: "",
      idle_timeout: SLACK_BRAIN_DEFAULT_IDLE_TIMEOUT_S,
      max_turns: SLACK_BRAIN_DEFAULT_MAX_TURNS,
    };
  },
};
