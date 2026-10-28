/** The Slack thread brain through the real interpreter (fake deps): one
 * short run per mention. A run admits the mention, folds the thread into a
 * prompt, hands it to the thread's standing agent (`run_agent`: the kept
 * session when one is alive, else a new one) and ends. The relay is the
 * session's (`automation_session_relay`), installed or re-pointed by the
 * run and rendered by the consumer with no run live. */

import { afterEach, describe, expect, test } from "bun:test";

import { coerceFieldValue, validateFieldValue } from "../../../connectors/field-schema.ts";
import { findAction } from "../../actions/execute.ts";
import { makeCodeBlockRuntime } from "../../code/runtime.ts";
import { makeFakeRelayStore, type FakeRelayStore } from "../../engine/__tests__/fake-relay-store.ts";
import { registerEngineBlocks } from "../../engine/blocks/index.ts";
import { setResolveUserDeps } from "../../engine/blocks/resolve-user.ts";
import { NO_USER_MSG } from "../../../integrations/slack-identity.ts";
import { setSlackPolicyFactoryForTest } from "../../../integrations/slack-policy.ts";
import type { RunSnapshot } from "../../engine/context.ts";
import type { EngineDeps, EngineSessionOps, EngineStepRecord } from "../../engine/deps.ts";
import type { AutomationInbox } from "../../engine/inbox.ts";
import { interpretAutomation } from "../../engine/interpreter.ts";
import type { CommunicationPolicy } from "../../../workflows/communication-policy.ts";
import { config } from "../../../config.ts";
import { SLACK_BRAIN_DEFINITION } from "../slack-brain.ts";
import type { DecisionsRequest, DecisionsResponse } from "../../../integrations/openrouter-decisions.ts";
import type { ProfileCard } from "../../profile-cards.ts";

const card = (id: string, name: string, description: string): ProfileCard => ({
  id,
  name,
  description,
  repos: [],
  skills: [],
  integrations: [],
  allowHosts: [],
  envVarNames: [],
});

const CARDS: ProfileCard[] = [
  card("p-web", "Web", "The web app"),
  card("p-infra", "Infra", "Terraform and Kubernetes"),
  card("p-d", "General", "Anything else"),
];

registerEngineBlocks();

const RUN = { runId: "autorun:slack-1:slack:Ev1", automationId: "slack-1" };

function mentionPayload(text = "<@UBOT> summarize the incident"): Record<string, unknown> {
  return {
    team_id: "T1",
    event_id: "Ev1",
    event: { type: "app_mention", channel: "C1", user: "U1", ts: "100.1", text },
  };
}

/** A follow-up MENTION in an existing thread (its own run). */
function followUpPayload(text: string, ts: string, extra: Record<string, unknown> = {}): Record<string, unknown> {
  return {
    team_id: "T1",
    event_id: `Ev-${ts}`,
    ...extra,
    event: { type: "app_mention", channel: "C1", user: "U2", ts, thread_ts: "100.1", text },
  };
}

/** One message of the thread page `list_replies` returns. */
interface ThreadMessage {
  ts: string;
  user?: string;
  bot_id?: string;
  subtype?: string;
  text: string;
}

interface Harness {
  deps: EngineDeps;
  names: string[];
  records: Array<{ path: string; attempt: number; record: EngineStepRecord }>;
  sessions: Array<{
    id: string;
    profileId: string;
    prompt: string;
    keep: boolean;
    title: string | null;
    ownerUserId: string | undefined;
    appendSystemPrompt?: string;
  }>;
  actions: Array<{ actionId: string; params: Record<string, unknown> }>;
  /** Slack user ids the identity gate was asked about. */
  resolved: string[];
  prompts: Array<{ sessionId: string; text: string }>;
  /** The session relays (contract 5): what the run installed or re-pointed. */
  relays: FakeRelayStore;
  policyCalls: string[];
  ended: string[];
  finalized: Array<{ status: string; error?: string }>;
  decisionRequests: DecisionsRequest[];
  choices: Array<{ id: string; question: string; options: string[] }>;
  /** Handles the run recorded for the thread's workstream. */
  handles: string[];
}

function harness(options: {
  eventKey?: string;
  payload?: Record<string, unknown>;
  inputs?: Record<string, unknown>;
  /** The mailbox: a `slack_choice` answer, or null for a recv timeout. */
  recv?: Array<AutomationInbox | null>;
  /** Slack user → engrams user (default: U1 → user-1; everyone else unlinked). */
  linkedUsers?: Record<string, string>;
  /** The thread as Slack holds it, for `list_replies`. Default: the mention
   * alone (a thread of mentions alone looks like this). */
  threadReplies?: ThreadMessage[];
  /** The kept session an earlier turn of this thread left behind, with its
   * relay installed under that turn's mention. `alive` = the control plane
   * still has it; false = swept away since. */
  previousSession?: { sessionId: string; runId: string; alive: boolean; mentionTs?: string };
  /** The prompt delivery fails (the session rejects it). */
  failPrompt?: boolean;
  /** How far the engine's clock moves per read (default 1 s). */
  clockStepMs?: number;
  /** The Decisions API: `connected` (default true) and the scripted answers. */
  decisions?: { connected?: boolean; replies?: DecisionsResponse[] };
  profileCards?: ProfileCard[];
} = {}): Harness {
  const names: string[] = [];
  const records: Harness["records"] = [];
  const sessions: Harness["sessions"] = [];
  const prompts: Harness["prompts"] = [];
  const relays = makeFakeRelayStore();
  const policyCalls: string[] = [];
  const ended: string[] = [];
  const finalized: Harness["finalized"] = [];
  const resolved: string[] = [];
  const actions: Array<{ actionId: string; params: Record<string, unknown> }> = [];
  const linked = options.linkedUsers ?? { U1: "user-1", U2: "user-2" };
  const recvQueue = [...(options.recv ?? [])];
  const payload = options.payload ?? mentionPayload();
  const openingEvent = (payload["event"] ?? {}) as Record<string, unknown>;
  const threadReplies: ThreadMessage[] = options.threadReplies ?? [
    { ts: String(openingEvent["ts"] ?? "100.1"), user: String(openingEvent["user"] ?? "U1"), text: String(openingEvent["text"] ?? "") },
  ];
  const runSessions: Array<{ sessionId: string; keep: boolean }> = [];
  const decisionRequests: DecisionsRequest[] = [];
  const decisionReplies = [...(options.decisions?.replies ?? [])];
  const choices: Array<{ id: string; question: string; options: string[] }> = [];
  let clock = 1_000_000;

  const policy: CommunicationPolicy = {
    async onStarted(_m, session) { policyCalls.push(`started:${session.id}`); },
    async onWorking(m) { policyCalls.push(`working:${m.ts}`); },
    async onIdle(m) { policyCalls.push(`idle:${m.ts}`); },
    async onAssistantMessage(_m, text) { policyCalls.push(`msg:${text}`); return "b1"; },
    async onUserQuestion() { policyCalls.push("question"); return "q1"; },
    async onChoice(_m, choice) {
      choices.push(choice);
      policyCalls.push(`choice:${choice.options.join("|")}`);
      return "c1";
    },
    async onAnswered() { policyCalls.push("answered"); },
    async onAsset() { policyCalls.push("asset"); },
    async onComplete(_m, _s, summary) { policyCalls.push(`complete:${summary.lastMessage ?? ""}`); },
    async onFail(_m, message) { policyCalls.push(`fail:${message}`); },
    async onNeutralClose(_m, message) { policyCalls.push(`neutral:${message}`); },
    async onDeliveryError() { policyCalls.push("delivery-error"); },
  };
  setSlackPolicyFactoryForTest(() => policy);
  setResolveUserDeps({
    resolveUser: async (_provider: string, externalUserId: string) => {
      resolved.push(externalUserId);
      return linked[externalUserId] ?? null;
    },
  });

  const previous = options.previousSession;
  if (previous) {
    // The earlier turn installed the relay on the kept session.
    relays.rows.set(previous.sessionId, {
      sessionId: previous.sessionId,
      automationId: RUN.automationId,
      provider: "slack",
      target: { team: "T1", channel: "C1", threadRoot: "100.1", user: "U1", ts: previous.mentionTs ?? "100.1", eventId: "Ev-old" },
      state: { questionTs: {}, questionProtocols: {}, assets: [], bubble: { ts: "b0", text: "earlier" }, lastAssistantText: "earlier" },
      version: 3,
    });
  }

  const snapshot: RunSnapshot = {
    definition: {
      ...SLACK_BRAIN_DEFINITION,
      trigger:
        SLACK_BRAIN_DEFINITION.trigger.kind === "integration"
          ? { ...SLACK_BRAIN_DEFINITION.trigger, connectionId: "conn-slack" }
          : SLACK_BRAIN_DEFINITION.trigger,
    },
    inputs: options.inputs ?? { channels: { C1: "prof-a" }, default_profile: "" },
    automationId: RUN.automationId,
    automationName: "Slack threads",
    version: 1,
    instanceId: "ai_thread1",
    trigger: {
      kind: "integration",
      receivedAt: "2026-08-22T10:00:00Z",
      eventKey: options.eventKey ?? "app_mention",
      deliveryKey: "slack:Ev1",
      payload,
    },
    aliases: [],
    startedAtMs: clock,
  };

  const sessionOps: EngineSessionOps = {
    async createSession(input) {
      const id = `s-${sessions.length + 1}`;
      sessions.push({
        id,
        profileId: input.profileId,
        prompt: input.prompt,
        keep: input.keep,
        title: input.title,
        ownerUserId: input.ownerUserId,
        appendSystemPrompt: input.appendSystemPrompt,
      });
      runSessions.push({ sessionId: id, keep: input.keep });
      return { sessionId: id, taskId: `t-${id}` };
    },
    async sendPrompt(sessionId, _promptId, text) {
      if (options.failPrompt) throw new Error("session rejected the prompt");
      prompts.push({ sessionId, text });
    },
    async endSession(sessionId) { ended.push(sessionId); },
    async exec() { return { exitStatus: 0, stdout: "", stderr: "" }; },
    async getSession(sessionId) {
      if (previous && previous.alive && previous.sessionId === sessionId) {
        return { found: true as const, status: "idle", lastActiveAt: "2026-08-22T09:00:00Z", lastEventAt: null };
      }
      return { found: false as const };
    },
    async writeFiles(_s, files) { return files.map((f) => ({ path: f.path, ok: true })); },
  };

  const handles: string[] = [];
  const deps: EngineDeps = {
    step: async (fn, name) => { names.push(name); return fn(); },
    recv: async () => recvQueue.shift() ?? null,
    instances: {
      async closeInstance() { return true; },
      async recordInstanceHandle(input) {
        handles.push(input.handle);
        return { kind: "recorded" as const };
      },
    },
    store: {
      async loadSnapshot() { return snapshot; },
      async markRunning() {},
      async recordStep(_r, path, attempt, record) { records.push({ path, attempt, record }); },
      async finalizeRun(_r, status, error) { finalized.push({ status, ...(error !== undefined ? { error } : {}) }); },
      async listRunSessions() { return runSessions; },
      async latestKeptInstanceSession() {
        return previous ? { sessionId: previous.sessionId, runId: previous.runId } : null;
      },
      async releaseConcurrency() { return null; },
      // D11: a session this run created is already ours; the previous run's
      // kept session (same workstream, terminal owner) adopts; anything
      // else is foreign.
      async adoptSession({ sessionId }) {
        if (runSessions.some((s) => s.sessionId === sessionId)) return "already_ours" as const;
        if (previous?.sessionId === sessionId) return "adopted" as const;
        return "foreign" as const;
      },
      async getSessionBinding() { return null; },
    },
    sessions: sessionOps,
    relays,
    announceStarted: async (sessionId, target) =>
      policy.onStarted(target, { id: sessionId, webUrl: `${config.baseUrl}/sessions/${sessionId}` }),
    clock: { nowMs: () => (clock += options.clockStepMs ?? 1000) },
    code: makeCodeBlockRuntime(),
    decisions: {
      connected: async () => options.decisions?.connected ?? true,
      async decide(request) {
        decisionRequests.push(request);
        const next = decisionReplies.shift();
        if (!next) throw new Error("fake decisions exhausted");
        return next;
      },
    },
    profileCards: async () => options.profileCards ?? CARDS,
    integrationActions: {
      async execute(input) {
        // The real contract: the connector's declared input schema, after
        // the executor's own coercion. A rendered param the catalog would
        // refuse fails HERE, not on the first live delivery.
        const { action } = await findAction(input.provider, input.actionId, { list: async () => [] });
        const params = coerceFieldValue(action.inputSchema, input.params) as Record<string, unknown>;
        const violations = validateFieldValue(action.inputSchema, params);
        if (violations.length > 0) {
          throw new Error(`invalid params for ${input.actionId}: ${violations[0]!.path} ${violations[0]!.message}`);
        }
        actions.push({ actionId: input.actionId, params });
        if (input.actionId === "list_replies") return { messages: threadReplies };
        return { ts: "9.0", channel: "C1" };
      },
    },
  };

  return { deps, names, records, sessions, prompts, relays, policyCalls, ended, finalized, resolved, actions, decisionRequests, choices, handles };
}

afterEach(() => {
  setSlackPolicyFactoryForTest(null);
  setResolveUserDeps(null);
});

const posts = (h: Harness) => h.actions.filter((a) => a.actionId === "post_message").map((a) => a.params["text"]);

describe("Slack thread brain through the interpreter", () => {
  test("(a) a mention opens the thread: one short run starts the session, relays it under the mention, posts the link, and ends", async () => {
    const h = harness();
    const result = await interpretAutomation(RUN, h.deps);

    expect(result.status).toBe("completed");
    // One session, the flagged channel's profile, KEPT: the thread outlives
    // every run. It starts with no prompt; the turn (mention text stripped
    // of the bot handle) follows once the relay is on.
    expect(h.sessions).toEqual([expect.objectContaining({ profileId: "prof-a", prompt: "", keep: true })]);
    expect(h.prompts).toEqual([{ sessionId: "s-1", text: "summarize the incident" }]);
    expect(h.sessions[0]!.title).toBe("summarize the incident");
    // The thread is the workstream's place (the handle ledger).
    expect(h.handles).toEqual(["slack:C1:100.1"]);
    // The session is the asking user's (legacy resolveUser parity): the
    // identity gate resolved U1 and the agent block passed the owner through.
    expect(h.resolved).toEqual(["U1"]);
    expect(h.sessions[0]!.ownerUserId).toBe("user-1");
    // The Slack flavor rides the session's system prompt (ADR 0060 Decision
    // 8): chat-friendly replies, mrkdwn, questions through the tool.
    expect(h.sessions[0]!.appendSystemPrompt).toContain("Conform to slack markdown");
    // The thread got the session link (legacy `onStarted`): the agent step
    // announces a session it started through the relay's policy.
    expect(posts(h)).toEqual([]);
    expect(h.policyCalls).toEqual(["started:s-1"]);
    // The thread page was read once for the fold.
    expect(h.actions.filter((a) => a.actionId === "list_replies").map((a) => a.params)).toEqual([
      { channel: "C1", threadTs: "100.1", oldest: "" },
    ]);
    // No earlier turn kept a session: a fresh one, looked up first.
    expect(h.names).toContain("step:previous:0");
    expect(h.names.indexOf("step:previous:0")).toBeLessThan(h.names.indexOf("step:agent:0"));
    expect(h.names.at(-2)).toBe("step:agent:0");
    // The relay was installed on the session under the mention: the
    // consumer renders its events from here on, with no run live.
    expect([...h.relays.rows.keys()]).toEqual(["s-1"]);
    expect(h.relays.rows.get("s-1")!.target).toMatchObject({ team: "T1", channel: "C1", threadRoot: "100.1", ts: "100.1", user: "U1", eventId: "Ev1" });
    // NOTHING waited: no wait step, no prompt beyond the opening one, no
    // session ended.
    expect(h.names.some((n) => n.endsWith(":wait"))).toBe(false);
    expect(h.prompts).toHaveLength(1);
    expect(h.ended).toEqual([]);
    expect(h.finalized).toEqual([{ status: "completed" }]);
  });

  test("(a0) the fold: a mention mid-thread brings the thread; plain replies ride the next mention as <thread context>", async () => {
    // The opening mention is a reply in a human thread rooted at 90.0; two
    // colleagues spoke before it.
    const opening = {
      team_id: "T1",
      event_id: "Ev1",
      event: { type: "app_mention", channel: "C1", user: "U1", ts: "100.1", thread_ts: "90.0", text: "<@UBOT> what do you make of this?" },
    };
    const first = harness({
      payload: opening,
      threadReplies: [
        { ts: "90.0", user: "U3", text: "deploy is red again" },
        { ts: "95.0", user: "U2", text: "same error as last week" },
        { ts: "100.1", user: "U1", text: "<@UBOT> what do you make of this?" },
      ],
    });
    expect((await interpretAutomation(RUN, first.deps)).status).toBe("completed");
    // Opening: the whole thread so far is context, the mention the directive.
    expect(first.prompts[0]!.text).toBe(
      "<thread context>\ndeploy is red again\nsame error as last week\n</thread context>\n\nwhat do you make of this?",
    );

    // After the first answer a plain reply (never an event the brain acts
    // on) lands in the thread, then a second mention: its own run, the
    // thread's kept session, and everything since the previous turn —
    // the colleague's plain reply, not the bot's own posts — then the new
    // directive.
    const second = harness({
      payload: {
        team_id: "T1",
        event_id: "Ev2",
        event: { type: "app_mention", channel: "C1", user: "U1", ts: "100.8", thread_ts: "90.0", text: "<@UBOT> does that matter?" },
      },
      previousSession: { sessionId: "s-old", runId: "r-old", alive: true },
      threadReplies: [
        { ts: "90.0", user: "U3", text: "deploy is red again" },
        { ts: "95.0", user: "U2", text: "same error as last week" },
        { ts: "100.1", user: "U1", text: "<@UBOT> what do you make of this?" },
        { ts: "100.5", bot_id: "B1", text: "Started a session — https://x/sessions/s-old" },
        { ts: "100.6", bot_id: "B1", text: "Looks like the cache mount." },
        { ts: "100.7", user: "U2", text: "we rotated the key yesterday" },
        { ts: "100.8", user: "U1", text: "<@UBOT> does that matter?" },
      ],
    });
    expect((await interpretAutomation(RUN, second.deps)).status).toBe("completed");
    expect(second.sessions).toEqual([]);
    expect(second.prompts).toEqual([
      { sessionId: "s-old", text: "<thread context>\nwe rotated the key yesterday\n</thread context>\n\ndoes that matter?" },
    ]);
  });

  test("(a5) a mention in a thread with a kept session continues it: a prompt, a re-pointed relay, no link", async () => {
    const h = harness({
      payload: followUpPayload("<@UBOT> and the root cause?", "100.2"),
      previousSession: { sessionId: "s-old", runId: "r-old", alive: true },
      threadReplies: [
        { ts: "100.1", user: "U1", text: "<@UBOT> summarize the incident" },
        { ts: "100.2", user: "U2", text: "<@UBOT> and the root cause?" },
      ],
    });
    const result = await interpretAutomation(RUN, h.deps);
    expect(result.status).toBe("completed");
    // No new session; the kept one was prompted (run_agent adopted it).
    expect(h.sessions).toEqual([]);
    expect(h.prompts).toEqual([{ sessionId: "s-old", text: "and the root cause?" }]);
    // The relay was re-pointed, not re-installed: the conversation state
    // stays, the bubble closes, ⏳/✅ land on the new mention.
    expect(h.relays.writes).toEqual(["repoint"]);
    const row = h.relays.rows.get("s-old")!;
    expect(row.target).toMatchObject({ ts: "100.2", user: "U2", eventId: "Ev-100.2" });
    expect(row.state).toMatchObject({ bubble: null, lastAssistantText: "earlier" });
    // A continued thread is silent: it carries the session link from its
    // first turn, and the conversation simply goes on. Nothing routed.
    expect(posts(h)).toEqual([]);
    expect(h.policyCalls).toEqual([]);
    expect(h.names.some((n) => n.includes("smart_route.candidates"))).toBe(false);
  });

  test("(a5') a continued session is prompted with what it has not seen: the replies since the previous turn, never its own posts, never another bot's", async () => {
    // The thread's first turn: the root mention, our "Started a session"
    // line and our answer. Then a colleague's plain reply, a post by ANOTHER
    // bot (which never moves the floor), and the mention that continues
    // the thread. The payload says who we are, so the previous turn is
    // the newest human mention of us.
    const h = harness({
      payload: {
        team_id: "T1",
        event_id: "Ev2",
        authorizations: [{ is_bot: true, user_id: "UBOT" }],
        event: { type: "app_mention", channel: "C1", user: "U1", ts: "100.1", thread_ts: "90.0", text: "<@UBOT> are you still there?" },
      },
      previousSession: { sessionId: "s-old", runId: "r-old", alive: true, mentionTs: "90.0" },
      threadReplies: [
        { ts: "90.0", user: "U1", text: "<@UBOT> hi! keep this thread open" },
        { ts: "90.5", user: "UBOT", bot_id: "B1", text: "Started a session — https://x/sessions/s-old" },
        { ts: "90.6", user: "UBOT", bot_id: "B1", text: "Hi! Got it — noted." },
        { ts: "95.0", user: "U2", text: "any update?" },
        { ts: "96.0", user: "UOTHER", bot_id: "B2", text: "CI failed on main" },
        { ts: "100.1", user: "U1", text: "<@UBOT> are you still there?" },
      ],
    });
    const result = await interpretAutomation(RUN, h.deps);
    expect(result.status).toBe("completed");
    expect(h.sessions).toEqual([]);
    expect(h.prompts).toEqual([
      { sessionId: "s-old", text: "<thread context>\nany update?\n</thread context>\n\nare you still there?" },
    ]);
  });

  test("(a5'') a human reply posted while the agent was working is not lost: the fold starts at the previous mention, not at our reply", async () => {
    const h = harness({
      payload: {
        team_id: "T1",
        event_id: "Ev2",
        authorizations: [{ is_bot: true, user_id: "UBOT" }],
        event: { type: "app_mention", channel: "C1", user: "U1", ts: "100.4", thread_ts: "100.1", text: "<@UBOT> thoughts?" },
      },
      previousSession: { sessionId: "s-old", runId: "r-old", alive: true },
      threadReplies: [
        { ts: "100.1", user: "U1", text: "<@UBOT> summarize the incident" },
        { ts: "100.2", user: "U2", text: "fwiw it started after the deploy" },
        { ts: "100.3", user: "UBOT", bot_id: "B1", text: "Here is the summary." },
        { ts: "100.4", user: "U1", text: "<@UBOT> thoughts?" },
      ],
    });
    await interpretAutomation(RUN, h.deps);
    expect(h.prompts).toEqual([
      { sessionId: "s-old", text: "<thread context>\nfwiw it started after the deploy\n</thread context>\n\nthoughts?" },
    ]);
  });

  test("(a6) a kept session that is gone (swept) starts a fresh one, with the whole thread", async () => {
    const h = harness({
      previousSession: { sessionId: "s-old", runId: "r-old", alive: false },
    });
    const result = await interpretAutomation(RUN, h.deps);
    expect(result.status).toBe("completed");
    expect(h.sessions.map((s) => s.id)).toEqual(["s-1"]);
    expect(h.prompts).toEqual([{ sessionId: "s-1", text: "summarize the incident" }]);
    expect(h.policyCalls).toEqual(["started:s-1"]);
    const previous = h.records.filter((r) => r.path === "previous").at(-1);
    expect(previous?.record.outputs).toMatchObject({ found: false, reason: "gone", session_id: "s-old" });
    // The new session got its own relay.
    expect(h.relays.rows.has("s-1")).toBe(true);
  });

  test("(a'') an author with no engrams user gets the legacy \"log in first\" message and no session", async () => {
    const h = harness({ linkedUsers: {} });
    const result = await interpretAutomation(RUN, h.deps);
    expect(result.status).toBe("filtered");
    expect(h.resolved).toEqual(["U1"]);
    expect(h.sessions).toEqual([]);
    expect(h.relays.rows.size).toBe(0);
    // The legacy message, posted into the thread by an ordinary Slack action
    // (the graph's decision, not the identity block's).
    expect(h.policyCalls).toEqual([]);
    expect(h.actions).toEqual([
      { actionId: "post_message", params: { channel: "C1", threadTs: "100.1", text: NO_USER_MSG } },
    ]);
    expect(h.names).toContain("step:identity:0");
    expect(h.names).toContain("step:unlinked.login_notice:0");
    expect(h.names).not.toContain("step:agent:0");
    expect(h.finalized).toHaveLength(1);
    expect(h.finalized[0]).toMatchObject({ status: "filtered" });
  });

  test("(a''') an unlinked colleague mentioning the bot in a live thread leaves the thread's relay alone", async () => {
    const h = harness({
      linkedUsers: { U1: "user-1" },
      payload: followUpPayload("<@UBOT> me too", "100.2"),
      previousSession: { sessionId: "s-old", runId: "r-old", alive: true },
    });
    const result = await interpretAutomation(RUN, h.deps);
    expect(result.status).toBe("filtered");
    expect(h.relays.rows.has("s-old")).toBe(true);
    expect(h.relays.writes).toEqual([]);
    expect(h.policyCalls).toEqual([]);
  });

  test("(b) a top-level channel message (no thread_ts) is filtered, never a session, and nothing is posted", async () => {
    const h = harness({
      eventKey: "message",
      payload: { team_id: "T1", event_id: "Ev9", event: { type: "message", channel: "C1", user: "U2", ts: "5.0", text: "hi" } },
    });
    const result = await interpretAutomation(RUN, h.deps);
    expect(result.status).toBe("filtered");
    expect(h.sessions).toEqual([]);
    expect(h.policyCalls).toEqual([]);
    expect(h.actions).toEqual([]);
  });

  test("(c) an unflagged channel with no default profile is filtered", async () => {
    const h = harness({ payload: { ...mentionPayload(), event: { ...(mentionPayload()["event"] as object), channel: "C-other" } } });
    const result = await interpretAutomation(RUN, h.deps);
    expect(result.status).toBe("filtered");
    expect(h.sessions).toEqual([]);
  });

  test("(d') a bare `@bot` is not a turn: the run ends filtered, no prompt, no routing, no failure, and the thread's relay stays", async () => {
    const h = harness({
      payload: followUpPayload("<@UBOT>", "100.2"),
      previousSession: { sessionId: "s-old", runId: "r-old", alive: true },
      threadReplies: [
        { ts: "100.1", user: "U1", text: "<@UBOT> summarize the incident" },
        { ts: "100.2", user: "U2", text: "<@UBOT>" },
      ],
    });
    const result = await interpretAutomation(RUN, h.deps);
    expect(result.status).toBe("filtered");
    expect(h.prompts).toEqual([]);
    expect(h.sessions).toEqual([]);
    expect(h.names).not.toContain("step:agent:0");
    expect(h.names.some((n) => n.includes("smart_route"))).toBe(false);
    expect(h.relays.writes).toEqual([]);
    expect(h.relays.rows.get("s-old")!.target).toMatchObject({ ts: "100.1" });
    expect(h.policyCalls).toEqual([]);
    expect(h.finalized).toMatchObject([{ status: "filtered" }]);
  });

  test("(d'') a bot-authored app_mention is rejected at admission", async () => {
    const h = harness({
      payload: {
        team_id: "T1",
        event_id: "Ev-bot",
        event: { type: "app_mention", channel: "C1", bot_id: "B1", user: "UBOT2", ts: "100.2", thread_ts: "100.1", text: "<@UBOT> look at this" },
      },
      previousSession: { sessionId: "s-old", runId: "r-old", alive: true },
    });
    const result = await interpretAutomation(RUN, h.deps);
    expect(result.status).toBe("filtered");
    expect(h.prompts).toEqual([]);
    expect(h.relays.writes).toEqual([]);
  });

  test("(a7) the fold starts at the mention the relay points at, not at a mention that was never a turn", async () => {
    // Between the previous turn (100.1) and this mention, an unlinked
    // colleague mentioned the bot (100.2): admission dropped it, so it was
    // no turn and the session never saw it. The fold must start after
    // 100.1 and carry 100.2 as context.
    const h = harness({
      payload: {
        team_id: "T1",
        event_id: "Ev2",
        authorizations: [{ is_bot: true, user_id: "UBOT" }],
        event: { type: "app_mention", channel: "C1", user: "U1", ts: "100.4", thread_ts: "100.1", text: "<@UBOT> thoughts?" },
      },
      previousSession: { sessionId: "s-old", runId: "r-old", alive: true, mentionTs: "100.1" },
      threadReplies: [
        { ts: "100.1", user: "U1", text: "<@UBOT> summarize the incident" },
        { ts: "100.2", user: "U9", text: "<@UBOT> is this the cache again?" },
        { ts: "100.3", user: "UBOT", bot_id: "B1", text: "Here is the summary." },
        { ts: "100.4", user: "U1", text: "<@UBOT> thoughts?" },
      ],
    });
    await interpretAutomation(RUN, h.deps);
    expect(h.prompts).toEqual([
      { sessionId: "s-old", text: "<thread context>\nis this the cache again?\n</thread context>\n\nthoughts?" },
    ]);
    // The page read is bounded to what came after the previous turn.
    expect(h.actions.find((a) => a.actionId === "list_replies")?.params).toMatchObject({ oldest: "100.1" });
  });

  test("(e) a turn whose prompt cannot be delivered fails the run and tells the thread why; the relay stays", async () => {
    const h = harness({
      payload: followUpPayload("<@UBOT> and now?", "100.2"),
      previousSession: { sessionId: "s-old", runId: "r-old", alive: true },
      threadReplies: [
        { ts: "100.1", user: "U1", text: "<@UBOT> summarize the incident" },
        { ts: "100.2", user: "U2", text: "<@UBOT> and now?" },
      ],
      failPrompt: true,
    });
    const result = await interpretAutomation(RUN, h.deps);
    expect(result.status).toBe("failed");
    expect(posts(h)).toEqual([`❌ This turn failed: block "agent": block_threw: session rejected the prompt`]);
    // The session is fine and keeps its relay: the next mention is a new
    // turn on it. Nothing closes.
    expect(h.policyCalls.some((c) => c.startsWith("fail:"))).toBe(false);
    expect(h.relays.rows.has("s-old")).toBe(true);
    expect(h.ended).toEqual([]);
  });
});

describe("Slack thread brain — smart routing", () => {
  const SMART = {
    channels: { C9: "p-web" },
    default_profile: "p-d",
    routing: "smart",
    smart_profiles: [],
    smart_min_confidence: 0.8,
    ask_timeout: 600,
  };
  const QUESTION = "Which profile should handle this thread?";
  const ASK_ID = `choice:${RUN.runId}:has_previous.smart_route.unsure.ask`;
  const answer = (profile: string, confidence: number, wants = 0.02): DecisionsResponse => ({
    model: "typesafe/jev-1.13-20260917",
    answers: {
      profile: {
        type: "choice",
        choice: profile,
        confidence,
        // The rest of the mass on the other profiles, Web before General.
        probabilities: {
          Infra: profile === "Infra" ? confidence : 0.05,
          Web: profile === "Web" ? confidence : (1 - confidence) * 0.5,
          General: profile === "General" ? confidence : (1 - confidence) * 0.4,
        },
      },
      wants_choice: { type: "noul", noul: wants },
    },
    usage: { input_tokens: 120, output_tokens: 0, cost: 0.000005 },
  });

  test("(r1) a confident pick routes the new session without asking", async () => {
    const h = harness({
      inputs: SMART,
      decisions: { replies: [answer("Infra", 0.91)] },
    });
    const result = await interpretAutomation(RUN, h.deps);
    expect(result.status).toBe("completed");
    // The model saw the opening fold and the profile cards, in one call.
    expect(h.decisionRequests).toHaveLength(1);
    expect(h.decisionRequests[0]!.state).toEqual({ message: "summarize the incident" });
    expect(Object.keys(h.decisionRequests[0]!.questions)).toEqual(["profile", "wants_choice"]);
    expect(h.choices).toEqual([]);
    expect(h.sessions).toEqual([expect.objectContaining({ profileId: "p-infra" })]);
    // The verdict is a line in the thread, then the session's link.
    expect(posts(h)).toEqual(["Profile: *Infra* (routed by Jev, 91% confident)"]);
    expect(h.policyCalls).toEqual(["started:s-1"]);
  });

  test("(r2) an unsure pick asks in the thread; the click decides and the card shows it", async () => {
    const h = harness({
      inputs: SMART,
      decisions: { replies: [answer("Infra", 0.41)] },
      recv: [
        { kind: "signal", name: "slack_answer", payload: { toolCallId: ASK_ID, answers: { [QUESTION]: ["Web"] } } },
      ],
    });
    const result = await interpretAutomation(RUN, h.deps);
    expect(result.status).toBe("completed");
    // The card lists the model's ranking plus the default, by name.
    expect(h.choices).toEqual([{ id: ASK_ID, question: QUESTION, options: ["Infra", "Web", "General"] }]);
    expect(h.sessions).toEqual([expect.objectContaining({ profileId: "p-web" })]);
    expect(h.actions.filter((a) => a.actionId === "update_message").map((a) => a.params)).toEqual([
      { channel: "C1", ts: "c1", text: "Profile: *Web* (picked in the thread)" },
    ]);
    // The card carries the verdict; no second line.
    expect(posts(h)).toEqual([]);
  });

  test("(r2b) a click on a label the card never offered is ignored", async () => {
    const h = harness({
      inputs: SMART,
      decisions: { replies: [answer("Infra", 0.41)] },
      recv: [
        { kind: "signal", name: "slack_answer", payload: { toolCallId: ASK_ID, answers: { [QUESTION]: ["Mobile"] } } },
        null,
      ],
      clockStepMs: 400_000,
    });
    await interpretAutomation(RUN, h.deps);
    expect(h.sessions).toEqual([expect.objectContaining({ profileId: "p-infra" })]);
  });

  test("(r3) no click before the timeout: the model's pick, and the run goes on", async () => {
    const h = harness({
      inputs: SMART,
      decisions: { replies: [answer("Infra", 0.41)] },
      recv: [null],
      // One empty recv passes the 600 s card deadline.
      clockStepMs: 400_000,
    });
    const result = await interpretAutomation(RUN, h.deps);
    expect(result.status).toBe("completed");
    expect(h.sessions).toEqual([expect.objectContaining({ profileId: "p-infra" })]);
    expect(h.actions.find((a) => a.actionId === "update_message")!.params["text"]).toBe(
      "Profile: *Infra* (no answer, so routed by Jev, 41% confident)",
    );
  });

  test("(r4) a person who asks to choose gets the card even when the model is sure", async () => {
    const h = harness({
      inputs: SMART,
      decisions: { replies: [answer("Infra", 0.95, 0.9)] },
      recv: [
        { kind: "signal", name: "slack_answer", payload: { toolCallId: ASK_ID, answers: { [QUESTION]: ["General"] } } },
      ],
    });
    await interpretAutomation(RUN, h.deps);
    expect(h.choices).toHaveLength(1);
    expect(h.sessions).toEqual([expect.objectContaining({ profileId: "p-d" })]);
  });

  test("(r5) a channel override wins: the model is never asked", async () => {
    const h = harness({ inputs: { ...SMART, channels: { C1: "p-web" } } });
    await interpretAutomation(RUN, h.deps);
    expect(h.decisionRequests).toEqual([]);
    expect(h.sessions).toEqual([expect.objectContaining({ profileId: "p-web" })]);
    expect(posts(h)).toEqual([]);
    expect(h.policyCalls).toEqual(["started:s-1"]);
  });

  test("(r6) without an OpenRouter key smart routing is default routing", async () => {
    const h = harness({ inputs: SMART, decisions: { connected: false } });
    await interpretAutomation(RUN, h.deps);
    expect(h.decisionRequests).toEqual([]);
    expect(h.choices).toEqual([]);
    expect(h.sessions).toEqual([expect.objectContaining({ profileId: "p-d" })]);
    // No key: no verdict line, only the link.
    expect(posts(h)).toEqual([]);
    expect(h.policyCalls).toEqual(["started:s-1"]);
  });

  test("(r7) no key and no default: the mention is not answered, as before", async () => {
    const h = harness({
      inputs: { ...SMART, default_profile: "" },
      decisions: { connected: false },
    });
    const result = await interpretAutomation(RUN, h.deps);
    expect(result.status).toBe("filtered");
    expect(h.sessions).toEqual([]);
    expect(h.choices).toEqual([]);
  });

  test("(r9) the candidates are the org's chosen profiles only", async () => {
    const h = harness({
      inputs: { ...SMART, smart_profiles: ["p-web", "p-infra"] },
      decisions: { replies: [answer("Infra", 0.91)] },
    });
    await interpretAutomation(RUN, h.deps);
    const profile = h.decisionRequests[0]!.questions["profile"] as { criteria: Record<string, unknown> };
    expect(Object.keys(profile.criteria)).toEqual(["Web", "Infra"]);
  });

  test("(r10) default routing adds only one condition step to a new thread", async () => {
    const h = harness();
    await interpretAutomation(RUN, h.deps);
    const routing = h.names.filter((n) => n.includes("smart_route"));
    // The decision and its ledger row; no model call, no profile list.
    expect(routing).toEqual(["step:has_previous.smart_route.__cond__:0", "step:has_previous.smart_route:0:ledger"]);
  });

  test("(r11) every chosen profile gone: no model call, the default profile serves", async () => {
    const h = harness({ inputs: { ...SMART, smart_profiles: ["p-archived"] } });
    const result = await interpretAutomation(RUN, h.deps);
    expect(result.status).toBe("completed");
    expect(h.decisionRequests).toEqual([]);
    expect(h.sessions).toEqual([expect.objectContaining({ profileId: "p-d" })]);
  });

  test("(r12) a long profile name is cut to fit a Slack button", async () => {
    const long = "W".repeat(120);
    const h = harness({
      inputs: SMART,
      profileCards: [
        { ...CARDS[0]!, name: long },
        CARDS[1]!,
        CARDS[2]!,
      ],
      decisions: {
        replies: [
          {
            model: "typesafe/jev-1.13-20260917",
            answers: {
              profile: { type: "choice", choice: "Infra", confidence: 0.4, probabilities: { Infra: 0.4, [long]: 0.35, General: 0.25 } },
              wants_choice: { type: "noul", noul: 0 },
            },
            usage: { input_tokens: 1, output_tokens: 0 },
          },
        ],
      },
      recv: [null],
      clockStepMs: 400_000,
    });
    const result = await interpretAutomation(RUN, h.deps);
    expect(result.status).toBe("completed");
    expect(h.choices[0]!.options).toEqual(["Infra", "W".repeat(75), "General"]);
  });

  test("(r8) a continued thread is never re-routed", async () => {
    const h = harness({
      inputs: SMART,
      payload: followUpPayload("<@UBOT> and now?", "100.2"),
      previousSession: { sessionId: "s-old", runId: "r-old", alive: true },
      threadReplies: [
        { ts: "100.1", user: "U1", text: "<@UBOT> summarize the incident" },
        { ts: "100.2", user: "U2", text: "<@UBOT> and now?" },
      ],
    });
    await interpretAutomation(RUN, h.deps);
    expect(h.decisionRequests).toEqual([]);
    expect(h.sessions).toEqual([]);
    expect(h.prompts).toEqual([{ sessionId: "s-old", text: "and now?" }]);
  });
});
