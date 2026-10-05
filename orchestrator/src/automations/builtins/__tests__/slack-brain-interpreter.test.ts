/** The Slack thread brain driven through the REAL interpreter (ADR 0119 phase
 * 4.6): scripted recv, fake session ops, a recording relay policy. These
 * cases are the Slack parity checklist the window is judged against.
 */

import { afterEach, describe, expect, test } from "bun:test";

import type { CuratedEvent } from "../../../control-plane/session-events.ts";
import { coerceFieldValue, validateFieldValue } from "../../../connectors/field-schema.ts";
import { findAction } from "../../actions/execute.ts";
import { makeCodeBlockRuntime } from "../../code/runtime.ts";
import { makeReplayRunner, type ReplayRunner, type ReplayScript } from "../../engine/__tests__/replay-step.ts";
import { registerEngineBlocks } from "../../engine/blocks/index.ts";
import { setSlackRelayDeps } from "../../engine/blocks/relay.ts";
import { setResolveUserDeps } from "../../engine/blocks/resolve-user.ts";
import { NO_USER_MSG } from "../../../integrations/slack-identity.ts";
import type { RunSnapshot } from "../../engine/context.ts";
import type { EngineDeps, EngineSessionOps, EngineStepRecord } from "../../engine/deps.ts";
import type { AutomationInbox } from "../../engine/inbox.ts";
import { interpretAutomation } from "../../engine/interpreter.ts";
import type { CommunicationPolicy } from "../../../workflows/communication-policy.ts";
import { config } from "../../../config.ts";
import { SLACK_BRAIN_DEFINITION } from "../slack-brain.ts";

registerEngineBlocks();

const RUN = { runId: "autorun:slack-1:slack:Ev1", automationId: "slack-1" };

function mentionPayload(text = "<@UBOT> summarize the incident"): Record<string, unknown> {
  return {
    team_id: "T1",
    event_id: "Ev1",
    event: { type: "app_mention", channel: "C1", user: "U1", ts: "100.1", text },
  };
}

function replyPayload(text: string, ts: string): Record<string, unknown> {
  return {
    team_id: "T1",
    event_id: `Ev-${ts}`,
    event: { type: "app_mention", channel: "C1", user: "U2", ts, thread_ts: "100.1", text },
  };
}

/** A follow-up MENTION in the thread, delivered into the run's mailbox
 * (the same workstream key → policy: join). Only a mention is a turn. */
function joined(text: string, ts: string): AutomationInbox {
  return {
    kind: "event",
    eventKey: "app_mention",
    deliveryKey: `slack:Ev-${ts}`,
    payload: replyPayload(text, ts),
    receivedAt: "2026-08-22T10:00:05Z",
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

/** A curated session event as the automation consumer forwards it. */
function curated(n: number, kind: string, payload: unknown, sessionId = "s-1"): AutomationInbox {
  const event: CuratedEvent = { idx: BigInt(n), kind, payloadJson: JSON.stringify(payload) };
  return { kind: "session_event", sessionId, event };
}

interface Harness {
  deps: EngineDeps;
  /** Set when the harness was built with `replay`. */
  runner: ReplayRunner | null;
  names: string[];
  records: Array<{ path: string; attempt: number; record: EngineStepRecord }>;
  sessions: Array<{
    id: string;
    profileId: string;
    prompt: string;
    keep: boolean;
    title: string | null;
    ownerUserId: string | undefined;
  }>;
  actions: Array<{ actionId: string; params: Record<string, unknown> }>;
  /** Workstream closes the finalize hook requested (ADR 0120). */
  closed: Array<{ instanceId: string; reason?: string }>;
  /** Slack user ids the identity gate was asked about. */
  resolved: string[];
  prompts: Array<{ sessionId: string; text: string }>;
  relayFlags: Array<{ sessionId: string; relay: boolean }>;
  policyCalls: string[];
  ended: string[];
  finalized: Array<{ status: string; error?: string }>;
}

function harness(options: {
  eventKey?: string;
  payload?: Record<string, unknown>;
  inputs?: Record<string, unknown>;
  /** null = a recv timeout (the wait's deadline). */
  recv?: Array<AutomationInbox | null>;
  /** Drive step + recv through the DBOS-recovery simulator instead
   * (`"crash"` entries kill the pod; `runner.restart()` brings it back). */
  replay?: ReplayScript;
  /** Slack user → engrams user (default: U1 → user-1; everyone else unlinked). */
  linkedUsers?: Record<string, string>;
  /** Make every relay-step ledger write throw (the row is observability). */
  failRelayLedger?: boolean;
  /** The thread as Slack holds it, for `list_replies`. Default: the opening
   * mention as the root plus every mention the test delivers through `recv`
   * (what a thread of mentions alone looks like). */
  threadReplies?: ThreadMessage[];
  /** The kept session an earlier run of this workstream left behind (the
   * thread went quiet and was mentioned again). `alive` = the control plane
   * still has it; false = swept away since. */
  previousSession?: { sessionId: string; runId: string; alive: boolean };
  /** How far the engine's clock moves per read (default 1 s). A long turn
   * is simulated by big steps: a wait's slice deadline then passes in a
   * couple of empty recvs. */
  clockStepMs?: number;
}): Harness {
  const runner = options.replay ? makeReplayRunner(options.replay) : null;
  const names: string[] = runner ? runner.names : [];
  const records: Harness["records"] = [];
  const sessions: Harness["sessions"] = [];
  const prompts: Harness["prompts"] = [];
  const relayFlags: Harness["relayFlags"] = [];
  const policyCalls: string[] = [];
  const ended: string[] = [];
  const finalized: Harness["finalized"] = [];
  const resolved: string[] = [];
  const actions: Array<{ actionId: string; params: Record<string, unknown> }> = [];
  const closed: Array<{ instanceId: string; reason?: string }> = [];
  const linked = options.linkedUsers ?? { U1: "user-1", U2: "user-2" };
  const recvQueue = [...(options.recv ?? [])];
  const openingEvent = ((options.payload ?? mentionPayload())["event"] ?? {}) as Record<string, unknown>;
  const threadReplies: ThreadMessage[] = options.threadReplies ?? [
    { ts: String(openingEvent["ts"] ?? "100.1"), user: String(openingEvent["user"] ?? "U1"), text: String(openingEvent["text"] ?? "") },
    ...(options.recv ?? []).flatMap((m): ThreadMessage[] => {
      if (!m || m.kind !== "event") return [];
      const ev = (m.payload["event"] ?? {}) as Record<string, unknown>;
      return [{
        ts: String(ev["ts"] ?? ""),
        user: String(ev["user"] ?? ""),
        ...(ev["bot_id"] !== undefined ? { bot_id: String(ev["bot_id"]) } : {}),
        text: String(ev["text"] ?? ""),
      }];
    }),
  ];
  const runSessions: Array<{ sessionId: string; keep: boolean }> = [];
  let clock = 1_000_000;

  const policy: CommunicationPolicy = {
    async onStarted() { policyCalls.push("started"); },
    async onWorking(m) { policyCalls.push(`working:${m.ts}`); },
    async onIdle(m) { policyCalls.push(`idle:${m.ts}`); },
    async onAssistantMessage(_m, text) { policyCalls.push(`msg:${text}`); return "b1"; },
    async onUserQuestion() { policyCalls.push("question"); return "q1"; },
    async onAnswered() { policyCalls.push("answered"); },
    async onAsset() { policyCalls.push("asset"); },
    async onComplete(_m, _s, summary) { policyCalls.push(`complete:${summary.lastMessage ?? ""}`); },
    async onFail(_m, message) { policyCalls.push(`fail:${message}`); },
    async onNeutralClose(_m, message) { policyCalls.push(`neutral:${message}`); },
    async onDeliveryError() { policyCalls.push("delivery-error"); },
  };
  setSlackRelayDeps({
    policy: () => policy,
    completeToolCall: async () => {},
    sessionWebUrl: (id: string) => `https://engrams.test/sessions/${id}`,
  });
  setResolveUserDeps({
    resolveUser: async (_provider: string, externalUserId: string) => {
      resolved.push(externalUserId);
      return linked[externalUserId] ?? null;
    },
  });

  const snapshot: RunSnapshot = {
    definition: {
      ...SLACK_BRAIN_DEFINITION,
      trigger:
        SLACK_BRAIN_DEFINITION.trigger.kind === "integration"
          ? { ...SLACK_BRAIN_DEFINITION.trigger, connectionId: "conn-slack" }
          : SLACK_BRAIN_DEFINITION.trigger,
    },
    inputs: options.inputs ?? { channels: { C1: "prof-a" }, default_profile: "", idle_timeout: 600, max_turns: 5 },
    automationId: RUN.automationId,
    automationName: "Slack thread brain",
    version: 1,
    instanceId: "ai_thread1",
    trigger: {
      kind: "integration",
      receivedAt: "2026-08-22T10:00:00Z",
      eventKey: options.eventKey ?? "app_mention",
      deliveryKey: "slack:Ev1",
      payload: options.payload ?? mentionPayload(),
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
      });
      runSessions.push({ sessionId: id, keep: input.keep });
      return { sessionId: id, taskId: `t-${id}` };
    },
    async setSessionRelay(sessionId, relay) { relayFlags.push({ sessionId, relay }); },
    async sendPrompt(sessionId, _promptId, text) { prompts.push({ sessionId, text }); },
    async endSession(sessionId) { ended.push(sessionId); },
    async exec() { return { exitStatus: 0, stdout: "", stderr: "" }; },
    async getSession(sessionId) {
      const prev = options.previousSession;
      if (prev && prev.alive && prev.sessionId === sessionId) {
        return { found: true as const, status: "idle", lastActiveAt: "2026-08-22T09:00:00Z", lastEventAt: null };
      }
      return { found: false as const };
    },
    async writeFiles(_s, files) { return files.map((f) => ({ path: f.path, ok: true })); },
  };

  const deps: EngineDeps = {
    step: runner ? runner.step : async (fn, name) => { names.push(name); return fn(); },
    recv: runner ? runner.recv : async () => recvQueue.shift() ?? null,
    store: {
      async loadSnapshot() { return snapshot; },
      async markRunning() {},
      async recordStep(_r, path, attempt, record) {
        if (options.failRelayLedger && path.endsWith(".__relay__")) throw new Error("ledger down");
        records.push({ path, attempt, record });
      },
      async finalizeRun(_r, status, error) { finalized.push({ status, ...(error !== undefined ? { error } : {}) }); },
      async listRunSessions() { return runSessions; },
      async latestKeptInstanceSession() {
        return options.previousSession
          ? { sessionId: options.previousSession.sessionId, runId: options.previousSession.runId }
          : null;
      },
      async releaseConcurrency() { return null; },
      // D11: a session this run created is already ours; the previous run's
      // kept session (same workstream, terminal owner) adopts; anything
      // else is foreign.
      async adoptSession({ sessionId }) {
        if (runSessions.some((s) => s.sessionId === sessionId)) return "already_ours" as const;
        if (options.previousSession?.sessionId === sessionId) return "adopted" as const;
        return "foreign" as const;
      },
      async getSessionBinding() { return null; },
    },
    sessions: sessionOps,
    clock: { nowMs: () => (clock += options.clockStepMs ?? 1000) },
    code: makeCodeBlockRuntime(),
    instances: {
      async closeInstance(input) {
        closed.push(input);
        return true;
      },
    },
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
        if (input.actionId === "list_replies") {
          // Slack's `oldest` is inclusive: the page starts AT that ts.
          const oldest = typeof params["oldest"] === "string" ? Number.parseFloat(params["oldest"]) : null;
          return {
            messages: threadReplies.filter((m) => oldest === null || Number.parseFloat(m.ts) >= oldest),
          };
        }
        return { ts: "9.0", channel: "C1" };
      },
    },
  };

  return { deps, runner, names, records, sessions, prompts, relayFlags, policyCalls, ended, finalized, resolved, actions, closed };
}

afterEach(() => {
  setSlackRelayDeps(null);
  setResolveUserDeps(null);
});

describe("Slack thread brain through the interpreter", () => {
  test("(a) mention → session + relay → first turn idle → joined follow-up → second prompt → idle-timeout end → ✅ recap", async () => {
    const h = harness({
      recv: [
        // The first turn (initial prompt) ends.
        { kind: "session_idle", sessionId: "s-1" },
        // A follow-up reply joins the active run's mailbox.
        joined("<@UBOT> and what about the root cause?", "100.2"),
        // The second turn ends.
        { kind: "session_idle", sessionId: "s-1" },
        // Nothing more: the idle wait times out → the thread went quiet.
        null,
        null,
      ],
    });

    const result = await interpretAutomation(RUN, h.deps);

    expect(result.status).toBe("completed");
    // One session, the flagged channel's profile, mention text stripped of the bot handle, KEPT.
    expect(h.sessions).toEqual([
      expect.objectContaining({ profileId: "prof-a", prompt: "summarize the incident", keep: true }),
    ]);
    expect(h.sessions[0]!.title).toBe("summarize the incident");
    // The session is the asking user's (legacy resolveUser parity): the
    // identity gate resolved U1 and create_session passed the owner through.
    expect(h.resolved).toEqual(["U1"]);
    expect(h.sessions[0]!.ownerUserId).toBe("user-1");
    // The thread got the session link first (legacy `onStarted`), by an
    // ordinary Slack action templated on create_session's `web_url`.
    expect(h.actions.filter((a) => a.actionId === "post_message")).toEqual([
      {
        actionId: "post_message",
        params: { channel: "C1", threadTs: "100.1", text: `Started a session — ${config.baseUrl}/sessions/s-1` },
      },
    ]);
    // The thread page was read once for the opening fold (whole thread) and
    // once per turn, from the previous mention on (Slack's inclusive
    // `oldest`), so a long thread costs one small read per turn.
    expect(h.actions.filter((a) => a.actionId === "list_replies").map((a) => a.params)).toEqual([
      { channel: "C1", threadTs: "100.1" },
      { channel: "C1", threadTs: "100.1", oldest: "100.1" },
    ]);
    expect(h.names.indexOf("step:has_previous.started:0")).toBeLessThan(h.names.indexOf("step:relay:0"));
    // No earlier run of this workstream kept a session: a fresh one.
    expect(h.names).toContain("step:previous:0");
    // The relay was installed on that session (consumer will forward curated events).
    expect(h.relayFlags).toEqual([{ sessionId: "s-1", relay: true }]);
    // The follow-up became the second prompt, mention stripped.
    expect(h.prompts).toEqual([{ sessionId: "s-1", text: "and what about the root cause?" }]);
    // Sessions are kept: nothing ended.
    expect(h.ended).toEqual([]);
    // The loop ran one real turn, then its idle wait timed out and the
    // `until` exited it (two iterations: the turn, then the quiet one).
    expect(h.names.filter((n) => /^step:thread\[\d+\]\.__until__:0$/.test(n)).length).toBe(2);
    expect(h.names).toContain("step:thread[1].next:0:wait");
    expect(h.names).not.toContain("step:thread[2].next:0");
    // The idle exit is a pause, not an end: the workstream stays open (the
    // next mention binds to it and resumes the kept session) and nothing is
    // posted — the ✅ on the last mention already says the turn is done.
    expect(h.closed).toEqual([]);
    expect(h.policyCalls.filter((c) => c.startsWith("complete:"))).toEqual([]);
    expect(h.finalized).toEqual([{ status: "completed" }]);
  });

  test("(a0) the legacy fold: a mention mid-thread brings the thread; plain replies ride the next mention as <thread context>", async () => {
    // The opening mention is a reply in a human thread rooted at 90.0; two
    // colleagues spoke before it. After the first answer a plain reply (never
    // an event the brain acts on) lands in the thread, then a second mention.
    const opening = {
      team_id: "T1",
      event_id: "Ev1",
      event: { type: "app_mention", channel: "C1", user: "U1", ts: "100.1", thread_ts: "90.0", text: "<@UBOT> what do you make of this?" },
    };
    const h = harness({
      payload: opening,
      threadReplies: [
        { ts: "90.0", user: "U3", text: "deploy is red again" },
        { ts: "95.0", user: "U2", text: "same error as last week" },
        { ts: "100.1", user: "U1", text: "<@UBOT> what do you make of this?" },
        { ts: "100.5", bot_id: "B1", text: "Started a session — https://x/sessions/s-1" },
        { ts: "100.6", bot_id: "B1", text: "Looks like the cache mount." },
        { ts: "100.7", user: "U2", text: "we rotated the key yesterday" },
        { ts: "100.8", user: "U1", text: "<@UBOT> does that matter?" },
      ],
      recv: [
        { kind: "session_idle", sessionId: "s-1" },
        joined("<@UBOT> does that matter?", "100.8"),
        { kind: "session_idle", sessionId: "s-1" },
        null,
        null,
      ],
    });
    const result = await interpretAutomation(RUN, h.deps);
    expect(result.status).toBe("completed");
    // Opening: the whole thread so far is context, the mention the directive.
    expect(h.sessions[0]!.prompt).toBe(
      "<thread context>\ndeploy is red again\nsame error as last week\n</thread context>\n\nwhat do you make of this?",
    );
    // Follow-up: everything since the opening mention — the colleague's plain
    // reply, not the bot's own posts — then the new directive.
    expect(h.prompts.map((p) => p.text)).toEqual([
      "<thread context>\nwe rotated the key yesterday\n</thread context>\n\ndoes that matter?",
    ]);
  });

  test("(a5) a mention in a thread whose run went quiet resumes the kept session in a new run of the same workstream", async () => {
    // The earlier run (r-old) answered this thread, went idle, and ended with
    // the workstream open; its session s-old is kept. This mention is a new
    // run in the same workstream.
    const h = harness({
      previousSession: { sessionId: "s-old", runId: "r-old", alive: true },
      recv: [
        // The resumed session's run (the opening fold as its prompt) ends.
        curated(1, "run_started", {}, "s-old"),
        curated(2, "agent_message", { role: "assistant", text: "picking up where we left off" }, "s-old"),
        curated(3, "run_completed", { ok: true }, "s-old"),
        // The resumed session's turn ends (the Nth idle ↔ the Nth prompt,
        // counted per session within THIS run).
        { kind: "session_idle", sessionId: "s-old" },
        null,
        null,
      ],
    });
    const result = await interpretAutomation(RUN, h.deps);
    expect(result.status).toBe("completed");
    // No new session; the kept one was prompted (send_prompt adopted it).
    expect(h.sessions).toEqual([]);
    expect(h.prompts).toEqual([{ sessionId: "s-old", text: "summarize the incident" }]);
    expect(h.relayFlags).toEqual([{ sessionId: "s-old", relay: true }]);
    expect(h.names).toContain("step:opening_turn.resume_turn:0");
    expect(h.names).not.toContain("step:has_previous.session:0");
    expect(h.names).not.toContain("step:has_previous.started:0");
    // A resume is silent: the thread carries the session link from its first
    // run, and the conversation simply continues.
    expect(h.actions.filter((a) => a.actionId === "post_message")).toEqual([]);
    expect(h.policyCalls).toContain("msg:picking up where we left off");
  });

  test("(a5') a resumed session is prompted with what it has not seen: the replies after our last answer, never its own earlier turns", async () => {
    // The thread's first run: the root mention, our "Started a session" line
    // and our answer. Then a colleague's plain reply, a post by ANOTHER bot
    // (which never moves the floor), and the mention that resumes the thread.
    const mention = {
      team_id: "T1",
      event_id: "Ev2",
      authorizations: [{ is_bot: true, user_id: "UBOT" }],
      event: { type: "app_mention", channel: "C1", user: "U1", ts: "100.1", thread_ts: "90.0", text: "<@UBOT> are you still there?" },
    };
    const h = harness({
      payload: mention,
      previousSession: { sessionId: "s-old", runId: "r-old", alive: true },
      threadReplies: [
        { ts: "90.0", user: "U1", text: "<@UBOT> hi! keep this thread open" },
        { ts: "90.5", user: "UBOT", bot_id: "B1", text: "Started a session — https://x/sessions/s-old" },
        { ts: "90.6", user: "UBOT", bot_id: "B1", text: "Hi! Got it — noted." },
        { ts: "95.0", user: "U2", text: "any update?" },
        { ts: "96.0", user: "UOTHER", bot_id: "B2", text: "CI failed on main" },
        { ts: "100.1", user: "U1", text: "<@UBOT> are you still there?" },
      ],
      recv: [{ kind: "session_idle", sessionId: "s-old" }, null, null],
    });
    const result = await interpretAutomation(RUN, h.deps);
    expect(result.status).toBe("completed");
    expect(h.sessions).toEqual([]);
    expect(h.prompts).toEqual([
      { sessionId: "s-old", text: "<thread context>\nany update?\n</thread context>\n\nare you still there?" },
    ]);
  });

  test("(a7) a turn has no deadline: a task longer than one wait slice is waited for slice by slice, then ✅", async () => {
    // The engine's clock moves 10 h per read. The first slice (24 h) expires
    // after a few empty recvs; that is a normal outcome, the loop waits
    // again, and the session's idle arrives in the second slice. No failure,
    // no second prompt, the relay still installed.
    const h = harness({
      clockStepMs: 10 * 3600 * 1000,
      recv: [null, null, null, { kind: "session_idle", sessionId: "s-1" }, null, null],
    });
    const result = await interpretAutomation(RUN, h.deps);
    expect(result.status).toBe("completed");
    expect(h.names).toContain("step:opening_turn.first_turn[0].first_turn_slice:0:wait");
    expect(h.names).toContain("step:opening_turn.first_turn[1].first_turn_slice:0:wait");
    expect(h.names).not.toContain("step:opening_turn.first_turn[2].first_turn_slice:0");
    expect(h.prompts).toEqual([]);
    expect(h.policyCalls.some((c) => c.startsWith("fail:"))).toBe(false);
    expect(h.finalized).toEqual([{ status: "completed" }]);
  });

  test("(a8) a resumed session's long turn is waited for the same way, the prompt sent once", async () => {
    const h = harness({
      clockStepMs: 10 * 3600 * 1000,
      previousSession: { sessionId: "s-old", runId: "r-old", alive: true },
      recv: [null, null, null, { kind: "session_idle", sessionId: "s-old" }, null, null],
    });
    const result = await interpretAutomation(RUN, h.deps);
    expect(result.status).toBe("completed");
    expect(h.prompts).toEqual([{ sessionId: "s-old", text: "summarize the incident" }]);
    expect(h.names).toContain("step:opening_turn.resume_wait[1].resume_wait_slice:0:wait");
    expect(h.policyCalls.some((c) => c.startsWith("fail:"))).toBe(false);
  });

  test("(a9) the engine's run ceiling under a running turn is a pause, not a failure: nothing is posted, the workstream stays open", async () => {
    // 48 h run deadline, 20 h per clock read, the session never goes idle.
    const h = harness({ clockStepMs: 20 * 3600 * 1000, recv: [null, null, null, null, null, null] });
    const result = await interpretAutomation(RUN, h.deps);
    expect(result.status).toBe("deadline");
    expect(h.policyCalls.some((c) => c.startsWith("fail:"))).toBe(false);
    expect(h.actions.filter((a) => a.actionId === "post_message").map((a) => a.params["text"])).toEqual([
      `Started a session — ${config.baseUrl}/sessions/s-1`,
    ]);
    expect(h.closed).toEqual([]);
    expect(h.ended).toEqual([]);
  });

  test("(a6) a kept session that is gone (swept) starts a fresh one", async () => {
    const h = harness({
      previousSession: { sessionId: "s-old", runId: "r-old", alive: false },
      recv: [{ kind: "session_idle", sessionId: "s-1" }, null, null],
    });
    const result = await interpretAutomation(RUN, h.deps);
    expect(result.status).toBe("completed");
    expect(h.sessions.map((s) => s.id)).toEqual(["s-1"]);
    expect(h.names).toContain("step:has_previous.started:0");
    const previous = h.records.filter((r) => r.path === "previous").at(-1);
    expect(previous?.record.outputs).toMatchObject({ found: false, reason: "gone", session_id: "s-old" });
  });

  test("(a') the relay survives a pod restart: its state rides the checkpointed step outputs, not process memory", async () => {
    // Pass 1: the relay renders three messages into one bubble, then the pod
    // dies while waiting. Pass 2 (recovery): DBOS replays the recorded step
    // outputs WITHOUT re-running their closures and re-delivers the recv'd
    // messages, then the run goes live: message 4 must append to the SAME
    // bubble (the state the dead pod built), and the ✅ recap must carry the
    // last message. With state in a module map both would be lost — the
    // relay would answer "pass" forever and the recap would post nothing.
    const h = harness({
      replay: [
        curated(1, "run_started", {}),
        curated(2, "agent_message", { role: "assistant", text: "one" }),
        curated(3, "agent_message", { role: "assistant", text: "two" }),
        "crash",
        curated(4, "agent_message", { role: "assistant", text: "three" }),
        curated(5, "run_completed", { ok: true }),
        { kind: "session_idle", sessionId: "s-1" },
        null,
        null,
      ],
    });
    const runner = h.runner!;
    // The dying pod: its interpretAutomation never settles (recv hangs).
    void interpretAutomation(RUN, h.deps).catch(() => {});
    // Let pass 1 run up to the hang.
    for (let i = 0; i < 50 && !h.policyCalls.includes("msg:one\n\ntwo"); i += 1) {
      await new Promise((r) => setTimeout(r, 1));
    }
    expect(h.policyCalls).toEqual(["working:100.1", "msg:one", "msg:one\n\ntwo"]);
    const liveBeforeCrash = runner.executed.length;
    expect(liveBeforeCrash).toBeGreaterThan(0);

    runner.restart();
    const result = await interpretAutomation(RUN, h.deps);

    expect(result.status).toBe("completed");
    // Recovery replayed the recorded relay steps (their closures did not
    // run again: no duplicate "msg:one"/"working" through the policy) …
    expect(h.policyCalls.filter((c) => c === "msg:one")).toHaveLength(1);
    expect(h.policyCalls.filter((c) => c === "working:100.1")).toHaveLength(1);
    expect(new Set(runner.executed).size).toBe(runner.executed.length);
    expect(runner.executed.length).toBeGreaterThan(liveBeforeCrash);
    // … then message 4 appended to the bubble the dead pod opened (the
    // relay's state came back through the checkpointed outputs) …
    expect(h.policyCalls).toContain("msg:one\n\ntwo\n\nthree");
    expect(h.policyCalls).toContain("idle:100.1");
    // … and the idle exit posted no recap (a pause, not an end).
    expect(h.policyCalls.filter((c) => c.startsWith("complete:"))).toEqual([]);
    // The relay's step names are unchanged across passes (contract stays 4).
    const relaySteps = h.names.filter((n) => n.includes(".__relay__:"));
    expect(relaySteps.slice(0, 3)).toEqual(relaySteps.slice(0, 3).map((n, i) => `step:relay.__relay__:${i + 1}`));
    // Only one install: recovery re-seeded the state, it did not re-bind.
    expect(h.relayFlags).toEqual([{ sessionId: "s-1", relay: true }]);
    expect(h.sessions).toHaveLength(1);
  });

  test("(a'') an author with no engrams user gets the legacy \"log in first\" message and no session", async () => {
    const h = harness({ linkedUsers: {} });
    const result = await interpretAutomation(RUN, h.deps);
    expect(result.status).toBe("filtered");
    expect(h.resolved).toEqual(["U1"]);
    expect(h.sessions).toEqual([]);
    expect(h.relayFlags).toEqual([]);
    // The legacy message, posted into the thread by an ordinary Slack action
    // (the graph's decision, not the identity block's).
    expect(h.policyCalls).toEqual([]);
    expect(h.actions).toEqual([
      { actionId: "post_message", params: { channel: "C1", threadTs: "100.1", text: NO_USER_MSG } },
    ]);
    expect(h.names).toContain("step:identity:0");
    expect(h.names).toContain("step:unlinked.login_notice:0");
    expect(h.names).not.toContain("step:session:0");
    expect(h.finalized).toHaveLength(1);
    expect(h.finalized[0]).toMatchObject({ status: "filtered" });
    // The notice bound the thread's handle to this workstream; closing it
    // would drop every later mention in the thread. It stays open.
    expect(h.closed).toEqual([]);
  });

  test("(a3) recovery still rebuilds the relay state when every ledger write fails", async () => {
    // Same crash/restart script as (a'), with the observability row never
    // landing: the checkpointed step outputs alone carry the state.
    const h = harness({
      failRelayLedger: true,
      replay: [
        curated(1, "run_started", {}),
        curated(2, "agent_message", { role: "assistant", text: "one" }),
        curated(3, "agent_message", { role: "assistant", text: "two" }),
        "crash",
        curated(4, "agent_message", { role: "assistant", text: "three" }),
        curated(5, "run_completed", { ok: true }),
        { kind: "session_idle", sessionId: "s-1" },
        null,
        null,
      ],
    });
    const runner = h.runner!;
    void interpretAutomation(RUN, h.deps).catch(() => {});
    for (let i = 0; i < 50 && !h.policyCalls.includes("msg:one\n\ntwo"); i += 1) {
      await new Promise((r) => setTimeout(r, 1));
    }
    runner.restart();
    const result = await interpretAutomation(RUN, h.deps);
    expect(result.status).toBe("completed");
    expect(h.policyCalls).toContain("msg:one\n\ntwo\n\nthree");
    expect(h.policyCalls.filter((c) => c.startsWith("complete:"))).toEqual([]);
    expect(h.records.filter((r) => r.path.endsWith(".__relay__"))).toEqual([]);
  });

  test("(a4) each accepted turn re-points the relay: ⏳/✅ land on the reply that started it", async () => {
    const h = harness({
      recv: [
        curated(1, "run_started", {}),
        curated(2, "agent_message", { role: "assistant", text: "first answer" }),
        curated(3, "run_completed", { ok: true }),
        { kind: "session_idle", sessionId: "s-1" },
        joined("<@UBOT> and then?", "100.2"),
        curated(4, "run_started", {}),
        curated(5, "agent_message", { role: "assistant", text: "second answer" }),
        curated(6, "run_completed", { ok: true }),
        { kind: "session_idle", sessionId: "s-1" },
        null,
        null,
      ],
    });
    const result = await interpretAutomation(RUN, h.deps);
    expect(result.status).toBe("completed");
    // Turn 1 reacts on the opening mention; turn 2 on the follow-up's ts.
    expect(h.policyCalls.filter((c) => c.startsWith("working:"))).toEqual(["working:100.1", "working:100.2"]);
    expect(h.policyCalls.filter((c) => c.startsWith("idle:"))).toEqual(["idle:100.1", "idle:100.2"]);
    // The second answer opened a fresh bubble (no append onto the first).
    expect(h.policyCalls.filter((c) => c.startsWith("msg:"))).toEqual(["msg:first answer", "msg:second answer"]);
    // The re-point ran inside the loop body, before the turn's prompt, and
    // was NOT a second install.
    const body = h.names.filter((n) => n.startsWith("step:thread[0].has_event.has_turn."));
    expect(body.indexOf("step:thread[0].has_event.has_turn.repoint:0")).toBeLessThan(
      body.indexOf("step:thread[0].has_event.has_turn.turn:0"),
    );
    expect(h.relayFlags).toEqual([{ sessionId: "s-1", relay: true }]);
    // Relay step names keep the FIRST install's path and a single counter
    // across the re-point (every mailbox message is offered: 6 curated
    // events, 2 idles, 1 joined event).
    expect(h.names.filter((n) => n.includes(".__relay__:"))).toEqual(
      [1, 2, 3, 4, 5, 6, 7, 8, 9].map((n) => `step:relay.__relay__:${n}`),
    );
    // The idle exit posts no recap; the last policy call is the turn's ✅.
    expect(h.policyCalls.at(-1)).toBe("idle:100.2");
  });

  test("(b) a top-level channel message (no thread_ts) is filtered, never a session", async () => {
    const h = harness({
      eventKey: "message",
      payload: { team_id: "T1", event_id: "Ev9", event: { type: "message", channel: "C1", user: "U2", ts: "5.0", text: "hi" } },
    });
    const result = await interpretAutomation(RUN, h.deps);
    expect(result.status).toBe("filtered");
    expect(h.sessions).toEqual([]);
    // No relay → the recap hook has nothing to post to and says so.
    const recap = h.records.filter((r) => r.path.includes("recap")).at(-1);
    expect(recap?.record.outputs).toMatchObject({ posted: false });
  });

  test("(c) an unflagged channel with no default profile is filtered", async () => {
    const h = harness({ inputs: { channels: {}, default_profile: "", idle_timeout: 600, max_turns: 5 } });
    expect((await interpretAutomation(RUN, h.deps)).status).toBe("filtered");
    expect(h.sessions).toEqual([]);
  });

  test("(d) max_turns bounds the conversation; exhaustion ends the run cleanly", async () => {
    const h = harness({
      inputs: { channels: { C1: "prof-a" }, default_profile: "", idle_timeout: 600, max_turns: 2 },
      recv: [
        { kind: "session_idle", sessionId: "s-1" },
        joined("turn one", "100.2"),
        { kind: "session_idle", sessionId: "s-1" },
        joined("turn two", "100.3"),
        { kind: "session_idle", sessionId: "s-1" },
        // Would be turn three, but max_turns = 2: never consumed.
        joined("turn three", "100.4"),
      ],
    });
    const result = await interpretAutomation(RUN, h.deps);
    expect(result.status).toBe("completed");
    expect(h.prompts.map((p) => p.text)).toEqual(["turn one", "turn two"]);
    // Each turn's read starts at the previous mention: the cursor advances
    // with the re-point, no extra state.
    expect(h.actions.filter((a) => a.actionId === "list_replies").map((a) => a.params["oldest"])).toEqual([
      undefined,
      "100.1",
      "100.2",
    ]);
    // Exactly two iterations ran; the third joined message was never consumed.
    expect(h.names.filter((n) => /^step:thread\[\d+\]\.next:0$/.test(n)).length).toBe(2);
    expect(h.finalized).toEqual([{ status: "completed" }]);
  });

  test("(d') a bare `@bot` follow-up is not a turn: no prompt, no failure, the thread continues", async () => {
    const h = harness({
      recv: [
        { kind: "session_idle", sessionId: "s-1" },
        // Nothing left once the mention is stripped.
        joined("<@UBOT>", "100.2"),
        joined("<@UBOT> now the real question", "100.3"),
        { kind: "session_idle", sessionId: "s-1" },
        null,
        null,
      ],
    });
    const result = await interpretAutomation(RUN, h.deps);
    expect(result.status).toBe("completed");
    // The empty turn never reached send_prompt (which refuses an empty prompt);
    // the next real message did.
    expect(h.prompts.map((p) => p.text)).toEqual(["now the real question"]);
    expect(h.names).not.toContain("step:thread[0].has_event.has_turn.turn:0");
    expect(h.names).toContain("step:thread[1].has_event.has_turn.turn:0");
    // The idle exit posts no recap.
    expect(h.policyCalls.filter((c) => c.startsWith("complete:"))).toEqual([]);
    expect(h.finalized).toEqual([{ status: "completed" }]);
  });

  test("(d'') a bot-authored app_mention in the thread is ignored by the continuation wait", async () => {
    const botMention = (ts: string): AutomationInbox => ({
      kind: "event",
      eventKey: "app_mention",
      deliveryKey: `slack:Ev-${ts}`,
      payload: {
        team_id: "T1",
        event_id: `Ev-${ts}`,
        event: {
          type: "app_mention",
          channel: "C1",
          bot_id: "B1",
          user: "UBOT2",
          ts,
          thread_ts: "100.1",
          text: "<@UBOT> look at this",
        },
      },
      receivedAt: "2026-08-22T10:00:05Z",
    });
    const h = harness({
      recv: [
        { kind: "session_idle", sessionId: "s-1" },
        botMention("100.2"),
        joined("<@UBOT> a human follow-up", "100.3"),
        { kind: "session_idle", sessionId: "s-1" },
        null,
        null,
      ],
    });
    const result = await interpretAutomation(RUN, h.deps);
    expect(result.status).toBe("completed");
    // The bot's mention never became a turn; the wait kept listening and the
    // human's message did.
    expect(h.prompts.map((p) => p.text)).toEqual(["a human follow-up"]);
    expect(h.names.filter((n) => /\.has_event\.has_turn\.turn:0$/.test(n))).toHaveLength(1);
    // The WAIT itself refused the bot event (not only the turn gate): the
    // first iteration's `next` matched the human's message.
    const firstNext = h.records.filter((r) => r.path === "thread[0].next").at(-1);
    expect(firstNext?.record.outputs).toMatchObject({
      outcome: "event",
      event: { event: { ts: "100.3", user: "U2" } },
    });
  });

  test("(e) a failed turn ends the run as failed and posts ❌ through the policy", async () => {
    const h = harness({
      recv: [{ kind: "session_idle", sessionId: "s-1", runFailed: true }],
    });
    const result = await interpretAutomation(RUN, h.deps);
    expect(result.status).toBe("failed");
    expect(h.policyCalls.some((c) => c.startsWith("fail:"))).toBe(true);
    expect(h.ended).toEqual([]);
  });
});
