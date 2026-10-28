/** The Slack thread-brain built-in automation (ADR 0119 D7; a workstream
 * per thread since 2026-09-29, ADR 0120; one short run per mention since
 * contract 5; an agent with one way in since the 2026-10-27 amendment).
 *
 * The conversation-shaped built-in, expressed as data on the engine: an
 * AGENT scoped to the Slack thread, with one way in — a mention. Every
 * mention is ONE short run that admits it, folds the thread into a prompt,
 * and ends on the agent step, which continues the thread's kept session
 * when it is alive or starts one. Nothing waits. The durable thing is the
 * kept session plus the open workstream; the relay that mirrors the session
 * into the thread is bound to the session, so the agent's replies,
 * questions and ⏳/✅ reach the thread with no run live.
 *
 * Every block here is a catalog block: this built-in is an example an
 * ordinary user could have built, and Duplicate gives them an editable copy.
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
 * Slack action plus a Code block.
 *
 * Routing: a channel override always decides. Otherwise `routing` picks:
 * `default` runs every thread on `default_profile`; `smart` lets a decision
 * model (the `decide` block, Jev through OpenRouter) pick the profile from
 * the thread, and asks the person with a card (`slack_choice`) when the
 * model is not confident or the person asks to choose. Without an
 * OpenRouter key, smart routing falls back to the default profile — the
 * same behavior as `default`. Only a NEW session is routed; a kept session
 * continues on its own profile.
 */

import {
  MAX_WAIT_DEADLINE_S,
  type AgentSpec,
  type AutomationDefinition,
  type BlockDef,
} from "../engine/definition.ts";
import type { BuiltinAutomation } from "../engine/builtins.ts";
import { RELAY_CLOSE_TYPE } from "../engine/blocks/relay-close.ts";
import { LOOKUP_INSTANCE_SESSION_TYPE } from "../engine/blocks/instance-session.ts";
import { RESOLVE_USER_TYPE } from "../engine/blocks/resolve-user.ts";
import { SLACK_CHOICE_LABEL_MAX, SLACK_CHOICE_TYPE } from "../engine/blocks/slack-choice.ts";
import { profileRouteQuestions } from "../profile-cards.ts";
import { NO_USER_MSG } from "../../integrations/slack-identity.ts";
import { DEFAULT_CONNECTION_PLACEHOLDER } from "./pr-review.ts";

export const SLACK_BRAIN_BUILTIN_KEY = "slack_brain";

/** Bump on any graph or inputs-schema change. */
export const SLACK_BRAIN_DEFINITION_VERSION = 15;

/** Smart routing routes without asking at or above this confidence. */
/** From a routing benchmark against real history: at 0.8 the model routes
 * about 60% of new threads alone and is right ~82-89% of the time on those;
 * the 0.6-0.8 band is barely better than a coin flip, so it asks. */
export const SLACK_BRAIN_DEFAULT_MIN_CONFIDENCE = 0.8;
/** How long the profile card waits for a click before the model's pick. */
export const SLACK_BRAIN_DEFAULT_ASK_TIMEOUT_S = 600;

const F = "steps.facts.value";

/** Appended to the agent's system prompt when the thread's session is
 * created (ADR 0060 Decision 8): how to behave in a chat thread, and
 * Slack's mrkdwn instead of Markdown. The same text ADR 0060's thread
 * workflow appended; it rides `appendSystemPrompt` (the harness's
 * ENGRAM_APPEND_SYSTEM_PROMPT), so a continued session keeps the flavor it
 * was created with. */
export const SLACK_SYSTEM_PROMPT_APPEND = `You are running inside an engrams session triggered from a Slack thread.
Keep replies concise and chat-friendly.

When you need a decision or clarification from the user, ask via the ask_user_question tool — it renders as interactive buttons in Slack — rather than guessing. The user cannot see your terminal, so surface results, links, and artifacts explicitly. When handling code related tasks, prefer showing your work rather than just saying you're done. Prefer video over images if available.

Conform to slack markdown in your responses. Examples:
Links are formatted as <url|optional link title>
Bold is single asterisks surrounding text, like *this*.
Italics are underlines surrounding text like _this_.`;

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
  // Only a mention is a turn. A plain reply rides the next mention's fold.
  if (key !== "app_mention") return null;
  const channels = inputs.channels ?? {};
  const override = channels[channel] ?? "";
  // Smart routing decides where no override does; the default profile is
  // its fallback. Without either, the mention is not answered.
  const smart = !override && inputs.routing === "smart";
  const profile = override || inputs.default_profile || "";
  if (!profile && !smart) return null;
  const text = String(ev.text ?? "")
    .replace(/<@[^>]+>/g, " ")
    .replace(/[^\\S\\n]+/g, " ")
    .trim();
  const str = (v) => (v === undefined || v === null ? "" : String(v));
  return {
    admit: true,
    profile_id: String(profile),
    override: Boolean(override),
    smart,
    team: str(raw.team_id),
    channel,
    thread_ts: str(ev.thread_ts || ev.ts),
    mention_ts: str(ev.ts),
    text,
    title: text.slice(0, 80) || "Slack thread",
    user_id: str(ev.user),
    event_id: str(raw.event_id),
    // The app's own bot user: the fold tells our posts (and the mentions
    // of us) from other bots' by it.
    bot_user_id: str((raw.authorizations ?? [])[0]?.user_id),
  };
};
`.trim();

/** The turn prompt: the legacy `foldReplies` in the QuickJS cage.
 * `messages` is the thread page the `list_replies` action read; the
 * message whose ts is the mention is the directive and goes at the bottom,
 * every other kept message is prior context in `<thread context>`.
 *
 * A NEW session gets the whole thread so far: a mention in the middle of a
 * human conversation brings that conversation along. A CONTINUED session
 * already saw everything up to the previous turn, so the fold starts after
 * the previous turn's mention — the newest human message before this one
 * that mentions the bot (any mention when the payload does not say who we
 * are; our own newest reply as the last resort). Messages newer than the
 * mention are left for
 * the NEXT turn (they arrived after the ask; the next mention's fold
 * carries them). Bot-authored messages never feed the prompt (the agent
 * must not read its own posts) EXCEPT the thread root, which is the subject
 * of the thread. Mentions are stripped. When the page does not carry the
 * mention yet (Slack lag), the event's own text is the directive.
 * `has_text` is the turn gate: a bare `@bot` is not a prompt. */
export const SLACK_TURN_SOURCE = `
export default ({ steps }) => {
  const facts = steps.facts?.value ?? {};
  const messages = steps.replies?.messages ?? [];
  const trigger = String(facts.mention_ts ?? "");
  const root = String(facts.thread_ts ?? "");
  const continued = Boolean(steps.previous?.found);
  const ownBot = String(facts.bot_user_id ?? "");
  const num = (ts) => Number.parseFloat(String(ts ?? "")) || 0;
  const strip = (t) => String(t ?? "")
    .replace(/<@[^>]+>/g, " ")
    .replace(/[^\\S\\n]+/g, " ")
    .trim();
  // The previous turn's mention, and our newest reply as the stand-in. A
  // bot post with no \`user\` counts as ours; a post by a different bot
  // user does not.
  let previousMention = "";
  let lastOwn = "";
  for (const m of messages) {
    const ts = String(m.ts ?? "");
    if (!ts || num(ts) >= num(trigger)) continue;
    const isBot = Boolean(m.bot_id || m.subtype === "bot_message");
    if (isBot) {
      const ours = !ownBot || !m.user || m.user === ownBot;
      if (ours && ts !== root && num(ts) > num(lastOwn)) lastOwn = ts;
    } else if (String(m.text ?? "").includes(ownBot ? "<@" + ownBot + ">" : "<@")) {
      if (num(ts) > num(previousMention)) previousMention = ts;
    }
  }
  const since = continued ? previousMention || lastOwn : "";
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
  const directive = idx >= 0 ? kept[idx].text : strip(facts.text);
  const context = kept.filter((_, i) => i !== idx).map((k) => k.text);
  const text = !directive
    ? ""
    : context.length
      ? "<thread context>\\n" + context.join("\\n") + "\\n</thread context>\\n\\n" + directive
      : directive;
  return { text, has_text: text.length > 0, continued };
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

/** The thread's kept session from an earlier turn, if any. Looked up before
 * the fold (a continued session's earlier turns stay out of its prompt) and
 * before routing (a continued session is never re-routed). */
const previous: BlockDef = { id: "previous", type: LOOKUP_INSTANCE_SESSION_TYPE, config: {} };

/** The thread as Slack holds it, for the fold. */
const replies: BlockDef = {
  id: "replies",
  type: "integration_action",
  config: {
    provider: "slack",
    actionId: "list_replies",
    params: {
      channel: `\${{ ${F}.channel }}`,
      threadTs: `\${{ ${F}.thread_ts }}`,
    },
  },
};

const turn: BlockDef = { id: "turn", type: "code", config: { source: SLACK_TURN_SOURCE, mode: "value" } };

/** Smart routing's verdict on the model's answer: ask the person when the
 * model is not confident or the person asks to choose, with the model's
 * best candidates (and the default profile) as the card's buttons. When the
 * model could not decide, the default profile serves — and with no default,
 * the person is asked, unless there is no OpenRouter key at all (then smart
 * routing behaves exactly like default routing). */
export const SLACK_TRIAGE_SOURCE = `
export default ({ steps, inputs }) => {
  const route = steps.route ?? {};
  const options = steps.candidates?.options ?? [];
  const labelOf = new Map(options.map((o) => [o.value, o.label ?? o.value]));
  const fallback = String(inputs.default_profile ?? "");
  const card = (ids) => {
    const order = [...ids];
    if (fallback && labelOf.has(fallback) && !order.includes(fallback)) order.push(fallback);
    // Slack shows at most 75 characters of a button; the card's labels
    // are the profile names, cut to fit.
    return order
      .filter((id) => labelOf.has(id))
      .slice(0, 5)
      .map((id) => ({ value: id, label: String(labelOf.get(id)).slice(0, ${SLACK_CHOICE_LABEL_MAX}) }));
  };
  if (options.length === 0) return { ask: false, options: [] };
  if (!route.decided) {
    const ask = !fallback && route.reason !== "router_not_connected" && options.length > 1;
    return { ask, options: ask ? card(options.map((o) => o.value)) : [] };
  }
  const pick = route.answers.profile;
  const min = Number(inputs.smart_min_confidence ?? ${SLACK_BRAIN_DEFAULT_MIN_CONFIDENCE});
  const wants = Number(route.answers.wants_choice?.yes ?? 0);
  const ask = options.length > 1 && (pick.confidence < min || wants >= 0.5);
  return { ask, options: ask ? card(pick.ranked.slice(0, 4)) : [] };
};
`.trim();

/** Smart routing's verdict: the profile the new session runs on, and how it
 * was chosen — the person's click, else the model's pick, else the default.
 * An empty id means nobody chose: the run ends `filtered` before any
 * session exists. */
export const SLACK_RESOLVE_SOURCE = `
export default ({ steps }) => {
  const facts = steps.facts.value;
  const route = steps.route ?? {};
  // No OpenRouter key: exactly default routing.
  if (route.reason === "router_not_connected") {
    return { profile_id: facts.profile_id, label: "", card: "" };
  }
  const options = steps.candidates?.options ?? [];
  const name = (id) => options.find((o) => o.value === id)?.label ?? "";
  const pick = route.decided ? route.answers.profile : null;
  const pct = pick ? Math.round(pick.confidence * 100) : 0;
  let id = "";
  let how = "";
  if (steps.ask?.outcome === "answered") {
    id = steps.ask.value;
    how = "picked in the thread";
  } else if (pick) {
    id = pick.value;
    how = (steps.ask?.outcome === "deadline" ? "no answer, so " : "") + "routed by Jev, " + pct + "% confident";
  } else if (facts.profile_id) {
    id = facts.profile_id;
    how = (steps.ask?.outcome === "deadline" ? "no answer, so " : "") + "the default profile";
  } else if (options.length === 1) {
    // One candidate and no default: there was nothing to choose between.
    id = options[0].value;
    how = "the only profile smart routing can pick";
  }
  if (!id) return { profile_id: "", label: "", card: "No profile was chosen. Mention me again to retry." };
  const shown = name(id) || "the default profile";
  return {
    profile_id: id,
    label: " on *" + shown + "* (" + how + ")",
    card: "Profile: *" + shown + "* (" + how + ")",
  };
};
`.trim();

const resolveProfile: BlockDef = {
  id: "resolve",
  type: "code",
  config: { source: SLACK_RESOLVE_SOURCE, mode: "value" },
};

/** The card becomes the outcome line (its buttons go), so the thread shows
 * what was decided, answered or not. When nobody was asked, the verdict is
 * posted as a line of its own, so the thread still says which profile
 * answers and why (an empty verdict — no key, default routing — posts
 * nothing). */
const closeCard: BlockDef = {
  id: "asked",
  type: "branch",
  config: {
    conditions: { mode: "all", conditions: [{ path: "steps.triage.value.ask", op: "is_true" }] },
  },
  then: [
    {
      id: "close_card",
      type: "integration_action",
      config: {
        provider: "slack",
        actionId: "update_message",
        params: {
          channel: `\${{ ${F}.channel }}`,
          ts: "${{ steps.ask.message_ts | default: '' }}",
          text: "${{ steps.resolve.value.card }}",
        },
      },
    },
  ],
  else: [
    {
      id: "has_verdict",
      type: "branch",
      config: {
        conditions: {
          mode: "all",
          conditions: [{ path: "steps.resolve.value.card", op: "not_equals", value: "" }],
        },
      },
      then: [
        {
          id: "verdict",
          type: "integration_action",
          config: {
            provider: "slack",
            actionId: "post_message",
            params: {
              channel: `\${{ ${F}.channel }}`,
              threadTs: `\${{ ${F}.thread_ts }}`,
              text: "${{ steps.resolve.value.card }}",
            },
          },
        },
      ],
      else: [],
    },
  ],
};

const routed: BlockDef = {
  id: "routed",
  type: "filter",
  config: {
    conditions: {
      mode: "all",
      conditions: [{ path: "steps.resolve.value.profile_id", op: "not_equals", value: "" }],
    },
  },
};

/** Smart routing, for a NEW session only: the candidates, the model's
 * answer, a card for the person when it is unsure, and the verdict. Default
 * routing skips all of it (one condition step), so it adds nothing to a
 * thread's first reply. */
const smartRoute: BlockDef = {
  id: "smart_route",
  type: "branch",
  config: {
    conditions: { mode: "all", conditions: [{ path: `${F}.smart`, op: "is_true" }] },
  },
  then: [
    { id: "candidates", type: "list_profiles", config: { ids: { $ref: "inputs.smart_profiles" } } },
    // No candidate left (every chosen profile archived): there is nothing
    // to ask the model, and the verdict falls back to the default.
    {
      id: "has_candidates",
      type: "branch",
      config: {
        conditions: {
          mode: "all",
          conditions: [{ path: "steps.candidates.count", op: "gt", value: 0 }],
        },
      },
      then: [
        {
          id: "route",
          type: "decide",
          config: {
            // The opening fold: the thread so far plus the mention, the same
            // text the session will get as its first prompt.
            state: { message: "${{ steps.turn.value.text }}" },
            questions: profileRouteQuestions({ $ref: "steps.candidates.options" }),
            // A model outage must never cost the thread its answer.
            onError: "undecided",
          },
        },
      ],
      else: [],
    },
    { id: "triage", type: "code", config: { source: SLACK_TRIAGE_SOURCE, mode: "value" } },
    {
      id: "unsure",
      type: "branch",
      config: {
        conditions: {
          mode: "all",
          conditions: [{ path: "steps.triage.value.ask", op: "is_true" }],
        },
      },
      then: [
        {
          id: "ask",
          type: SLACK_CHOICE_TYPE,
          tunable: ["question"],
          config: {
            provider: "slack",
            team: `\${{ ${F}.team }}`,
            channel: `\${{ ${F}.channel }}`,
            threadTs: `\${{ ${F}.thread_ts }}`,
            question: "Which profile should handle this thread?",
            options: { $ref: "steps.triage.value.options" },
            deadlineSeconds: { $ref: "inputs.ask_timeout" },
          },
        },
      ],
      else: [],
    },
    resolveProfile,
    closeCard,
    routed,
  ],
  else: [],
};

/** Route a NEW session only. A continued session keeps its profile. (`is_true`
 * with an empty then-arm, not `is_false`: the editor's preview cannot run
 * the lookup, and a missing value is not false — the preview must still
 * walk the routing arm.) */
const routeUnlessContinued: BlockDef = {
  id: "has_previous",
  type: "branch",
  config: {
    conditions: { mode: "all", conditions: [{ path: "steps.previous.found", op: "is_true" }] },
  },
  then: [],
  else: [smartRoute],
};

/** The turn gate: a bare `@bot` leaves no text and is not a turn — the run
 * ends filtered before routing or the agent; no prompt, no failure; the
 * thread simply goes on. */
const hasTurn: BlockDef = {
  id: "has_turn",
  type: "filter",
  config: {
    conditions: { mode: "all", conditions: [{ path: "steps.turn.value.has_text", op: "is_true" }] },
  },
};

/** The thread's standing agent. The way in ends on it: the kept session
 * gets the turn as a prompt; a new session starts with it. Either way the
 * relay is (re-)pointed at this mention, so ⏳/✅ and the replies land
 * under it, and a new session's link is posted into the thread first. */
const THREAD_KEY =
  '${{ event.raw.team_id }}:${{ event.raw.event.channel }}:${{ event.raw.event | coalesce: "thread_ts", "ts" }}';

const agent: AgentSpec = {
  // ADR 0120: a Slack thread is a WORKSTREAM. The first mention opens it; a
  // later mention in the same thread renders the same key, binds to the
  // open workstream, and its run finds the thread's kept session.
  // (`coalesce`, not `default:` — default's fallback argument is strict.)
  // The thread's title on the Slack page and the rail: the opening
  // mention's text, as a human reads it.
  scope: {
    kind: "key",
    keyTemplate: THREAD_KEY,
    labelTemplate: "${{ event.raw.event.text | strip_mentions | truncate: 80 }}",
  },
  // Smart routing's verdict, else the facts' (override or default).
  profileId: `\${{ steps.resolve.value.profile_id | default: ${F}.profile_id }}`,
  promptTemplate: "${{ steps.turn.value.text }}",
  titleTemplate: `\${{ ${F}.title }}`,
  role: "primary",
  // The session is the asking user's: their credentials and attribution
  // (never the programmatic org credential a stranger could borrow).
  ownerUserId: "${{ steps.identity.user_id }}",
  // D8 + the thread model: the session outlives every run.
  keepOnFinish: true,
  appendSystemPrompt: SLACK_SYSTEM_PROMPT_APPEND,
  relay: {
    provider: "slack",
    team: `\${{ ${F}.team }}`,
    channel: `\${{ ${F}.channel }}`,
    threadTs: `\${{ ${F}.thread_ts }}`,
    mentionTs: `\${{ ${F}.mention_ts }}`,
    userId: `\${{ ${F}.user_id }}`,
    eventId: `\${{ ${F}.event_id }}`,
  },
};

export const SLACK_BRAIN_DEFINITION: AutomationDefinition = {
  engine: 1,
  trigger: {
    kind: "integration",
    provider: "slack",
    connectionId: DEFAULT_CONNECTION_PLACEHOLDER,
    // A mention is a turn; a plain reply is thread context on the next
    // mention. No channel scope: the brain answers wherever the Slack app
    // is a member and gets mentioned, as the legacy workflow did. (The
    // per-channel map only overrides the profile per channel.)
    eventKeys: ["app_mention"],
  },
  blocks: [...admit, previous, replies, turn, hasTurn, routeUnlessContinued],
  // One way in. The thread's agent is the definition's: every mention's
  // run ends on it.
  entrypoints: [],
  agent,
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
      key: "routing",
      label: "Routing",
      type: "enum",
      values: ["default", "smart"],
      help: "default: threads without a channel override run on the default profile. smart: a decision model picks the profile from the thread and asks in the thread when it is not sure (needs an OpenRouter key; without one, smart works like default).",
      default: "default",
    },
    {
      key: "smart_profiles",
      label: "Profiles smart routing can pick",
      type: "list",
      help: "Profile ids smart routing chooses between. Empty = every active profile. Leave out worker profiles no thread should run on.",
      default: [],
      valueShape: { type: "string" },
    },
    {
      key: "smart_min_confidence",
      label: "Smart routing confidence",
      type: "number",
      help: "Smart routing picks on its own at or above this confidence (0-1); below it, it asks in the thread.",
      default: SLACK_BRAIN_DEFAULT_MIN_CONFIDENCE,
      min: 0,
      max: 1,
    },
    {
      key: "ask_timeout",
      label: "Profile question timeout (seconds)",
      type: "number",
      help: "How long the profile question waits for a click before smart routing takes the model's pick.",
      default: SLACK_BRAIN_DEFAULT_ASK_TIMEOUT_S,
      min: 30,
      max: MAX_WAIT_DEADLINE_S,
    },
  ],
  settings: {
    concurrency: {
      // One run at a time per thread: two mentions in quick succession
      // deliver their turns in order, and a turn never finds the thread's
      // session owned by a live run.
      keyTemplate: THREAD_KEY,
      policy: "queue",
    },
    endSessionsOnFinish: false,
    onFinalize: [
      // A run that fails after reaching the session (the prompt could not
      // be delivered) tells the thread ❌ and lets the relay go; the next
      // mention re-installs it on the same session. The session is the
      // agent step's when it got that far, else the thread's kept one (a
      // failed delivery leaves the agent step without outputs). No post on
      // `completed` (the session's own ✅ already says the turn is done),
      // nor on `filtered` (an unlinked author, a bare mention: the
      // thread's relay must stay).
      {
        when: ["failed"],
        block: {
          id: "recap",
          type: RELAY_CLOSE_TYPE,
          config: {
            session: { template: "${{ steps.agent.session_id | default: steps.previous.session_id }}" },
            status: "${{ run.status }}",
          },
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
      routing: "default",
      smart_profiles: [],
      smart_min_confidence: SLACK_BRAIN_DEFAULT_MIN_CONFIDENCE,
      ask_timeout: SLACK_BRAIN_DEFAULT_ASK_TIMEOUT_S,
    };
  },
};
