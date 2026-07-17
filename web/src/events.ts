/**
 * SessionEvent union, IndexedEvent, and related types — the canonical
 * shape of events that flow from the orchestrator SSE feed into the UI.
 *
 * Moved from web/src/types.ts (Task 26) so the SSE parse layer and its
 * contract test can import from here without pulling in the full types
 * module. types.ts re-exports everything for backward compatibility.
 *
 * Shape source of truth:
 *   crates/engram-coordinator/src/state.rs   (SessionEvent enum)
 *   crates/engram-harness-proto/src/lib.rs   (AgentRole)
 */

import type { SessionState } from "./lib/types";

export type AgentRole = "assistant" | "user" | "system";

export interface ExecRusage {
  duration_ms: number;
  // Other rusage fields exist on the wire but the UI doesn't read them.
  [k: string]: unknown;
}

// ADR 0054: one clarifying question the agent asked via `AskUserQuestion`,
// carried on a `user_question` event. Mirrors `engram_harness_proto::Question`
// — note `multiSelect` is camelCase on the wire (the Rust field carries
// `#[serde(rename = "multiSelect")]` to stay byte-faithful to Claude's
// `tool_input.questions[]`).
export interface UserQuestionOption {
  label: string;
  description: string;
}
export interface UserQuestion {
  /** The full question text — ALSO the key in the answers map (finding #8). */
  question: string;
  /** Short column header (a few words) labelling this question. */
  header: string;
  /** true → the user may pick several options; false → exactly one. */
  multiSelect: boolean;
  options: UserQuestionOption[];
}

// ADR 0054 Flavor A: what changed about a file, carried on a `file_changed`
// event. Mirrors `engram_harness_proto::FileChange` — **externally tagged**
// (a single `write`/`edit` key), NOT an `op` discriminant, because the Rust
// type rides the bincode harness wire where internally-tagged enums panic at
// decode. The web discriminates by which key is present.
export interface EditHunk {
  /** The replaced text (Claude's `old_string`); empty for a pure insertion. */
  old: string;
  /** The replacement text (Claude's `new_string`). */
  new: string;
}
export type FileChange =
  | { write: { content: string }; edit?: undefined }
  | { edit: { hunks: EditHunk[] }; write?: undefined }
  | { patch: { unified_diff: string }; write?: undefined; edit?: undefined };

// `serde(tag = "type", rename_all = "snake_case")` produces a discriminated
// union with `type` as the discriminant.
export type SessionEvent =
  | {
      type: "status_changed";
      from: SessionState;
      to: SessionState;
      at: string;
    }
  | {
      type: "exec_started";
      exec_id: string;
      command: string[];
      at: string;
    }
  | {
      type: "exec_completed";
      exec_id: string;
      exit_status: number | null;
      rusage: ExecRusage;
      at: string;
    }
  | { type: "stdout"; exec_id: string; chunk: string }
  | { type: "stderr"; exec_id: string; chunk: string }
  | {
      type: "snapshot_taken";
      snapshot_id: string;
      size_bytes: number;
      at: string;
    }
  | { type: "evicted"; at: string }
  | { type: "resumed"; snapshot_id: string; at: string }
  // The coordinator started waking an idle/parked session back up, before
  // the multi-second restore + harness reattach. Rendered as a transient
  // "waking up…" indicator that resolves when the first run event lands.
  | { type: "resume_started"; at: string }
  | {
      type: "run_started";
      run_id: string;
      prompt_summary: string | null;
      // Phase 1b: client-minted id of the prompt that started this run.
      // The "queued prompt consumed" signal — a greyed/pending user
      // bubble with this prompt_id transitions to solid (ungreys) here.
      // Optional: events persisted before Phase 1b omit it.
      prompt_id?: string | null;
      at: string;
    }
  | {
      type: "agent_message";
      run_id: string;
      message_id: string;
      role: AgentRole;
      text: string;
      // Phase 1b: set on the coord's `role:user` echo to the client
      // prompt_id, so the optimistic bubble dedupes against it. Null for
      // assistant/system messages; absent on events persisted pre-Phase-1b.
      prompt_id?: string | null;
      at: string;
    }
  // Phase 1c (ADR 0052): one live token delta of the in-flight assistant
  // message. EPHEMERAL — streamed over SSE with NO `idx` (never persisted,
  // never replayed on reconnect); the web accumulates it into a live
  // overlay keyed on `message_id` and the terminal `agent_message` (same
  // id) supersedes it. Routed via `onDelta`, NOT the durable event array.
  | {
      type: "agent_message_chunk";
      run_id: string;
      message_id: string;
      chunk: string;
      at: string;
    }
  | {
      type: "tool_call_started";
      run_id: string;
      tool_call_id: string;
      tool_name: string;
      args_summary: string | null;
      at: string;
    }
  | {
      type: "tool_call_completed";
      run_id: string;
      tool_call_id: string;
      tool_name: string;
      ok: boolean;
      duration_ms: number;
      result_summary: string | null;
      at: string;
    }
  | {
      type: "browser_activity";
      run_id: string;
      tool_call_id: string;
      intent: string;
      at: string;
    }
  // ADR 0089: an orchestrator-registered tool was invoked. `args_json` is
  // deliberately opaque JSON text; tool-specific presenters parse it.
  | {
      type: "tool_call_requested";
      run_id: string;
      tool_call_id: string;
      name: string;
      args_json: string;
      at: string;
    }
  // ADR 0089: the coordinator synchronously accepted a result for delivery.
  // Surfaces resolve pending UI from this event without waiting for the harness.
  | {
      type: "tool_result_submitted";
      tool_call_id: string;
      result_json: string;
      at: string;
    }
  | { type: "run_completed"; run_id: string; ok: boolean; at: string }
  // ADR 0030: the in-flight run was stopped by an operator interrupt
  // (`POST /sessions/:id/interrupt`). The session stays alive; the
  // transcript renders an "interrupted" receipt and the run closes.
  | { type: "run_interrupted"; run_id: string; at: string }
  | { type: "harness_idle"; at: string }
  // ADR 0054 legacy read shape: pre-upgrade sessions may contain this durable
  // question card. Unanswered cards are read-only after ADR 0089 P5d; answered
  // cards still fold in their historical `question_answered` receipt.
  | {
      type: "user_question";
      run_id: string;
      tool_call_id: string;
      questions: UserQuestion[];
      at: string;
    }
  // ADR 0054 legacy read shape: the deferred question was answered — the harness held the
  // answer and is feeding it back on the `--resume` re-fire. Resolves the
  // card (same `tool_call_id`); `answers` is keyed by question text, values
  // are the selected option labels (1 for single-select, N for multi).
  | {
      type: "question_answered";
      run_id: string;
      tool_call_id: string;
      answers: Record<string, string[]>;
      at: string;
    }
  // ADR 0054 Flavor A: the agent successfully changed a file via a
  // Write/Edit/MultiEdit tool. Correlated to the originating tool call by
  // `tool_call_id`; the web renders a rich diff (Pierre) in place of that
  // tool's generic card.
  | {
      type: "file_changed";
      run_id: string;
      tool_call_id: string;
      path: string;
      change: FileChange;
      at: string;
    }
  // Phase 1b (ADR 0052): a prompt arrived mid-run and was queued
  // (type-ahead / steering). Rendered as a greyed, editable composer
  // item keyed on prompt_id until run_started{prompt_id} consumes it.
  | { type: "prompt_queued"; prompt_id: string; summary: string | null; at: string }
  | { type: "prompt_edited"; prompt_id: string; summary: string | null; at: string }
  | { type: "prompt_dequeued"; prompt_id: string; at: string }
  | { type: "prompt_steered"; prompt_id: string; at: string }
  // ADR 0056: a third-party integration surfaced a typed asset/action.
  // Subsumes the old `pull_request_opened` (a PR is provider:"forge",
  // asset_kind:"pull_request"). The wire is semantic-only — the web keys its
  // renderer on (provider, asset_kind), with a generic fallback; the payload
  // never carries rendering instructions.
  | {
      type: "integration_asset";
      provider: string;
      asset_kind: string;
      surface: "action" | "asset";
      data: Record<string, unknown>;
      fetchable:
        | { kind: "external"; url: string }
        | { kind: "artifact"; artifact_id: string; media_type: string; size_bytes: number }
        | null;
      at: string;
    }
  // ADR 0026: a file artifact (agent screenshot/recording, or an
  // operator file pull) shared into the session. `media_type` is the
  // coord-detected type; the transcript renders image/video inline and
  // anything else as a download chip.
  | {
      type: "file_shared";
      artifact_id: string;
      media_type: string;
      size_bytes: number;
      caption: string | null;
      at: string;
    }
  // ADR 0028 A.log: a rung-1 recovery rewound the live transcript to a
  // checkpoint. The boundary the transcript renders ("↩ Recovered from
  // a checkpoint…"); `rolled_back` events with idx > through_idx are
  // tombstoned (rendered collapsed/greyed). `surviving_side_effects`
  // are outside-world actions in the rolled-back span the platform
  // can't undo (opened PRs, shared files) — surfaced, not hidden.
  | {
      type: "recovered_from_checkpoint";
      recovery_epoch: number;
      through_idx: number;
      rolled_back: number;
      surviving_side_effects: string[];
      // ADR 0045 F1: why the rewind happened — `planned_relocation`
      // (operator drain / teleport, no host failed) vs the original
      // `host_failure_recovery`. Optional: events persisted before this
      // field omit it, and the renderer treats a missing value as a
      // host failure (the card's historical meaning).
      cause?: "planned_relocation" | "host_failure_recovery" | "checkpoint_lag";
      at: string;
    }
  // Session titles: the harness proposed an LLM-generated title. Not rendered
  // in the transcript — it drives the session's display title (materialized on
  // the task via the coordinator + the 1s ListTasks poll). Typed here so the
  // frame is a known kind, not an untyped passthrough.
  | { type: "title_suggested"; title: string; at: string };

export type SessionEventKind = SessionEvent["type"];

/**
 * An event together with its monotonic per-session index (the SSE id).
 *
 * ADR 0028 A.log: `rewound` marks events tombstoned by a rung-1
 * recovery (kept for audit, rendered collapsed/greyed). `recoveryEpoch`
 * segments the transcript across recoveries. Both default to
 * not-rewound / epoch 0 for the common no-recovery case.
 */
export interface IndexedEvent {
  idx: number;
  event: SessionEvent;
  rewound?: boolean;
  recoveryEpoch?: number;
}

// ---------------------------------------------------------------------------
// Orchestrator SSE envelope
// ---------------------------------------------------------------------------

/**
 * The raw JSON frame the orchestrator emits on its SSE data: line.
 * Snake_case — deliberately NOT protobuf-JSON.
 *
 * Emitted by orchestrator/src/routes/events.ts (Task 20):
 *   { idx: number|null, kind: string, payload_json: string }
 *
 * `payload_json` is itself a JSON string. When parsed it yields the
 * SessionEvent fields PLUS the ADR 0028 rewind metadata
 * (_rewound: boolean, _recovery_epoch: number) folded in by
 * crates/engram-coordinator/src/api/events.rs :: with_rewind_meta.
 */
export interface OrchestratorSseEnvelope {
  idx: number | null;
  kind: string;
  payload_json: string;
}

/**
 * Parse one raw SSE MessageEvent from the orchestrator feed into an
 * IndexedEvent for the UI, or return null if the frame should be ignored
 * (ping, unknown kind, malformed data).
 *
 * Exported so events.contract.test.ts can exercise it directly against
 * pinned fixture strings without mounting a full EventSource.
 *
 * Parsing contract:
 *   1. JSON.parse(frame.data) → OrchestratorSseEnvelope
 *   2. JSON.parse(envelope.payload_json) → raw payload object
 *   3. Lift _rewound + _recovery_epoch → IndexedEvent metadata; delete
 *      from payload so SessionEvent stays clean (same as the old
 *      coordinator-direct parse in sse.ts).
 *   4. Reconstruct IndexedEvent with idx from the envelope (not from
 *      ev.lastEventId — the envelope idx is authoritative and works in
 *      tests without a real EventSource).
 *   5. "lagged" kind → return null (caller invokes onLagged separately).
 *   6. idx === null / non-numeric idx (contract-breaking envelope) →
 *      return null.
 *   7. Unknown event kinds are NOT filtered here — the EventSource layer
 *      only dispatches kinds that have registered listeners, so a new
 *      server kind is dropped there (same as the legacy wire).
 */
export function parseOrchestratorFrame(frameData: string, kind: string): IndexedEvent | null {
  // Ping keepalives and lagged frames are not IndexedEvents.
  if (kind === "ping" || kind === "lagged") return null;

  let envelope: OrchestratorSseEnvelope;
  try {
    envelope = JSON.parse(frameData) as OrchestratorSseEnvelope;
  } catch {
    return null;
  }

  // Lagged frames have idx === null; they're handled by the lagged listener.
  // Non-numeric idx = contract-breaking envelope — drop it (mirrors the old
  // dispatch's Number.isFinite guard) while keeping idx 0 valid.
  if (typeof envelope.idx !== "number" || !Number.isFinite(envelope.idx)) return null;

  let raw: Record<string, unknown>;
  try {
    raw = JSON.parse(envelope.payload_json) as Record<string, unknown>;
  } catch {
    return null;
  }

  // ADR 0028 A.log: lift rewind metadata folded in by with_rewind_meta
  // (crates/engram-coordinator/src/api/events.rs). Strip them so the
  // typed SessionEvent payload stays clean.
  const rewound = raw._rewound === true;
  const recoveryEpoch = typeof raw._recovery_epoch === "number" ? raw._recovery_epoch : 0;
  delete raw._rewound;
  delete raw._recovery_epoch;

  // The coordinator sets `type` in the payload; ensure it matches the
  // SSE event: field (defensive — be explicit about the discriminant).
  const payload = { ...raw, type: kind } as SessionEvent;

  return {
    idx: envelope.idx,
    event: payload,
    rewound,
    recoveryEpoch,
  };
}
