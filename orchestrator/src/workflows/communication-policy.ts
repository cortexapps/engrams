/**
 * The per-source communication seam (ADR 0060 §Communication policy, P1.4).
 *
 * "Where does this go" is CODE, not config. The thread workflow (the
 * framework) owns event classification, identity, session create/resume, the
 * recv/drain loop, and outbound replay-dedupe (DBOS step checkpoints). A
 * `CommunicationPolicy` owns ONLY the provider mechanics — how an ack, a
 * question, an asset, a summary, or a failure is rendered on the source, and
 * how a thread's new messages are gathered into a prompt. Slack is the only
 * impl for v1 (P2); Linear/Jira/cron are designed-for (§Future).
 *
 * This module is framework-side and DBOS-free: the interface, the pure
 * event-routing decision the workflow makes per event, and a fake for tests.
 */

import type { CuratedEvent } from "../control-plane/session-events.ts";
import type { SourceMention } from "./thread-inbox.ts";

/** A started session, as the thread workflow needs to reference it. */
export interface StartedSession {
  id: string;
  webUrl: string;
}

/** A durable asset the session produced (a PR, a shared file), collapsed to one
 *  recap line for the closing summary. `url` is set only for assets with an
 *  external link (a PR page); files are reached via the session web URL. */
export interface AssetSummary {
  label: string;
  url?: string;
}

/** What `onComplete` renders: the session's final assistant message (if any)
 *  plus the durable assets it produced, in arrival order. */
export interface ClosingSummary {
  lastMessage: string | null;
  assets: AssetSummary[];
}

/**
 * The provider seam. Every method is invoked by the framework as a checkpointed
 * DBOS step, so a completed effect is never re-run on replay. `onUserQuestion`
 * returns a provider message ref (e.g. a Slack `ts`) the framework holds in
 * workflow-local state to later update via `onAnswered`.
 */
export interface CommunicationPolicy {
  /** Constant flavor appended to the agent's system prompt at session create
   *  (ADR 0060 Decision 8) — NOT connector config. */
  readonly systemPromptAppend: string;

  /** The trigger was picked up (Slack: 👀 on the mention). */
  onPickup(m: SourceMention): Promise<void>;
  /** The session started (Slack: a link message into the thread). */
  onStarted(m: SourceMention, session: StartedSession): Promise<void>;
  /** A run began on `m`'s turn — the agent is working (Slack: ⏳ on the message). */
  onWorking(m: SourceMention): Promise<void>;
  /** A run finished — the turn is done and the session is idle, waiting for the
   *  user (Slack: clear the ⏳ and add ✅ on `m`). This is the per-turn ack and
   *  the "your turn" indicator the static delivered-✅ couldn't express. */
  onIdle(m: SourceMention): Promise<void>;
  /** Render or extend the turn's running assistant message with `text` (the full
   *  accumulated text the framework coalesced); `ref` is the existing message to
   *  edit, or undefined to post a new one. Returns the (new or same) message ref. */
  onAssistantMessage(m: SourceMention, text: string, ref: string | undefined): Promise<string>;
  /** Render an `AskUserQuestion`; return the provider message ref. */
  onUserQuestion(m: SourceMention, ev: CuratedEvent): Promise<string>;
  /** A question was answered; `ref` is the value `onUserQuestion` returned. */
  onAnswered(m: SourceMention, ev: CuratedEvent, ref: string | undefined): Promise<void>;
  /** Render an asset (a PR `integration_asset` or a `file_shared` artifact). The
   *  session is passed so a `file_shared` artifact can be fetched by id and
   *  uploaded to the source (and so the fallback can link to the session). */
  onAsset(m: SourceMention, ev: CuratedEvent, session: StartedSession): Promise<void>;
  /** The session completed successfully — posts a closing summary enriched with
   *  the session's last assistant message + a recap of the assets it produced. */
  onComplete(m: SourceMention, session: StartedSession, summary: ClosingSummary): Promise<void>;
  /** A failure (identity, create, or terminal-failure) — ❌ + actionable text. */
  onFail(m: SourceMention, message: string): Promise<void>;
  /** Gather thread messages after `since` (null = the whole thread) into a
   *  prompt; `maxTs` is the newest message seen, the next `since`. */
  gatherThreadContext(m: SourceMention, since: string | null): Promise<{ prompt: string; maxTs: string }>;
}

/** The effect a curated session event maps to — the framework's per-event
 *  classification, consumed by the thread workflow's dispatch. */
export type SessionEffect =
  | { kind: "question"; toolCallId: string | undefined }
  | { kind: "answered"; toolCallId: string | undefined }
  | { kind: "asset" }
  /** Assistant text — coalesced into the turn's running thread message. */
  | { kind: "message"; text: string }
  /** A run began (agent is working) / finished (idle, waiting for the user). */
  | { kind: "working" }
  | { kind: "idle" }
  | { kind: "ignore" };

/**
 * Decide which policy method a curated session event drives. Pure. Curated
 * kinds that render no thread effect (`run_started`/`run_completed`) map to
 * `ignore` — `run_completed` is NOT terminal (Invariant 2); the closing
 * summary is driven by `session_terminal`, not here.
 */
export function routeSessionEvent(ev: CuratedEvent): SessionEffect {
  switch (ev.kind) {
    case "user_question":
      return { kind: "question", toolCallId: parseToolCallId(ev.payloadJson) };
    case "question_answered":
      return { kind: "answered", toolCallId: parseToolCallId(ev.payloadJson) };
    case "integration_asset":
    case "file_shared":
      return { kind: "asset" };
    case "agent_message":
      return { kind: "message", text: parseMessageText(ev.payloadJson) };
    case "run_started":
      return { kind: "working" };
    case "run_completed":
      return { kind: "idle" };
    default:
      return { kind: "ignore" };
  }
}

/** Extract the assistant text from an `agent_message` payload (empty if absent
 *  or unparseable — the event still routes, it just renders nothing). */
function parseMessageText(payloadJson: string): string {
  try {
    const t: unknown = (JSON.parse(payloadJson) as { text?: unknown })?.text;
    return typeof t === "string" ? t : "";
  } catch {
    return "";
  }
}

/** Extract the `tool_call_id` correlation token from a question payload. */
function parseToolCallId(payloadJson: string): string | undefined {
  try {
    const id: unknown = (JSON.parse(payloadJson) as { tool_call_id?: unknown })?.tool_call_id;
    return typeof id === "string" ? id : undefined;
  } catch {
    return undefined;
  }
}

/** The asset/file payload shapes the recap reads (a structural slice of the
 *  coordinator's `integration_asset` / `file_shared` events). */
interface AssetPayload {
  provider?: string;
  asset_kind?: string;
  surface?: string;
  data?: { number?: unknown; title?: unknown };
  fetchable?: { kind?: string; url?: string } | null;
  caption?: string;
}

/**
 * Collapse a curated asset event into a one-line recap entry, or null if it
 * isn't a durable asset worth recapping. Pure. A transient
 * `integration_asset` with `surface:"action"` (a verb the agent ran, not a
 * surviving side-effect — ADR 0056) is NOT recapped.
 */
export function summarizeAsset(ev: CuratedEvent): AssetSummary | null {
  let p: AssetPayload;
  try {
    p = JSON.parse(ev.payloadJson) as AssetPayload;
  } catch {
    return null;
  }

  if (ev.kind === "file_shared") {
    return { label: typeof p.caption === "string" && p.caption ? p.caption : "shared a file" };
  }

  if (ev.kind === "integration_asset") {
    if (p.surface === "action") return null; // transient, not a recap asset
    const url = p.fetchable?.kind === "external" ? p.fetchable.url : undefined;
    const provider = typeof p.provider === "string" ? p.provider : "asset";
    const kind = typeof p.asset_kind === "string" ? p.asset_kind : "";
    const label =
      kind === "pull_request"
        ? `PR #${String(p.data?.number ?? "")}: ${String(p.data?.title ?? "")}`.trim()
        : `${provider} ${kind}`.trim();
    return url ? { label, url } : { label };
  }

  return null;
}
