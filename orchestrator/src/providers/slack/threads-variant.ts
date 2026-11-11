/** Slack's variant of the chat-threads built-in. */

import { THREADS_FACTS, type ThreadsVariant } from "../../automations/builtins/threads-variant.ts";

const F = THREADS_FACTS;

/** How to behave in a Slack thread, and Slack's mrkdwn instead of Markdown
 * (ADR 0060 Decision 8). A session keeps the text it was created with. */
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
 * the previous turn's mention — the one the session's relay points at (a
 * mention the brain dropped at admission was no turn and moves nothing);
 * with no relay, the newest human message before this one that mentions
 * the bot (any mention when the payload does not say who we are; our own
 * newest reply as the last resort). Messages newer than the mention are
 * left for
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
  const relayMention = String(steps.previous?.mention_ts ?? "");
  const since = continued ? relayMention || previousMention || lastOwn : "";
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

export const SLACK_THREADS: ThreadsVariant = {
  provider: "slack",
  key: "slack_brain",
  name: "Slack threads",
  previousNames: ["Slack thread brain"],
  description:
    "Answers @-mentions in Slack with one session per thread, relaying the conversation both ways.",
  definitionVersion: 15,
  mentionEvents: ["app_mention"],
  facts: {
    type: "code",
    config: { source: SLACK_FACTS_SOURCE, mode: "value" },
  },
  // A continued session reads only what came after its previous turn: the
  // page is capped, and a long thread must not push the new messages off it.
  replies: {
    type: "integration_action",
    config: {
      provider: "slack",
      actionId: "list_replies",
      params: {
        channel: `\${{ ${F}.channel }}`,
        threadTs: `\${{ ${F}.thread_ts }}`,
        oldest: "${{ steps.previous.mention_ts | default: '' }}",
      },
    },
  },
  turn: { type: "code", config: { source: SLACK_TURN_SOURCE, mode: "value" } },
  actions: { post: "post_message", update: "update_message" },
  // `coalesce`, not `default:`, whose fallback argument is strict.
  threadKeyTemplate:
    '${{ event.raw.team_id }}:${{ event.raw.event.channel }}:${{ event.raw.event | coalesce: "thread_ts", "ts" }}',
  threadLabelTemplate: "${{ event.raw.event.text | strip_mentions | truncate: 80 }}",
  systemPromptAppend: SLACK_SYSTEM_PROMPT_APPEND,
};
