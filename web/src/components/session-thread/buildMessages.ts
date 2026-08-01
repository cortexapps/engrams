import type { ThreadMessageLike } from "@assistant-ui/react";
import type {
  AgentRole,
  FileChange,
  IndexedEvent,
  SessionState,
  UserQuestion,
} from "../../lib/types";

// Session statuses that mean "no turn is in flight" — the authoritative
// signal that overrides the event stream. A session evicted/terminated
// mid-run emits no terminal run event, so `runOpen` would otherwise hang the
// composer on "working…" forever (a dead Stop button). `idle` covers the
// snapshot/idle-evict case; the rest are terminal.
export const INACTIVE_STATUSES: ReadonlySet<SessionState> = new Set<SessionState>([
  // Parked = VM paused in place after the harness went idle (ADR 0101):
  // no turn can be in flight inside a frozen guest.
  "parked",
  // Unreachable = the guest is dead/wedged (ADR 0091): whatever turn was
  // open died with it — without this the composer shows "working…" and a
  // dead Stop button for a guest that can no longer answer.
  "unreachable",
  "idle",
  "completed",
  "failed",
  "dead",
]);

// Adapt the session's append-only SSE event stream (ADR 0030) into the
// assistant-ui message model. This is the successor to Transcript's
// `buildBlocks`: instead of a flat block union we emit `ThreadMessageLike`
// records that the external-store runtime renders.
//
// The model shift:
//
//   - One assistant message PER RUN. A run's assistant text and its tool
//     calls (and shell execs) become ordered PARTS of a single assistant
//     message — assistant-ui treats tool calls as message parts, not
//     siblings. The message status flows running → complete/incomplete as
//     the run opens and closes; the run tally (read N · edited N · ran N)
//     rides in `metadata.custom` as a footer.
//   - Shell execs have no native model, so each becomes a synthetic
//     tool-call part named `engram.shell` (args carry the command + exit;
//     stdout/stderr stream into `result`). A registered tool UI renders it.
//   - Harness narration that isn't a message and isn't an agent tool call
//     (snapshot/resume durability, opened PRs, shared artifacts) becomes a
//     SYSTEM message. System messages are constrained to a single text
//     part, so the real payload rides in `metadata.custom.marker` and a
//     custom SystemMessage renderer switches on `marker.kind`. This is the
//     "harness register", visually distinct from the agent's tool calls.
//   - `harness_idle` and bare run boundaries collapse into message spacing
//     + the derived `isRunning` flag rather than discrete markers.

/** Reserved synthetic tool name for a sandbox shell exec (vs. an agent
 *  tool_call). Namespaced so it can't collide with a real harness tool. */
export const SHELL_TOOL = "engram.shell";

/** Args we stash on the synthetic shell tool-call part. */
export interface ShellArgs {
  command: string;
  exit?: number | null;
  durationMs?: number | null;
}

/** ADR 0054 Flavor A: synthetic tool name for a file change. A
 *  Write/Edit/MultiEdit tool call that has a matching `file_changed` event is
 *  re-rendered under this name (a rich diff) instead of the generic tool card;
 *  a registered tool UI (`FileChangePart`) reads {@link FileChangeArgs}. */
export const FILE_CHANGE_TOOL = "engram.fileChange";

/** Args we stash on the synthetic file-change tool-call part. */
export interface FileChangeArgs {
  path: string;
  change: FileChange;
}

/** Synthetic presenter for a browser-enriched Shell/Bash call. The matching
 * browser_activity event carries display intent; generic completion still
 * owns success/failure correlation. */
export const BROWSER_ACTIVITY_TOOL = "engram.browserActivity";

export interface BrowserActivityArgs {
  intent: string;
}

/** Payload carried in a system message's `metadata.custom.marker` — the
 *  harness-register events that aren't agent messages. */
export type SystemMarker =
  | { kind: "durability"; mark: "snapshot" | "resumed" | "waking"; sizeBytes?: number; at: string }
  // ADR 0056: a generic integration asset/action. Subsumes the old
  // `pull_request` marker. The renderer keys on (provider, assetKind) with a
  // generic fallback (SystemMessage.tsx) — no per-provider marker shape.
  | {
      kind: "integration_asset";
      provider: string;
      assetKind: string;
      surface: "action" | "asset";
      data: Record<string, unknown>;
      fetchable:
        | { kind: "external"; url: string }
        | { kind: "artifact"; artifactId: string; mediaType: string; sizeBytes: number }
        | null;
      at: string;
    }
  | {
      kind: "artifact";
      sessionId: string;
      artifactId: string;
      mediaType: string;
      sizeBytes: number;
      caption: string | null;
      at: string;
    }
  | { kind: "note"; role: AgentRole; at: string }
  // ADR 0054: the agent asked a clarifying question via `AskUserQuestion`
  // (deferred by the harness). Rendered as an INTERACTIVE card — a form
  // while unanswered, a read-only receipt once answered. `answers` is folded
  // in from the matching `question_answered` event (keyed by `tool_call_id`)
  // so a full rebuild shows the resolved state; null while still awaiting.
  | {
      kind: "user_question";
      toolCallId: string;
      questions: UserQuestion[];
      answers: Record<string, string[]> | null;
      via: QuestionProtocol;
      at: string;
    }
  // ADR 0028 A.log: a rung-1 recovery rewound the live transcript to a
  // checkpoint. The boundary itself is live; the rolled-back events above
  // it render greyed (see `rewound` tagging below). Outside-world side
  // effects in the rolled-back span survived and are surfaced, not hidden.
  | {
      kind: "recovery";
      rolledBack: number;
      survivingSideEffects: string[];
      // ADR 0045 F1: planned operator relocation vs unplanned host failure.
      cause: "planned_relocation" | "host_failure_recovery" | "checkpoint_lag";
      at: string;
    }
  // ADR 0090 (2026-07-20 durability-rollback incident): a quarantined-survivor
  // eviction exhausted its budget; the coordinator destroyed the crippled VM
  // and the next resume rewinds to the last published disk manifest, dropping
  // guest writes acked-but-never-uploaded past it. A prominent warning marker.
  | {
      kind: "durability_rollback";
      manifest: string | null;
      reason: string;
      at: string;
    }
  // ADR 0107: the agent proposed a plan via the deferred `exit_plan_mode`
  // tool. Rendered as an INTERACTIVE card — the doc + approve/reject while
  // unresolved, a one-line receipt once decided. `resolution` folds in from
  // the matching `tool_result_submitted`.
  | {
      kind: "plan";
      toolCallId: string;
      plan: string;
      /** 1-based ordinal of this plan among the session's plan proposals. */
      revision: number;
      resolution: { approved: boolean; feedback: string | null; at: string } | null;
      at: string;
    }
  // ADR 0107: a validated session-mode directive rode a prompt — a faint
  // one-line marker narrating the transition ("planning — read-only" /
  // "plan mode off").
  | { kind: "mode"; mode: string; at: string }
  // ADR 0107: the agent called exit_plan_mode OUTSIDE plan mode (the harness
  // rejected it in place — no park, no card). Rendered as a hint marker
  // instead of a dead failed-tool row.
  | { kind: "plan_attempt"; at: string };

/** Footer carried in an assistant message's `metadata.custom.run` — the
 *  per-run receipt (`↳ read N · edited N · ran N`). */
export interface RunFooter {
  reads: number;
  edits: number;
  ran: number;
  other: number;
  ok: boolean;
  interrupted: boolean;
  endAt: string;
}

/** The completion wire a question card must use. */
export type QuestionProtocol = "generic" | "legacy";

/** A prompt the harness has queued (type-ahead) and not yet consumed. */
export interface QueuedPrompt {
  promptId: string;
  /** The wire `summary` (first ~1 KB of the prompt) — recall display fallback
   *  when the full client-side text isn't available (refresh / other client). */
  summary: string;
}

export interface BuildMessagesResult {
  messages: ThreadMessageLike[];
  /** Flows to `thread.isRunning` — drives the composer send/stop toggle
   *  and the trailing working indicator. */
  isRunning: boolean;
  /** ADR 0052: prompts queued mid-turn (type-ahead) and not yet consumed,
   *  oldest→newest. Drives the composer's ↑-to-edit recall + cancel. Rebuilt
   *  purely from session events, so it survives refresh / multi-client. */
  queue: QueuedPrompt[];
  /** ADR 0107: the newest unresolved plan proposal, if any — drives the
   *  composer's review hint and the attention affordances. Null when every
   *  plan is decided or none was ever proposed. */
  pendingPlan: { toolCallId: string } | null;
  /** ADR 0107: the session's current mode as derivable from the event log —
   *  the latest `harness_mode_changed`, overridden to the default by a later
   *  approved plan. Drives the composer chip. */
  currentMode: string;
}

// ---- internal mutable drafts (assignable to ThreadMessageLike) --------

type TextPart = { type: "text"; text: string };
type ToolPart = {
  type: "tool-call";
  toolCallId: string;
  toolName: string;
  args: Record<string, unknown>;
  argsText: string;
  result?: unknown;
  isError?: boolean;
};
type Part = TextPart | ToolPart;

interface Draft {
  role: "user" | "assistant" | "system";
  content: Part[];
  id: string;
  createdAt?: Date;
  status?: ThreadMessageLike["status"];
  metadata?: { custom: Record<string, unknown> };
}

const PENDING_ID = "pending";

function classifyTool(name: string): "reads" | "edits" | "ran" | "other" {
  if (/^(read|grep|glob|ls|list|search|cat|find|notebookread|fetch|web)/i.test(name))
    return "reads";
  if (
    /^(edit|write|create|apply_?patch|multiedit|notebookedit|update|delete|remove|move|rename|mkdir)/i.test(
      name,
    )
  )
    return "edits";
  // Command-running tools — the agent's `Bash`/shell calls live here so they
  // tally as "ran" alongside sandbox execs, not as opaque "other".
  if (/^(bash|shell|sh|exec|run|command|terminal|kill|process|task|agent)/i.test(name))
    return "ran";
  return "other";
}

function parseArgs(argsSummary: string | null): Record<string, unknown> {
  if (!argsSummary) return {};
  try {
    const o = JSON.parse(argsSummary) as unknown;
    return o && typeof o === "object" ? (o as Record<string, unknown>) : {};
  } catch {
    return {};
  }
}

function parseCanonicalQuestions(argsJson: string): UserQuestion[] | null {
  try {
    const raw = JSON.parse(argsJson) as { questions?: unknown };
    if (!Array.isArray(raw.questions)) return null;
    const questions: UserQuestion[] = [];
    for (const value of raw.questions) {
      if (typeof value !== "object" || value === null || Array.isArray(value)) return null;
      const question = value as {
        question?: unknown;
        header?: unknown;
        multiSelect?: unknown;
        options?: unknown;
      };
      if (
        typeof question.question !== "string" ||
        typeof question.header !== "string" ||
        typeof question.multiSelect !== "boolean" ||
        !Array.isArray(question.options)
      ) {
        return null;
      }
      const options: UserQuestion["options"] = [];
      for (const value of question.options) {
        if (typeof value !== "object" || value === null || Array.isArray(value)) return null;
        const option = value as { label?: unknown; description?: unknown };
        if (typeof option.label !== "string" || typeof option.description !== "string") return null;
        options.push({ label: option.label, description: option.description });
      }
      questions.push({
        question: question.question,
        header: question.header,
        multiSelect: question.multiSelect,
        options,
      });
    }
    return questions;
  } catch {
    return null;
  }
}

function parseCanonicalAnswers(resultJson: string): Record<string, string[]> | null {
  try {
    const raw: unknown = JSON.parse(resultJson);
    if (typeof raw !== "object" || raw === null || Array.isArray(raw)) return null;
    const answers: Record<string, string[]> = {};
    for (const [question, value] of Object.entries(raw)) {
      if (!Array.isArray(value) || !value.every((label) => typeof label === "string")) return null;
      answers[question] = value;
    }
    return answers;
  } catch {
    return null;
  }
}

/** ADR 0107: `exit_plan_mode` request args — `{plan: markdown}`. */
function parsePlanArgs(argsJson: string): string | null {
  try {
    const raw: unknown = JSON.parse(argsJson);
    if (typeof raw !== "object" || raw === null) return null;
    const plan = (raw as Record<string, unknown>).plan;
    return typeof plan === "string" && plan.length > 0 ? plan : null;
  } catch {
    return null;
  }
}

/** ADR 0107: `exit_plan_mode` result — `{decision, feedback?}`. */
function parsePlanResolution(
  resultJson: string,
  at: string,
): { approved: boolean; feedback: string | null; at: string } | null {
  try {
    const raw: unknown = JSON.parse(resultJson);
    if (typeof raw !== "object" || raw === null) return null;
    const decision = (raw as Record<string, unknown>).decision;
    if (decision !== "approve" && decision !== "reject") return null;
    const feedback = (raw as Record<string, unknown>).feedback;
    return {
      approved: decision === "approve",
      feedback: typeof feedback === "string" && feedback.trim().length > 0 ? feedback : null,
      at,
    };
  } catch {
    return null;
  }
}

function parseJsonValue(json: string): { ok: true; value: unknown } | { ok: false } {
  try {
    return { ok: true, value: JSON.parse(json) as unknown };
  } catch {
    return { ok: false };
  }
}

/** Claude exposes registered tools as `mcp__engrams__<name>` while generic
 * request events carry the canonical registry name. */
function canonicalToolName(name: string): string {
  const prefix = "mcp__engrams__";
  return name.startsWith(prefix) ? name.slice(prefix.length) : name;
}

export function buildMessages(
  events: IndexedEvent[],
  sessionId: string,
  status?: SessionState,
  // Phase 1c: the live token tail (ephemeral `agent_message_chunk` deltas
  // for the in-flight assistant message, NOT in `events`). Appended to the
  // active assistant turn so tokens render as they stream; the terminal
  // durable `agent_message` supersedes it (the hook empties it the instant
  // that lands, so re-running yields identical text — no double-render).
  streamingText = "",
): BuildMessagesResult {
  const out: Draft[] = [];

  // The assistant message currently accumulating this run's parts, or null
  // between runs / after a harness marker breaks the flow.
  let active: Draft | null = null;
  const openTools = new Map<string, ToolPart>(); // tool_call_id → part
  const openExecs = new Map<string, ToolPart>(); // exec_id → part
  // The text of a user prompt that carried a client-minted prompt_id, held by
  // id until its consuming `run_started{prompt_id}`. A prompt_id user message
  // is NOT rendered inline at echo time: it enters the transcript at the
  // CONSUMPTION position, so a message queued mid-run lands AFTER the turn it
  // was queued during (not above it). While still queued it lives by the
  // composer — `SessionThread` renders the `queue` there (Claude-Code style).
  const heldUserText = new Map<string, { text: string; at: string }>();

  // ADR 0052: the harness-owned queue, mirrored from events. prompt_id →
  // summary, insertion-ordered (oldest→newest). Entries leave on run_started
  // (consumed) or prompt_dequeued, or are updated by prompt_edited. Surfaced
  // as `queue` for the composer's queued-message rail.
  const queued = new Map<string, string>();

  // Is a run in flight (run_started seen, no run_completed/_interrupted yet)?
  let runOpen = false;
  // `out.length` when the current run opened — bounds the search for "this
  // run's assistant message" so the run receipt attaches to the right bubble
  // even when a trailing harness marker (e.g. a deferred question) broke the
  // active turn before run_completed.
  let runStartLen = 0;

  // Per-run tally feeding the assistant-message footer.
  let tally: { reads: number; edits: number; ran: number; other: number } | null = null;
  const bump = (k: "reads" | "edits" | "ran" | "other") => {
    if (tally) tally[k] += 1;
  };

  // The most recent assistant draft created during the current run (since
  // `runStartLen`), or null if this run produced none — used to attach the run
  // receipt without synthesizing an empty bubble.
  const runAssistant = (): Draft | null => {
    for (let i = out.length - 1; i >= runStartLen; i--) {
      if (out[i]!.role === "assistant") return out[i]!;
    }
    return null;
  };

  // Assistant message ids MUST be position-independent. A previous `a:${out.length}`
  // scheme keyed on array position, so inserting/removing an earlier bubble (a
  // queued user message landing mid-run, or a `prompt_dequeued` filtering one out
  // after the loop) re-numbered every later assistant turn — and assistant-ui keys
  // messages by id, so a turn silently changing id throws "a message with the same
  // id already exists in the parent tree" (the crash). A dedicated monotonic counter
  // gives the Nth assistant turn a STABLE `a:N` regardless of what surrounds it.
  let assistantSeq = 0;

  const ensureAssistant = (at?: string): Draft => {
    if (active) return active;
    active = {
      role: "assistant",
      content: [],
      id: `a:${assistantSeq++}`,
      createdAt: at ? new Date(at) : undefined,
      status: { type: "running" },
    };
    out.push(active);
    return active;
  };

  // Push a system "harness register" message carrying a marker payload. The
  // single text part is a plain-text fallback; the renderer reads `marker`.
  let planRevision = 0;
  const planAttemptToolCallIds = new Set<string>();
  const pushSystem = (id: string, fallback: string, marker: SystemMarker) => {
    active = null;
    out.push({
      role: "system",
      content: [{ type: "text", text: fallback }],
      id,
      createdAt: "at" in marker ? new Date(marker.at) : undefined,
      metadata: { custom: { marker } },
    });
  };

  // The "waking up" resume marker is TRANSIENT — it should read as live
  // status while a session wakes, not accrete permanent history. The
  // coordinator emits (rewind-excluded) ResumeStarted events, potentially
  // one per resume op across retries; we render only the latest, and drop
  // it entirely once the session actually produces something (resumed /
  // run_started / an agent message). `wakingId` tracks the live marker so
  // `clearWaking` can splice it back out of `out`.
  let wakingId: string | null = null;
  const clearWaking = () => {
    if (wakingId == null) return;
    const i = out.findIndex((d) => d.id === wakingId);
    if (i !== -1) out.splice(i, 1);
    wakingId = null;
  };

  // ADR 0028 A.log: stamp `rewound` into the custom metadata of every draft
  // an event touched, so the rolled-back span renders greyed. Preserves any
  // existing custom payload (run footer, marker).
  const markRewound = (d: Draft) => {
    d.metadata = { custom: { ...d.metadata?.custom, rewound: true } };
  };

  // Question cards span two durable protocols forever: historical
  // user_question/question_answered and ADR 0089's generic request/result pair.
  // Pre-scan makes folding order-independent and lets #64389 phantom starts be
  // distinguished from real tool rows.
  const questionToolCallIds = new Set<string>();
  const answersByToolCallId = new Map<string, Record<string, string[]>>();
  // ADR 0107: plan proposals + their folded decisions.
  const planToolCallIds = new Set<string>();
  const planResolutionByToolCallId = new Map<
    string,
    { approved: boolean; feedback: string | null; at: string }
  >();
  const genericRequests = new Map<
    string,
    Extract<IndexedEvent["event"], { type: "tool_call_requested" }>
  >();
  const submittedResults = new Map<string, unknown>();
  const startedToolCallIds = new Set<string>();
  const completedToolCallIds = new Set<string>();
  const endedRunIds = new Set<string>();
  const requestedToolNames = new Set<string>();
  // ADR 0054 Flavor A: a Write/Edit/MultiEdit tool call emits a generic
  // tool_call_started AND (on success) a `file_changed` carrying the diff. We
  // pre-scan so the generic card is re-rendered as a rich diff in place; a
  // failed edit emits NO file_changed and keeps its generic (error) card.
  const fileChangesByToolCallId = new Map<string, FileChangeArgs>();
  const browserActivityByToolCallId = new Map<string, BrowserActivityArgs>();
  // prod session 68c70a65: a `prompt_id` user echo is HELD and rendered at its
  // consuming `run_started{prompt_id}` (below). That relies on the echo being
  // seen BEFORE its run_started — true when the coordinator appends the echo
  // ahead of the forward, but a `SendPrompt` forwarded before the echo lands
  // inverts them (run_started first), so `heldUserText` is still empty when the
  // run_started renders and the echo (held next) never renders → the user's
  // turn vanishes. Pre-scanning the echo text by prompt_id makes the render
  // order-independent: run_started finds it whether the echo came before or
  // after. (A recalled prompt has no run_started, so it still never renders.)
  const userEchoByPromptId = new Map<string, string>();
  for (const { event } of events) {
    if (event.type === "user_question") questionToolCallIds.add(event.tool_call_id);
    else if (event.type === "tool_call_requested") {
      genericRequests.set(event.tool_call_id, event);
      requestedToolNames.add(event.name);
      if (event.name === "ask_user_question" && parseCanonicalQuestions(event.args_json) !== null) {
        questionToolCallIds.add(event.tool_call_id);
      }
      if (event.name === "exit_plan_mode" && parsePlanArgs(event.args_json) !== null) {
        planToolCallIds.add(event.tool_call_id);
      }
    } else if (event.type === "tool_call_started") {
      startedToolCallIds.add(event.tool_call_id);
    } else if (event.type === "tool_call_completed") {
      completedToolCallIds.add(event.tool_call_id);
    } else if (event.type === "run_completed" || event.type === "run_interrupted") {
      endedRunIds.add(event.run_id);
    } else if (event.type === "file_changed")
      fileChangesByToolCallId.set(event.tool_call_id, {
        path: event.path,
        change: event.change,
      });
    else if (event.type === "browser_activity")
      browserActivityByToolCallId.set(event.tool_call_id, { intent: event.intent });
    else if (event.type === "agent_message" && event.role === "user" && event.prompt_id)
      userEchoByPromptId.set(event.prompt_id, event.text);
  }
  for (const { event } of events) {
    if (event.type === "question_answered") {
      answersByToolCallId.set(event.tool_call_id, event.answers);
    } else if (event.type === "tool_result_submitted") {
      const parsed = parseJsonValue(event.result_json);
      if (parsed.ok) submittedResults.set(event.tool_call_id, parsed.value);
      if (questionToolCallIds.has(event.tool_call_id)) {
        const answers = parseCanonicalAnswers(event.result_json);
        if (answers) answersByToolCallId.set(event.tool_call_id, answers);
      }
      if (planToolCallIds.has(event.tool_call_id)) {
        const resolution = parsePlanResolution(event.result_json, event.at);
        if (resolution) planResolutionByToolCallId.set(event.tool_call_id, resolution);
      }
    }
  }

  for (const indexed of events) {
    const { idx, event: ev } = indexed;
    const lenBefore = out.length;
    switch (ev.type) {
      case "harness_mode_changed": {
        pushSystem(`mode:${idx}`, `mode: ${ev.mode}`, {
          kind: "mode",
          mode: ev.mode,
          at: ev.at,
        });
        break;
      }

      case "run_started": {
        // The session is producing output — the transient "waking up" marker
        // has served its purpose; drop it BEFORE capturing runStartLen so the
        // splice can't shift the run's bounds.
        clearWaking();
        tally = { reads: 0, edits: 0, ran: 0, other: 0 };
        runOpen = true;
        active = null;
        runStartLen = out.length;
        if (ev.prompt_id) {
          // The prompt that started this run enters the conversation HERE — its
          // consumption position. For a message queued mid-run that's AFTER the
          // turn it was queued during (the correct order); for an idle prompt
          // it's right where it was sent. Text from the held echo (full text),
          // falling back to the queue summary.
          const held = heldUserText.get(ev.prompt_id);
          // Fall back to the pre-scanned echo (prod 68c70a65): when the echo was
          // appended AFTER this run_started, `heldUserText` is empty here but the
          // pre-scan still has the text, so the user turn renders instead of
          // vanishing.
          const text =
            held?.text ?? userEchoByPromptId.get(ev.prompt_id) ?? queued.get(ev.prompt_id) ?? "";
          if (text) {
            out.push({
              role: "user",
              content: [{ type: "text", text }],
              id: ev.prompt_id,
              createdAt: held ? new Date(held.at) : new Date(ev.at),
            });
          }
          heldUserText.delete(ev.prompt_id);
          queued.delete(ev.prompt_id); // consumed → leaves the queue
        } else if (ev.prompt_summary) {
          // No prompt_id (env-seeded initial prompt) but a summary is present —
          // surface it as the user turn at its run position.
          out.push({
            role: "user",
            content: [{ type: "text", text: ev.prompt_summary }],
            id: `rs:${idx}`,
            createdAt: new Date(ev.at),
          });
        }
        break;
      }

      case "agent_message": {
        if (ev.role === "user") {
          active = null;
          if (ev.prompt_id) {
            // HOLD — don't render inline. The echo of a prompt_id user message
            // is logged at send/queue time, which for a queued message is mid
            // the PRIOR run; rendering it here would place it above that run's
            // response. Instead we hold the text and emit the bubble at its
            // `run_started{prompt_id}` (the consumption position). While still
            // queued it shows by the composer, not in the thread.
            heldUserText.set(ev.prompt_id, { text: ev.text, at: ev.at });
          } else {
            // No prompt_id (env-seeded initial prompt) — render inline.
            out.push({
              role: "user",
              content: [{ type: "text", text: ev.text }],
              id: `m:${idx}`,
              createdAt: new Date(ev.at),
            });
          }
        } else if (ev.role === "system") {
          pushSystem(`m:${idx}`, ev.text, { kind: "note", role: ev.role, at: ev.at });
        } else {
          const a = ensureAssistant(ev.at);
          const last = a.content[a.content.length - 1];
          // Coalesce consecutive assistant text into one prose part (matches
          // the old block model's join), keeping tool parts as boundaries.
          if (last && last.type === "text") last.text += `\n\n${ev.text}`;
          else a.content.push({ type: "text", text: ev.text });
        }
        break;
      }

      case "tool_call_started": {
        // ADR 0107: an exit_plan_mode call with NO generic request behind it
        // is the out-of-mode rejection (the harness answered it in place).
        // A failed tool row reads as breakage; a hint reads as guidance.
        if (
          canonicalToolName(ev.tool_name) === "exit_plan_mode" &&
          !genericRequests.has(ev.tool_call_id)
        ) {
          planAttemptToolCallIds.add(ev.tool_call_id);
          pushSystem(`plan-attempt:${idx}`, "the agent drafted a plan outside plan mode", {
            kind: "plan_attempt",
            at: ev.at,
          });
          break;
        }
        // #64389: Claude may narrate multiple AskUserQuestion tool_use rows for
        // one real deferred request. A start is phantom only when its tool maps
        // to a deferred request seen in this transcript (or the native binding),
        // and its own id has neither a request nor a completion. Ordinary
        // in-flight sync tools remain visible.
        const nativeQuestion = ev.tool_name === "AskUserQuestion";
        const mapsToObservedDeferred =
          requestedToolNames.has(canonicalToolName(ev.tool_name)) && endedRunIds.has(ev.run_id);
        const phantom =
          (nativeQuestion || mapsToObservedDeferred) &&
          !genericRequests.has(ev.tool_call_id) &&
          !completedToolCallIds.has(ev.tool_call_id);
        if (questionToolCallIds.has(ev.tool_call_id) || phantom) break;
        bump(classifyTool(ev.tool_name));
        const a = ensureAssistant(ev.at);
        // ADR 0054 Flavor A: a Write/Edit/MultiEdit that produced a successful
        // `file_changed` renders as a rich diff (the `FILE_CHANGE_TOOL` part)
        // in place of the generic card. The tally still counts the ORIGINAL
        // tool (an edit), and the part keeps its real id so the completion
        // correlates as usual. No file_changed (e.g. a failed edit) → generic.
        const fc = fileChangesByToolCallId.get(ev.tool_call_id);
        const browserActivity = browserActivityByToolCallId.get(ev.tool_call_id);
        const part: ToolPart = fc
          ? {
              type: "tool-call",
              toolCallId: ev.tool_call_id,
              toolName: FILE_CHANGE_TOOL,
              args: fc as unknown as Record<string, unknown>,
              argsText: fc.path,
            }
          : browserActivity
            ? {
                type: "tool-call",
                toolCallId: ev.tool_call_id,
                toolName: BROWSER_ACTIVITY_TOOL,
                args: browserActivity as unknown as Record<string, unknown>,
                argsText: browserActivity.intent,
              }
            : {
                type: "tool-call",
                toolCallId: ev.tool_call_id,
                toolName: ev.tool_name,
                args: parseArgs(ev.args_summary),
                argsText: ev.args_summary ?? "",
              };
        a.content.push(part);
        if (submittedResults.has(ev.tool_call_id)) {
          part.result = submittedResults.get(ev.tool_call_id);
        }
        openTools.set(ev.tool_call_id, part);
        break;
      }

      case "tool_call_requested": {
        if (ev.name === "exit_plan_mode") {
          const plan = parsePlanArgs(ev.args_json);
          if (plan) {
            planRevision += 1;
            pushSystem(`tcr:${idx}`, "the agent proposed a plan", {
              kind: "plan",
              toolCallId: ev.tool_call_id,
              plan,
              revision: planRevision,
              resolution: planResolutionByToolCallId.get(ev.tool_call_id) ?? null,
              at: ev.at,
            });
          }
          break;
        }
        if (ev.name === "ask_user_question") {
          const questions = parseCanonicalQuestions(ev.args_json);
          if (questions) {
            pushSystem(`tcr:${idx}`, "the agent asked a question", {
              kind: "user_question",
              toolCallId: ev.tool_call_id,
              questions,
              answers: answersByToolCallId.get(ev.tool_call_id) ?? null,
              via: "generic",
              at: ev.at,
            });
          }
          break;
        }
        // The harness may also emit a native tool_call_started with this same
        // id. That row owns the render when present; otherwise the durable
        // request itself becomes the pending fallback row.
        if (startedToolCallIds.has(ev.tool_call_id)) break;
        const part: ToolPart = {
          type: "tool-call",
          toolCallId: ev.tool_call_id,
          toolName: ev.name,
          args: parseArgs(ev.args_json),
          argsText: ev.args_json,
        };
        if (submittedResults.has(ev.tool_call_id)) {
          part.result = submittedResults.get(ev.tool_call_id);
        }
        ensureAssistant(ev.at).content.push(part);
        openTools.set(ev.tool_call_id, part);
        break;
      }

      // Folded onto its generic request row/card by the pre-scan.
      case "tool_result_submitted":
        break;

      case "tool_call_completed": {
        // ADR 0054: the answered AskUserQuestion's tool_result (it re-fired on
        // resume) — its outcome is the card, not a tool part. `tool_name` is
        // blank on completed events, so match on the pre-scanned id set.
        if (questionToolCallIds.has(ev.tool_call_id)) break;
        // ADR 0107: a plan's outcome is the card's receipt, not a tool row;
        // an out-of-mode attempt's outcome is its hint marker.
        if (planToolCallIds.has(ev.tool_call_id)) break;
        if (planAttemptToolCallIds.has(ev.tool_call_id)) break;
        const part = openTools.get(ev.tool_call_id);
        if (part) {
          part.result = ev.result_summary ?? undefined;
          part.isError = !ev.ok;
          openTools.delete(ev.tool_call_id);
        } else {
          // Completion without a matching start (replay edge) — emit a
          // standalone completed part.
          const a = ensureAssistant(ev.at);
          a.content.push({
            type: "tool-call",
            toolCallId: ev.tool_call_id,
            toolName: ev.tool_name,
            args: {},
            argsText: "",
            result: ev.result_summary ?? undefined,
            isError: !ev.ok,
          });
        }
        break;
      }

      case "exec_started": {
        bump("ran");
        const a = ensureAssistant(ev.at);
        const command = (ev.command ?? []).join(" ");
        const part: ToolPart = {
          type: "tool-call",
          toolCallId: ev.exec_id,
          toolName: SHELL_TOOL,
          args: { command } satisfies ShellArgs,
          argsText: command,
        };
        a.content.push(part);
        openExecs.set(ev.exec_id, part);
        break;
      }

      case "stdout":
      case "stderr": {
        const part = openExecs.get(ev.exec_id);
        if (part) part.result = `${(part.result as string | undefined) ?? ""}${ev.chunk}`;
        break;
      }

      case "exec_completed": {
        const part = openExecs.get(ev.exec_id);
        if (part) {
          const args = part.args as unknown as ShellArgs;
          args.exit = ev.exit_status;
          args.durationMs = ev.rusage?.wall_ms ?? null;
          part.isError = ev.exit_status != null && ev.exit_status !== 0;
          openExecs.delete(ev.exec_id);
        }
        break;
      }

      case "run_completed":
      case "run_interrupted": {
        const interrupted = ev.type === "run_interrupted";
        const ok = interrupted ? false : ev.ok;
        const footer: RunFooter = {
          reads: tally?.reads ?? 0,
          edits: tally?.edits ?? 0,
          ran: tally?.ran ?? 0,
          other: tally?.other ?? 0,
          ok,
          interrupted,
          endAt: ev.at,
        };
        // Attach the receipt to THIS run's assistant. When a deferred question
        // (or other trailing marker) ended the turn, `active` is null and the
        // run may have no assistant bubble at all — don't synthesize an empty
        // one just to hold a footer (it would render as a stray ✗ receipt).
        const a = active ?? runAssistant();
        if (a) {
          a.status = ok
            ? { type: "complete", reason: "stop" }
            : { type: "incomplete", reason: interrupted ? "cancelled" : "error" };
          a.metadata = { custom: { ...a.metadata?.custom, run: footer } };
        }
        active = null;
        runOpen = false;
        tally = null;
        break;
      }

      case "snapshot_taken":
        pushSystem(`snap:${idx}`, "snapshot taken", {
          kind: "durability",
          mark: "snapshot",
          sizeBytes: ev.size_bytes,
          at: ev.at,
        });
        break;

      case "resumed":
        // Resume completed — the transient "waking up" marker is done.
        // (run_started clears it too, for a resume that starts a run before
        // the Resumed event lands.)
        clearWaking();
        pushSystem(`res:${idx}`, "resumed", {
          kind: "durability",
          mark: "resumed",
          at: ev.at,
        });
        break;

      // The coordinator started waking the session up. Rendered as a faint,
      // TRANSIENT "waking up…" marker so a user who prompts an evicted
      // session sees progress during the multi-second restore. Collapse to a
      // single marker (drop any earlier one — a retrying resume emits
      // several) and let the superseding events below remove it once the
      // session is actually producing output.
      case "resume_started":
        clearWaking();
        pushSystem(`wake:${idx}`, "waking up", {
          kind: "durability",
          mark: "waking",
          at: ev.at,
        });
        wakingId = `wake:${idx}`;
        break;

      case "integration_asset": {
        const f = ev.fetchable;
        // Fallback text (single-part shape constraint) — a title if the
        // payload carries one, else the provider/kind pair.
        const fallback =
          typeof ev.data?.title === "string"
            ? (ev.data.title as string)
            : `${ev.provider} ${ev.asset_kind}`;
        pushSystem(`ia:${idx}`, fallback, {
          kind: "integration_asset",
          provider: ev.provider,
          assetKind: ev.asset_kind,
          surface: ev.surface,
          data: ev.data ?? {},
          fetchable:
            f == null
              ? null
              : f.kind === "external"
                ? { kind: "external", url: f.url }
                : {
                    kind: "artifact",
                    artifactId: f.artifact_id,
                    mediaType: f.media_type,
                    sizeBytes: f.size_bytes,
                  },
          at: ev.at,
        });
        break;
      }

      case "file_shared":
        pushSystem(`art:${idx}`, ev.caption ?? "shared a file", {
          kind: "artifact",
          sessionId,
          artifactId: ev.artifact_id,
          mediaType: ev.media_type,
          sizeBytes: ev.size_bytes,
          caption: ev.caption,
          at: ev.at,
        });
        break;

      case "recovered_from_checkpoint":
        pushSystem(`rec:${idx}`, "↩ recovered from a checkpoint", {
          kind: "recovery",
          rolledBack: ev.rolled_back,
          survivingSideEffects: ev.surviving_side_effects,
          // ADR 0045 F1: a missing cause (events predating the field)
          // reads as a host failure — the card's historical meaning.
          cause: ev.cause ?? "host_failure_recovery",
          at: ev.at,
        });
        break;

      case "harness_idle":
        active = null;
        break;

      // ADR 0090: the durability-rollback warning boundary. The next resume
      // rewound the disk to `manifest`, dropping acked-but-unuploaded writes.
      case "durability_rollback":
        pushSystem(`dr:${idx}`, "guest disk was rolled back", {
          kind: "durability_rollback",
          manifest: ev.rewind_disk_manifest
            ? `${ev.rewind_disk_manifest.manifest_id}@v${ev.rewind_disk_manifest.version}`
            : null,
          reason: ev.reason,
          at: ev.at,
        });
        break;

      // ADR 0054: the interactive question card. Ends the active assistant
      // turn (the run deferred here) and renders below the agent's reasoning.
      // The answer (if it has landed, in a later run) is folded in from the
      // pre-scanned map so a full rebuild shows the resolved card.
      case "user_question":
        pushSystem(`uq:${idx}`, "the agent asked a question", {
          kind: "user_question",
          toolCallId: ev.tool_call_id,
          questions: ev.questions,
          answers: answersByToolCallId.get(ev.tool_call_id) ?? null,
          via: "legacy",
          at: ev.at,
        });
        break;

      // ADR 0054: folded onto its `user_question` card (above) via the
      // pre-scan — no standalone render.
      case "question_answered":
        break;

      // ADR 0054 Flavor A: folded onto its originating tool-call part (the
      // FILE_CHANGE_TOOL swap in `tool_call_started`) via the pre-scan — no
      // standalone render.
      case "file_changed":
      case "browser_activity":
        break;

      // ADR 0052: the harness-owned queue, reflected up. A mid-turn prompt is
      // PromptQueued (then optionally PromptEdited), and leaves on
      // PromptDequeued (pulled back / cancelled) or when run_started consumes
      // it (above). The greyed user bubble is driven by the echo + run_started;
      // these maintain the recall/cancel queue.
      case "prompt_queued": {
        queued.set(ev.prompt_id, ev.summary ?? "");
        break;
      }
      case "prompt_edited": {
        if (queued.has(ev.prompt_id)) queued.set(ev.prompt_id, ev.summary ?? "");
        break;
      }
      case "prompt_dequeued": {
        queued.delete(ev.prompt_id);
        // Recalled before consumption (back in the composer / cancelled) — drop
        // the held text so it never enters the transcript.
        heldUserText.delete(ev.prompt_id);
        break;
      }
      case "prompt_steered":
        // The durable user echo is already present; this is only the
        // confirmation that the active agent turn consumed it.
        break;

      default:
        // status_changed, evicted, checkpoint_* — not surfaced; the RAW
        // tab shows them.
        break;
    }

    // ADR 0028 A.log: events tombstoned by a rung-1 rewind stay viewable but
    // greyed. Tag both any draft this event pushed and the assistant draft it
    // appended to (tool/message events accrue into `active` without pushing).
    // The recovery boundary itself is live (not flagged rewound), so it stays
    // full-opacity below the greyed span.
    if (indexed.rewound) {
      for (let i = lenBefore; i < out.length; i++) markRewound(out[i]!);
      if (active) markRewound(active);
    }
  }

  // Bug fix (mid-turn eviction): the session's authoritative status wins over
  // the event stream. When a session is evicted/terminated mid-run the server
  // emits no terminal run event, so `runOpen` / a trailing user turn would hang
  // the composer on "working…". An inactive session has no live run — finalize
  // any open assistant message as cut-short so its spinner clears, and veto
  // `isRunning` so the composer flips back to Send.
  const sessionInactive = status != null && INACTIVE_STATUSES.has(status);
  if (sessionInactive && runOpen) {
    for (let i = out.length - 1; i >= 0; i--) {
      const m = out[i]!;
      if (m.role === "assistant" && m.status?.type === "running") {
        m.status = { type: "incomplete", reason: "cancelled" };
        break;
      }
    }
    runOpen = false;
  }

  // Phase 1c: render the live token tail. Only while a run is genuinely open
  // (so a stale tail can't resurrect a finished turn); append it to the
  // active assistant message — creating one if the run just opened with no
  // parts yet — so it also serves as the working indicator. When the durable
  // `agent_message` lands the hook empties `streamingText`, and this turn's
  // text comes wholly from the coalesced durable event instead: same text,
  // same positional assistant bubble, no flash.
  if (streamingText && runOpen) {
    const a = ensureAssistant();
    const last = a.content[a.content.length - 1];
    if (last && last.type === "text") last.text += streamingText;
    else a.content.push({ type: "text", text: streamingText });
  }

  const isRunning = !sessionInactive && (runOpen || tailAwaiting(out));

  // Give the working indicator somewhere to live when we're running but the
  // tail isn't already a running assistant message (e.g. the user just sent
  // a prompt, or a fresh run opened with no parts yet).
  if (isRunning) {
    const last = out[out.length - 1];
    if (!(last && last.role === "assistant" && last.status?.type === "running")) {
      out.push({ role: "assistant", content: [], id: PENDING_ID, status: { type: "running" } });
    }
  }

  const queue: QueuedPrompt[] = [...queued].map(([promptId, summary]) => ({
    promptId,
    summary,
  }));

  // ADR 0107: the newest unresolved plan (order-independent via the
  // pre-scanned sets), suppressed on TERMINAL sessions — nothing can consume
  // a decision there. A parked/idle session is exactly where a plan waits.
  const sessionTerminal =
    status === "completed" || status === "failed" || status === "dead" || status === "host_lost";
  let pendingPlan: { toolCallId: string } | null = null;
  if (!sessionTerminal) {
    for (const { event } of events) {
      if (
        event.type === "tool_call_requested" &&
        planToolCallIds.has(event.tool_call_id) &&
        !planResolutionByToolCallId.has(event.tool_call_id)
      ) {
        pendingPlan = { toolCallId: event.tool_call_id };
      }
    }
  }

  // ADR 0107: latest mode directive, flipped back to the default by a later
  // plan approval (the harness stamps the same transition guest-side).
  let currentMode = "default";
  let currentModeIdx = -1;
  for (const { idx, event } of events) {
    if (event.type === "harness_mode_changed") {
      currentMode = event.mode;
      currentModeIdx = idx;
    } else if (
      event.type === "tool_result_submitted" &&
      planResolutionByToolCallId.get(event.tool_call_id)?.approved &&
      idx > currentModeIdx
    ) {
      currentMode = "default";
      currentModeIdx = idx;
    }
  }

  return { messages: out as ThreadMessageLike[], isRunning, queue, pendingPlan, currentMode };
}

// Walk back from the tail (skipping durability markers, which don't imply
// work in flight) to decide whether we're awaiting the assistant — mirrors
// Transcript's `isBusy`.
function tailAwaiting(out: Draft[]): boolean {
  for (let i = out.length - 1; i >= 0; i--) {
    const m = out[i]!;
    if (m.role === "system") {
      const marker = m.metadata?.custom?.marker as SystemMarker | undefined;
      if (marker?.kind === "durability") continue;
      return false;
    }
    if (m.role === "assistant") return m.status?.type === "running";
    if (m.role === "user") return true;
  }
  return false;
}
