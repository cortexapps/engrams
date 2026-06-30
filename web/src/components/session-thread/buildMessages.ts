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

/** Payload carried in a system message's `metadata.custom.marker` — the
 *  harness-register events that aren't agent messages. */
export type SystemMarker =
  | { kind: "durability"; mark: "snapshot" | "resumed"; sizeBytes?: number; at: string }
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
      planned: boolean;
      at: string;
    };

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

  // ADR 0028 A.log: stamp `rewound` into the custom metadata of every draft
  // an event touched, so the rolled-back span renders greyed. Preserves any
  // existing custom payload (run footer, marker).
  const markRewound = (d: Draft) => {
    d.metadata = { custom: { ...(d.metadata?.custom ?? {}), rewound: true } };
  };

  // ADR 0054: AskUserQuestion is observed TWICE on the wire — as a generic
  // `tool_call_started` (the harness translates every `tool_use` block) AND
  // as the dedicated `user_question`/`question_answered` pair. Pre-scan so we
  // can (a) suppress the generic tool part for those tool_call_ids — the
  // interactive card is the canonical render — and (b) fold the answer onto
  // the card even though it arrives in a LATER run (the deferred tool re-fires
  // on `--resume`, so `question_answered` lands after a fresh run_started).
  const questionToolCallIds = new Set<string>();
  const answersByToolCallId = new Map<string, Record<string, string[]>>();
  // ADR 0054 Flavor A: a Write/Edit/MultiEdit tool call emits a generic
  // tool_call_started AND (on success) a `file_changed` carrying the diff. We
  // pre-scan so the generic card is re-rendered as a rich diff in place; a
  // failed edit emits NO file_changed and keeps its generic (error) card.
  const fileChangesByToolCallId = new Map<string, FileChangeArgs>();
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
    else if (event.type === "question_answered")
      answersByToolCallId.set(event.tool_call_id, event.answers);
    else if (event.type === "file_changed")
      fileChangesByToolCallId.set(event.tool_call_id, {
        path: event.path,
        change: event.change,
      });
    else if (event.type === "agent_message" && event.role === "user" && event.prompt_id)
      userEchoByPromptId.set(event.prompt_id, event.text);
  }

  for (const indexed of events) {
    const { idx, event: ev } = indexed;
    const lenBefore = out.length;
    switch (ev.type) {
      case "run_started": {
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
        // ADR 0054: the AskUserQuestion call renders as the interactive
        // `user_question` card, not a generic tool part — drop the duplicate
        // (and don't tally it as a tool run).
        if (ev.tool_name === "AskUserQuestion" || questionToolCallIds.has(ev.tool_call_id)) break;
        bump(classifyTool(ev.tool_name));
        const a = ensureAssistant(ev.at);
        // ADR 0054 Flavor A: a Write/Edit/MultiEdit that produced a successful
        // `file_changed` renders as a rich diff (the `FILE_CHANGE_TOOL` part)
        // in place of the generic card. The tally still counts the ORIGINAL
        // tool (an edit), and the part keeps its real id so the completion
        // correlates as usual. No file_changed (e.g. a failed edit) → generic.
        const fc = fileChangesByToolCallId.get(ev.tool_call_id);
        const part: ToolPart = fc
          ? {
              type: "tool-call",
              toolCallId: ev.tool_call_id,
              toolName: FILE_CHANGE_TOOL,
              args: fc as unknown as Record<string, unknown>,
              argsText: fc.path,
            }
          : {
              type: "tool-call",
              toolCallId: ev.tool_call_id,
              toolName: ev.tool_name,
              args: parseArgs(ev.args_summary),
              argsText: ev.args_summary ?? "",
            };
        a.content.push(part);
        openTools.set(ev.tool_call_id, part);
        break;
      }

      case "tool_call_completed": {
        // ADR 0054: the answered AskUserQuestion's tool_result (it re-fired on
        // resume) — its outcome is the card, not a tool part. `tool_name` is
        // blank on completed events, so match on the pre-scanned id set.
        if (questionToolCallIds.has(ev.tool_call_id)) break;
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
          args.durationMs = ev.rusage?.duration_ms ?? null;
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
          a.metadata = { custom: { ...(a.metadata?.custom ?? {}), run: footer } };
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
        pushSystem(`res:${idx}`, "resumed", {
          kind: "durability",
          mark: "resumed",
          at: ev.at,
        });
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
          planned: ev.cause === "planned_relocation",
          at: ev.at,
        });
        break;

      case "harness_idle":
        active = null;
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

  return { messages: out as ThreadMessageLike[], isRunning, queue };
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
