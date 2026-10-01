/** The Slack thread-brain built-in automation (ADR 0119 D7, phase 4.6; a
 * workstream per thread since 2026-09-29, ADR 0120).
 *
 * ADR 0060's per-thread DBOS workflow (`slack-thread.ts`), expressed as data
 * on the engine. It is the conversation-shaped built-in: one run per Slack
 * thread, kept alive across turns by `join` concurrency (a later message in
 * the thread is delivered into the active run's mailbox instead of starting
 * a new run) and a loop of wait_event → send_prompt until the thread goes
 * quiet. The session is KEPT when the run ends — a thread's session lives on
 * for a human to pick up.
 *
 * The relay (`relay_session`, phase 4.5) streams agent messages into the
 * thread as coalesced bubbles, round-trips AskUserQuestion as Block Kit,
 * sets the ⏳/✅ reactions, and answers arrive as the `slack_answer` signal
 * it consumes itself. Every block here is a catalog block: this built-in is
 * an example an ordinary user could have built, and Duplicate gives them an
 * editable copy.
 *
 * The first block is a Code block for the same reason as the review
 * built-in: admission reads a MAP input keyed by a dynamic payload value
 * (`inputs.channels[event.channel]`), and the same block derives the facts
 * every later block templates from (`steps.facts.value.*`).
 *
 * Turns are the legacy model: ONLY an explicit `@bot` mention in the thread
 * is a turn. A plain reply is not answered on its own; it is folded into the
 * next mention's prompt as `<thread context>` (every message since the
 * previous turn, bots skipped, mentions stripped), with the mention's text
 * as the directive underneath — the legacy `foldReplies`, as a `list_replies`
 * Slack action plus a Code block. An in-thread mention reaches the thread's
 * run by the workstream key (join), so the automation has one trigger and no
 * reply entrypoint.
 *
 * The thread outlives any one run (D8 + ADR 0120). A run ends when the
 * thread goes quiet (the idle wait's deadline), silently — no "Session
 * complete" post for a pause — and the workstream stays OPEN: the next
 * mention in the thread binds to it, starts a new run, and
 * `lookup_instance_session` finds the kept session so the conversation
 * continues in it (a fresh session only when none exists or it is gone).
 * Only an explicit end (halted, superseded) closes the workstream.
 *
 * One divergence from the legacy loop, deliberate: the LLM profile picker +
 * the "which profile?" dropdown are retired — the channel → profile map
 * input decides, with `default_profile` as the fallback.
 */

import {
  MAX_LOOP_ITERATIONS,
  MAX_WAIT_DEADLINE_S,
  type AutomationDefinition,
  type BlockDef,
} from "../engine/definition.ts";
import type { BuiltinAutomation } from "../engine/builtins.ts";
import { RELAY_CLOSE_TYPE } from "../engine/blocks/relay-close.ts";
import { LOOKUP_INSTANCE_SESSION_TYPE } from "../engine/blocks/instance-session.ts";
import { RELAY_SESSION_TYPE } from "../engine/blocks/relay.ts";
import { RESOLVE_USER_TYPE } from "../engine/blocks/resolve-user.ts";
import { NO_USER_MSG } from "../../integrations/slack-identity.ts";
import { DEFAULT_CONNECTION_PLACEHOLDER } from "./pr-review.ts";

export const SLACK_BRAIN_BUILTIN_KEY = "slack_brain";

/** Bump on any graph or inputs-schema change. */
export const SLACK_BRAIN_DEFINITION_VERSION = 9;

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
  // Bots (including ourselves) never open a brain thread.
  if (ev.bot_id || ev.subtype === "bot_message") return null;
  // Only a mention opens a thread. A reply reaches the thread's run through
  // the reply entrypoint (joined into this run's mailbox), never here.
  if (key !== "app_mention") return null;
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
    // The app's own bot user: the fold tells our posts from other bots' by it.
    bot_user_id: str((raw.authorizations ?? [])[0]?.user_id),
  };
};
`.trim();

/** The turn prompt: the legacy `foldReplies` in the QuickJS cage. `messages`
 * is the thread page the `list_replies` action read; `since` is exclusive
 * (the previous turn's mention — everything up to and including it was
 * already delivered); the message whose ts is `trigger` is the directive
 * and goes at the bottom, every other kept message is prior context in
 * `<thread context>`. Messages newer than the trigger are left for the NEXT
 * turn (they arrived after the ask; the next mention's fold carries them).
 * Bot-authored messages never feed the prompt (the agent must not read its
 * own posts) EXCEPT the thread root, which is the subject of the thread.
 * Mentions are stripped. When the page does not carry the trigger yet
 * (Slack lag), the event's own text is the directive.
 * `has_text` is the turn gate: a bare `@bot` is not a prompt — send_prompt
 * refuses an empty prompt, and that refusal would fail the whole run. */
function foldSource(args: {
  replies: string;
  trigger: string;
  since: string;
  eventText: string;
  eventUser: string;
  fromBot: string;
}): string {
  return `
export default ({ steps }) => {
  const facts = steps.facts?.value ?? {};
  const messages = ${args.replies} ?? [];
  const trigger = String(${args.trigger} ?? "");
  const root = String(facts.thread_ts ?? "");
  const num = (ts) => Number.parseFloat(String(ts ?? "")) || 0;
  // The newest reply of ours in the page: everything up to it was already
  // delivered to the session (as a directive or as context) by an earlier
  // turn. A bot post with no \`user\` counts as ours; a post by a different
  // bot user does not.
  const ownBot = String(facts.bot_user_id ?? "");
  let lastOwn = "";
  for (const m of messages) {
    const ts = String(m.ts ?? "");
    if (ts === root) continue;
    const isBot = Boolean(m.bot_id || m.subtype === "bot_message");
    const ours = isBot && (!ownBot || !m.user || m.user === ownBot);
    if (ours && num(ts) > num(lastOwn)) lastOwn = ts;
  }
  const since = ${args.since};
  const strip = (t) => String(t ?? "")
    .replace(/<@[^>]+>/g, " ")
    .replace(/[^\\S\\n]+/g, " ")
    .trim();
  const kept = [];
  for (const m of messages) {
    const ts = String(m.ts ?? "");
    if (since && num(ts) <= num(since)) continue;
    if (trigger && num(ts) > num(trigger)) continue;
    const isRoot = ts === root;
    if (!isRoot && (m.bot_id || m.subtype === "bot_message")) continue;
    const text = strip(m.text);
    if (text) kept.push({ ts, text });
  }
  const idx = kept.findIndex((k) => k.ts === trigger);
  const directive = idx >= 0 ? kept[idx].text : strip(${args.eventText});
  const context = kept.filter((_, i) => i !== idx).map((k) => k.text);
  const text = !directive
    ? ""
    : context.length
      ? "<thread context>\\n" + context.join("\\n") + "\\n</thread context>\\n\\n" + directive
      : directive;
  const fromBot = Boolean(${args.fromBot});
  return {
    text,
    has_text: !fromBot && text.length > 0,
    mention_ts: trigger,
    user_id: String(${args.eventUser} ?? ""),
  };
};
`.trim();
}

/** The opening turn: the whole thread so far (a mention in the middle of a
 * human conversation brings that conversation along), the mention as the
 * directive. When the thread's kept session is resumed, only what arrived
 * after our last reply is new to it — the earlier turns were its own. */
const OPENING_TEXT_SOURCE = foldSource({
  replies: "steps.replies.messages",
  trigger: "facts.mention_ts",
  since: "steps.previous?.found ? lastOwn : null",
  eventText: "facts.text",
  eventUser: "facts.user_id",
  fromBot: "false",
});

/** A follow-up turn: everything since the previous turn's mention (the last
 * re-point's mention — block outputs are keyed by id, so `steps.repoint` is
 * the previous iteration's — or the opening mention before any re-point),
 * the new mention as the directive. */
const NEXT_TEXT_SOURCE = foldSource({
  replies: "steps.thread_replies.messages",
  trigger: "steps.next?.event?.event?.ts",
  since: 'String(steps.repoint?.handler_state?.mention?.ts ?? facts.mention_ts ?? "")',
  eventText: "steps.next?.event?.event?.text",
  eventUser: "steps.next?.event?.event?.user",
  fromBot:
    'steps.next?.event?.event?.bot_id || steps.next?.event?.event?.subtype === "bot_message"',
});

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
    type: RESOLVE_USER_TYPE,
    config: { provider: "slack", externalUserId: `\${{ ${F}.user_id }}` },
  },
  {
    id: "unlinked",
    type: "branch",
    config: {
      conditions: {
        mode: "all",
        conditions: [{ path: "steps.identity.found", op: "is_false" }],
      },
    },
    then: [
      {
        id: "login_notice",
        type: "integration_action",
        tunable: ["params"],
        config: {
          provider: "slack",
          actionId: "post_message",
          params: {
            channel: `\${{ ${F}.channel }}`,
            threadTs: `\${{ ${F}.thread_ts }}`,
            text: NO_USER_MSG,
          },
        },
      },
    ],
    else: [],
  },
  {
    id: "linked",
    type: "filter",
    config: {
      conditions: {
        mode: "all",
        conditions: [{ path: "steps.identity.found", op: "is_true" }],
      },
    },
  },
];

/** The previous turn's mention ts: the fold's exclusive `since` and the
 * read's inclusive `oldest`. Block outputs are keyed by id, so
 * `steps.repoint` is the previous iteration's re-point; before any re-point
 * it is the opening mention. (`default:` tolerates the missing path; its
 * argument, the facts, is always present.) */
const PREVIOUS_MENTION_TS = `\${{ steps.repoint.handler_state.mention.ts | default: ${F}.mention_ts }}`;

/** The thread as Slack holds it, for the fold. `oldest` bounds the read to
 * the tail since the previous turn (inclusive — the fold drops the boundary
 * message itself); the opening read is unbounded because a mention in the
 * middle of a human thread brings the whole thread along. */
function listReplies(id: string, oldest?: string): BlockDef {
  return {
    id,
    type: "integration_action",
    config: {
      provider: "slack",
      actionId: "list_replies",
      params: {
        channel: `\${{ ${F}.channel }}`,
        threadTs: `\${{ ${F}.thread_ts }}`,
        ...(oldest !== undefined ? { oldest } : {}),
      },
    },
  };
}

/** The thread's kept session from an earlier run of this workstream, if any:
 * the thread went quiet, its run ended, the next mention is this run. Looked
 * up before the fold, which keeps a resumed session's earlier turns out of
 * its prompt. */
const previous: BlockDef = { id: "previous", type: LOOKUP_INSTANCE_SESSION_TYPE, config: {} };

const opening: BlockDef[] = [
  previous,
  listReplies("replies"),
  { id: "opening", type: "code", config: { source: OPENING_TEXT_SOURCE, mode: "value" } },
];

const session: BlockDef = {
  id: "session",
  type: "create_session",
  tunable: ["promptTemplate"],
  config: {
    profileId: `\${{ ${F}.profile_id }}`,
    promptTemplate: "${{ steps.opening.value.text }}",
    titleTemplate: `\${{ ${F}.title }}`,
    role: "primary",
    // The session is the asking user's: their credentials and attribution
    // (never the programmatic org credential a stranger could borrow).
    ownerUserId: "${{ steps.identity.user_id }}",
    // D8 + the thread model: the session outlives the run.
    keepOnFinish: true,
  },
};

/** The legacy `onStarted` message: the thread gets the session link as
 * soon as the session exists, before any agent output arrives. An ordinary
 * Slack action, so Duplicate lets a user reword or remove it. */
const started: BlockDef = {
  id: "started",
  type: "integration_action",
  tunable: ["params"],
  config: {
    provider: "slack",
    actionId: "post_message",
    params: {
      channel: `\${{ ${F}.channel }}`,
      threadTs: `\${{ ${F}.thread_ts }}`,
      text: "Started a session — ${{ steps.session.web_url }}",
    },
  },
};

/** The session this run talks to: the resumed one, else the one it created. */
const PICK_SOURCE = `
export default ({ steps }) => ({
  session_id: steps.previous?.found ? String(steps.previous.session_id ?? "") : String(steps.session?.session_id ?? ""),
  resumed: Boolean(steps.previous?.found),
});
`.trim();

/** Create the thread's session unless a kept one resumes. A resume is
 * silent: the thread already carries the session link from its first run,
 * and to the people in it the conversation simply continues. (`is_true`
 * with an empty then-arm, not `is_false`: the editor's preview cannot run
 * the lookup, and a missing value is not false — the preview must still
 * walk the create arm.) */
const sessionUnlessResumed: BlockDef = {
  id: "has_previous",
  type: "branch",
  config: {
    conditions: { mode: "all", conditions: [{ path: "steps.previous.found", op: "is_true" }] },
  },
  then: [],
  else: [session, started],
};

const pick: BlockDef = { id: "pick", type: "code", config: { source: PICK_SOURCE, mode: "value" } };

/** Every later block's session ref: adoption (D11) moves a resumed session's
 * binding to this run; a created one is already ours. */
const SESSION_REF = { template: "${{ steps.pick.value.session_id }}" } as const;

const relay: BlockDef = {
  id: "relay",
  type: RELAY_SESSION_TYPE,
  config: {
    session: SESSION_REF,
    provider: "slack",
    team: `\${{ ${F}.team }}`,
    channel: `\${{ ${F}.channel }}`,
    threadTs: `\${{ ${F}.thread_ts }}`,
    mentionTs: `\${{ ${F}.mention_ts }}`,
    userId: `\${{ ${F}.user_id }}`,
    eventId: `\${{ ${F}.event_id }}`,
  },
};

/** The first turn. A created session got the fold as its initial prompt, so
 * the run only waits for it to go idle. A resumed session is prompted with
 * the fold (send_prompt adopts it) and waited on the same way; a resumed
 * session with nothing to say to (a bare mention) is only waited on. */
const firstTurn: BlockDef = {
  id: "opening_turn",
  type: "branch",
  config: {
    conditions: {
      mode: "all",
      conditions: [
        { path: "steps.previous.found", op: "is_true" },
        { path: "steps.opening.value.has_text", op: "is_true" },
      ],
    },
  },
  then: [
    {
      id: "resume_turn",
      type: "send_prompt",
      tunable: ["promptTemplate", "deadlineSeconds"],
      config: {
        session: SESSION_REF,
        promptTemplate: "${{ steps.opening.value.text }}",
        waitFor: { kind: "run_end" },
        deadlineSeconds: TURN_DEADLINE_S,
      },
    },
  ],
  else: [
    {
      id: "first_turn",
      type: "wait_session",
      tunable: ["deadlineSeconds"],
      config: { session: SESSION_REF, until: "idle", deadlineSeconds: TURN_DEADLINE_S },
    },
  ],
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
        // Only an explicit mention in the thread is a turn (the legacy
        // model). A plain reply is never delivered here; the next mention's
        // fold carries it as thread context.
        eventKeys: ["app_mention"],
        // Bots never continue a thread either (the opening admission has the
        // same rule): an `app_mention` authored by a bot — including our own
        // bubbles when they quote the handle — reaches the mailbox, so the
        // wait refuses it here and keeps listening.
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
    // A quiet thread leaves outcome=deadline and no event: nothing to read
    // or fold (the loop's `until` exits). Only a mention goes on.
    {
      id: "has_event",
      type: "branch",
      config: {
        conditions: {
          mode: "all",
          conditions: [{ path: "steps.next.outcome", op: "equals", value: "event" }],
        },
      },
      then: [
        listReplies("thread_replies", PREVIOUS_MENTION_TS),
        {
          id: "turn_text",
          type: "code",
          config: { source: NEXT_TEXT_SOURCE, mode: "value" },
        },
        // The turn gate: a bare mention leaves no text, and send_prompt must not
        // fire on it (an empty prompt is a render failure that would end the
        // run), so the branch requires text.
        {
          id: "has_turn",
          type: "branch",
          config: {
            conditions: {
              mode: "all",
              conditions: [{ path: "steps.turn_text.value.has_text", op: "is_true" }],
            },
          },
          then: [
            // Re-point the relay at the accepted follow-up BEFORE its prompt is
            // sent: ⏳ lands on the reply, its responses open a fresh bubble and
            // ✅ seals on the reply — the legacy per-turn mention ownership.
            {
              id: "repoint",
              type: RELAY_SESSION_TYPE,
              config: {
                session: SESSION_REF,
                provider: "slack",
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
                session: SESSION_REF,
                promptTemplate: "${{ steps.turn_text.value.text }}",
                waitFor: { kind: "run_end" },
                deadlineSeconds: TURN_DEADLINE_S,
              },
            },
          ],
          else: [],
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
    // A mention opens a thread. Replies come in through the `reply`
    // entrypoint below. No channel scope: the brain answers wherever the
    // Slack app is a member and gets mentioned, as the legacy workflow did.
    // (The per-channel map was the ADR 0119 parallel-window gate; enabling
    // the automation is the switch now, and the map only overrides the
    // profile per channel.)
    eventKeys: ["app_mention"],
  },
  blocks: [...admit, ...opening, sessionUnlessResumed, pick, relay, firstTurn, conversation],
  // ADR 0120: a Slack thread is a WORKSTREAM. The first mention opens it; a
  // later mention in the same thread renders the same key, so it binds to
  // the open workstream and JOINS the thread's live run (the mailbox event
  // the loop's wait consumes). No reply entrypoint: a plain reply is not an
  // event the brain acts on.
  entrypoints: [],
  inputsSchema: [
    {
      key: "channels",
      label: "Channel overrides",
      type: "map",
      keyNoun: "channel",
      help: "Channels whose threads run on a different profile than the default.",
      default: {},
      valueShape: { type: "string", label: "Profile id" },
    },
    {
      key: "default_profile",
      label: "Default profile",
      type: "string",
      help: "The session profile every thread runs on unless a channel override says otherwise. With no default, only channels with an override get answers.",
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
    instance: {
      // One workstream per thread: team, channel, thread root. A reply
      // carries thread_ts; the opening mention only has ts. (`coalesce`, not
      // `default:` — default's fallback argument is strict.)
      keyTemplate:
        '${{ event.raw.team_id }}:${{ event.raw.event.channel }}:${{ event.raw.event | coalesce: "thread_ts", "ts" }}',
      // The thread's title on the Slack page and the rail: the opening
      // mention's text, as a human reads it.
      labelTemplate: "${{ event.raw.event.text | strip_mentions | truncate: 80 }}",
    },
    concurrency: {
      // One run per thread, instance-scoped: the same template as the
      // workstream key, so a reply's claim lands on the mention's run.
      keyTemplate:
        '${{ event.raw.team_id }}:${{ event.raw.event.channel }}:${{ event.raw.event | coalesce: "thread_ts", "ts" }}',
      policy: "join",
    },
    runDeadlineSeconds: RUN_DEADLINE_S,
    endSessionsOnFinish: false,
    onFinalize: [
      // No post on `completed`: that is the idle exit — a pause, not an
      // end. The ✅ on the last mention already says the turn is done, and
      // the next mention continues the same session.
      {
        when: ["deadline", "failed", "halted", "superseded", "filtered"],
        block: {
          id: "recap",
          type: RELAY_CLOSE_TYPE,
          config: { status: "${{ run.status }}" },
        },
      },
      // Only an explicit end closes the workstream: a quiet thread's run
      // completes (and a failed or deadline-hit one ends) with the workstream
      // OPEN, so the next mention binds to it, starts a new run, and resumes
      // the kept session. Halted (`@stop`) and superseded are the ends.
      //
      // NOT on `filtered` either: an unlinked author's mention posts the
      // "log in first" notice through the Slack post_message action, which
      // binds the thread's handle to this workstream; closing would silence
      // the thread for the linked colleague who mentions the bot next.
      {
        when: ["halted", "superseded"],
        block: {
          id: "close",
          type: "instance_close",
          config: { reason: "thread run ended: ${{ run.status }}" },
        },
      },
    ],
  },
};

export const SLACK_BRAIN_BUILTIN: BuiltinAutomation = {
  key: SLACK_BRAIN_BUILTIN_KEY,
  name: "Slack threads",
  previousNames: ["Slack thread brain"],
  description:
    "Answers @-mentions in Slack with one session per thread, relaying the conversation both ways.",
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
