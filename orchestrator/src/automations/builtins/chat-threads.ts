/** The chat-threads built-in automation, one variant for each chat provider
 * (ADR 0119 D7; a workstream per thread, ADR 0120).
 *
 * `chatThreadsDefinition` holds the graph every variant shares: an agent
 * scoped to the thread, with one way in, a mention. Every mention is one
 * short run that admits it, folds the thread into a prompt, and ends on the
 * agent step, which continues the thread's kept session or starts one.
 * Nothing waits. The relay that mirrors the session into the thread is
 * bound to the session, so the agent's replies and questions reach the
 * thread with no run live.
 *
 * A `ThreadsVariant` fills what a chat provider decides: the mention
 * events, the facts step, how the thread is read and folded into a prompt,
 * the message actions, the thread's key, and the system prompt.
 *
 * Every block is a catalog block: Duplicate gives a user an editable copy.
 *
 * Only an explicit mention is a turn. A plain reply is folded into the next
 * mention's prompt as thread context, with the mention's text as the
 * directive.
 *
 * Routing: a channel override always decides. Otherwise `routing` picks:
 * `default` runs every thread on `default_profile`; `smart` lets a decision
 * model pick the profile from the thread, and asks the person with a card
 * when the model is not confident or the person asks to choose. When no
 * model router can decide, smart routing is the same as `default`. Only a
 * new session is routed; a kept session continues on its own profile.
 */

import {
  MAX_WAIT_DEADLINE_S,
  type AgentSpec,
  type AutomationDefinition,
  type BlockDef,
} from "../engine/definition.ts";
import type { BuiltinAutomation } from "../engine/builtins.ts";
import { LOOKUP_INSTANCE_SESSION_TYPE } from "../engine/blocks/instance-session.ts";
import { RESOLVE_USER_TYPE } from "../engine/blocks/resolve-user.ts";
import { CHOICE_LABEL_MAX, CHOICE_TYPE } from "../engine/blocks/choice.ts";
import { profileRouteQuestions } from "../profile-cards.ts";
import { NO_USER_MSG } from "../../chat/identity.ts";
import { DEFAULT_CONNECTION_PLACEHOLDER } from "../engine/builtins.ts";
import { THREADS_FACTS, type ThreadsVariant } from "./threads-variant.ts";

/** Smart routing routes without asking at or above this confidence. In a
 * routing benchmark against real history, at 0.8 the model routes about 60%
 * of new threads alone and is right ~82-89% of the time on those; the
 * 0.6-0.8 band is barely better than a coin flip, so it asks. */
export const THREADS_DEFAULT_MIN_CONFIDENCE = 0.8;
/** How long the profile card waits for a click before the model's pick. */
export const THREADS_DEFAULT_ASK_TIMEOUT_S = 600;

const F = THREADS_FACTS;

/** Smart routing's verdict on the model's answer: ask the person when the
 * model is not confident or the person asks to choose, with the model's
 * best candidates (and the default profile) as the card's buttons. When the
 * model could not decide, the default profile serves — and with no default,
 * the person is asked, unless no model router can decide (then smart routing
 * behaves exactly like default routing). */
export const TRIAGE_SOURCE = `
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
      .map((id) => ({ value: id, label: String(labelOf.get(id)).slice(0, ${CHOICE_LABEL_MAX}) }));
  };
  if (options.length === 0) return { ask: false, options: [] };
  if (!route.decided) {
    const ask = !fallback && route.reason !== "router_not_connected" && options.length > 1;
    return { ask, options: ask ? card(options.map((o) => o.value)) : [] };
  }
  const pick = route.answers.profile;
  const min = Number(inputs.smart_min_confidence ?? ${THREADS_DEFAULT_MIN_CONFIDENCE});
  const wants = Number(route.answers.wants_choice?.yes ?? 0);
  const ask = options.length > 1 && (pick.confidence < min || wants >= 0.5);
  return { ask, options: ask ? card(pick.ranked.slice(0, 4)) : [] };
};
`.trim();

/** Smart routing's verdict: the profile the new session runs on, and how it
 * was chosen — the person's click, else the model's pick, else the default.
 * An empty id means nobody chose: the run ends `filtered` before any
 * session exists. */
export const RESOLVE_SOURCE = `
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


export function chatThreadsDefinition(v: ThreadsVariant): AutomationDefinition {
  const admit: BlockDef[] = [
    v.facts,
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
    // The identity gate: the author must be an engrams user, and the session
    // runs as that user. An unlinked author gets "log in first" in the
    // thread, and the run ends `filtered` before any session exists.
    {
      id: "identity",
      type: RESOLVE_USER_TYPE,
      config: { provider: v.provider, externalUserId: `\${{ ${F}.user_id }}` },
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
            provider: v.provider,
            actionId: v.actions.post,
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

  const resolveProfile: BlockDef = {
    id: "resolve",
    type: "code",
    config: { source: RESOLVE_SOURCE, mode: "value" },
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
          provider: v.provider,
          actionId: v.actions.update,
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
              provider: v.provider,
              actionId: v.actions.post,
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
      { id: "triage", type: "code", config: { source: TRIAGE_SOURCE, mode: "value" } },
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
            type: CHOICE_TYPE,
            tunable: ["question"],
            config: {
              provider: v.provider,
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
  const agent: AgentSpec = {
    // A thread is a workstream (ADR 0120). The first mention opens it; a
    // later mention in the same thread renders the same key, binds to the
    // open workstream, and its run finds the thread's kept session. The
    // label is the thread's title on its page and the rail.
    scope: {
      kind: "key",
      keyTemplate: v.threadKeyTemplate,
      labelTemplate: v.threadLabelTemplate,
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
    appendSystemPrompt: v.systemPromptAppend,
    relay: {
      provider: v.provider,
      team: `\${{ ${F}.team }}`,
      channel: `\${{ ${F}.channel }}`,
      threadTs: `\${{ ${F}.thread_ts }}`,
      mentionTs: `\${{ ${F}.mention_ts }}`,
      userId: `\${{ ${F}.user_id }}`,
      eventId: `\${{ ${F}.event_id }}`,
    },
  };

  return {
    engine: 1,
    trigger: {
      kind: "integration",
      provider: v.provider,
      connectionId: DEFAULT_CONNECTION_PLACEHOLDER,
      // A mention is a turn; a plain reply is thread context on the next
      // mention. No channel scope: the built-in answers wherever the app is
      // a member and gets mentioned. The per-channel map only overrides the
      // profile.
      eventKeys: v.mentionEvents,
    },
    blocks: [...admit, previous, v.replies, v.turn, hasTurn, routeUnlessContinued],
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
        default: THREADS_DEFAULT_MIN_CONFIDENCE,
        min: 0,
        max: 1,
      },
      {
        key: "ask_timeout",
        label: "Profile question timeout (seconds)",
        type: "number",
        help: "How long the profile question waits for a click before smart routing takes the model's pick.",
        default: THREADS_DEFAULT_ASK_TIMEOUT_S,
        min: 30,
        max: MAX_WAIT_DEADLINE_S,
      },
    ],
    // One run at a time per thread (the engine derives `queue` from the
    // kept scope): two mentions in quick succession deliver their turns in
    // order, and a turn never finds the thread's session owned by a live run.
    settings: {
      endSessionsOnFinish: false,
      onFinalize: [
        // A run that fails (the thread page could not be read, the prompt
        // could not be delivered) tells the thread ❌ and why. The session's
        // relay stays: the session is fine, this turn did not reach it, and
        // the next mention is a new turn on it. No post on `completed` (the
        // session's own ✅ already says the turn is done), nor on `filtered`
        // (an unlinked author, a bare mention).
        {
          when: ["failed"],
          block: {
            id: "recap",
            type: "integration_action",
            tunable: ["params"],
            config: {
              provider: v.provider,
              actionId: v.actions.post,
              params: {
                channel: `\${{ ${F}.channel }}`,
                threadTs: `\${{ ${F}.thread_ts }}`,
                text: "❌ This turn failed: ${{ run.error }}",
              },
            },
          },
        },
      ],
    },
  };
}

export function chatThreadsBuiltin(v: ThreadsVariant): BuiltinAutomation {
  return {
    key: v.key,
    name: v.name,
    ...(v.previousNames ? { previousNames: v.previousNames } : {}),
    description: v.description,
    definitionVersion: v.definitionVersion,
    definition: chatThreadsDefinition(v),
    async defaultInputs() {
      return {
        channels: {},
        default_profile: "",
        routing: "default",
        smart_profiles: [],
        smart_min_confidence: THREADS_DEFAULT_MIN_CONFIDENCE,
        ask_timeout: THREADS_DEFAULT_ASK_TIMEOUT_S,
      };
    },
  };
}
