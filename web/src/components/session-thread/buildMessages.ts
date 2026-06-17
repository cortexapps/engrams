import type { ThreadMessageLike } from "@assistant-ui/react";
import type { AgentRole, IndexedEvent, SessionState } from "../../lib/types";

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

/** Payload carried in a system message's `metadata.custom.marker` — the
 *  harness-register events that aren't agent messages. */
export type SystemMarker =
  | { kind: "durability"; mark: "snapshot" | "resumed"; sizeBytes?: number; at: string }
  | {
      kind: "pull_request";
      url: string;
      repo: string;
      title: string;
      number: number;
      headBranch: string;
      baseBranch: string;
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
  let out: Draft[] = [];

  // The assistant message currently accumulating this run's parts, or null
  // between runs / after a harness marker breaks the flow.
  let active: Draft | null = null;
  const openTools = new Map<string, ToolPart>(); // tool_call_id → part
  const openExecs = new Map<string, ToolPart>(); // exec_id → part
  // Phase 1b: user-turn drafts keyed by prompt_id. A user message is
  // rendered "pending" (greyed) from the moment its `role:user` echo
  // lands until its `run_started{prompt_id}` consumes it — covering both
  // the send→echo gap and a type-ahead message that's still queued.
  const userDraftByPromptId = new Map<string, Draft>();

  // ADR 0052: the harness-owned queue, mirrored from events. prompt_id →
  // summary, insertion-ordered (oldest→newest). Entries leave on
  // run_started (consumed), prompt_dequeued, or are updated by prompt_edited.
  const queued = new Map<string, string>();
  // prompt_ids whose greyed user bubble was pulled out of the thread by a
  // dequeue (recalled to the composer / cancelled) — filtered from `out`.
  const dequeuedPromptIds = new Set<string>();

  // Is a run in flight (run_started seen, no run_completed/_interrupted yet)?
  let runOpen = false;

  // Per-run tally feeding the assistant-message footer.
  let tally: { reads: number; edits: number; ran: number; other: number } | null = null;
  const bump = (k: "reads" | "edits" | "ran" | "other") => {
    if (tally) tally[k] += 1;
  };

  const ensureAssistant = (at?: string): Draft => {
    if (active) return active;
    active = {
      role: "assistant",
      content: [],
      id: `a:${out.length}`,
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

  for (const indexed of events) {
    const { idx, event: ev } = indexed;
    const lenBefore = out.length;
    switch (ev.type) {
      case "run_started": {
        tally = { reads: 0, edits: 0, ran: 0, other: 0 };
        runOpen = true;
        active = null;
        // Most harnesses carry the prompt on a preceding user agent_message
        // (our claude harness sends a null prompt_summary). When a summary
        // IS present, surface it as the user turn.
        if (ev.prompt_summary) {
          out.push({
            role: "user",
            content: [{ type: "text", text: ev.prompt_summary }],
            id: `rs:${idx}`,
            createdAt: new Date(ev.at),
          });
        }
        // Phase 1b: the prompt that started this run is now consumed —
        // un-grey its pending user bubble (optimistic/queued → solid).
        if (ev.prompt_id) {
          const d = userDraftByPromptId.get(ev.prompt_id);
          if (d?.metadata?.custom) delete d.metadata.custom.pending;
          userDraftByPromptId.delete(ev.prompt_id);
          queued.delete(ev.prompt_id); // consumed → leaves the queue
        }
        break;
      }

      case "agent_message": {
        if (ev.role === "user") {
          active = null;
          // Phase 1b: a `prompt_id` ties this echo to the optimistic
          // bubble (dedup, same id) and marks it "pending" (greyed) until
          // its run_started consumes it. Echoes without a prompt_id (e.g.
          // the env-seeded initial prompt) render solid as before.
          const draft: Draft = {
            role: "user",
            content: [{ type: "text", text: ev.text }],
            id: ev.prompt_id ?? `m:${idx}`,
            createdAt: new Date(ev.at),
          };
          if (ev.prompt_id) {
            draft.metadata = { custom: { pending: true } };
            userDraftByPromptId.set(ev.prompt_id, draft);
          }
          out.push(draft);
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
        bump(classifyTool(ev.tool_name));
        const a = ensureAssistant(ev.at);
        const part: ToolPart = {
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
        const a = ensureAssistant(ev.at);
        a.status = ok
          ? { type: "complete", reason: "stop" }
          : { type: "incomplete", reason: interrupted ? "cancelled" : "error" };
        a.metadata = { custom: { ...(a.metadata?.custom ?? {}), run: footer } };
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

      case "pull_request_opened":
        pushSystem(`pr:${idx}`, `opened PR #${ev.number}: ${ev.title}`, {
          kind: "pull_request",
          url: ev.url,
          repo: ev.repo,
          title: ev.title,
          number: ev.number,
          headBranch: ev.head_branch,
          baseBranch: ev.base_branch,
          at: ev.at,
        });
        break;

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
        // The greyed bubble (keyed by prompt_id) leaves the thread — it's back
        // in the composer being edited, or cancelled.
        dequeuedPromptIds.add(ev.prompt_id);
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

  // ADR 0052: drop greyed bubbles pulled out of the thread by a dequeue
  // (recalled to the composer / cancelled). Their id IS the prompt_id.
  if (dequeuedPromptIds.size) {
    out = out.filter((d) => !dequeuedPromptIds.has(d.id));
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
