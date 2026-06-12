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

import type { SessionState } from "./types";

export type AgentRole = "assistant" | "user" | "system";

export interface ExecRusage {
  duration_ms: number;
  // Other rusage fields exist on the wire but the UI doesn't read them.
  [k: string]: unknown;
}

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
  | {
      type: "run_started";
      run_id: string;
      prompt_summary: string | null;
      at: string;
    }
  | {
      type: "agent_message";
      run_id: string;
      message_id: string;
      role: AgentRole;
      text: string;
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
  | { type: "run_completed"; run_id: string; ok: boolean; at: string }
  // ADR 0030: the in-flight run was stopped by an operator interrupt
  // (`POST /sessions/:id/interrupt`). The session stays alive; the
  // transcript renders an "interrupted" receipt and the run closes.
  | { type: "run_interrupted"; run_id: string; at: string }
  | { type: "harness_idle"; at: string }
  | {
      type: "pull_request_opened";
      url: string;
      repo: string;
      title: string;
      number: number;
      head_branch: string;
      base_branch: string;
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
      cause?: "planned_relocation" | "host_failure_recovery";
      at: string;
    };

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
 *   6. idx === null (lagged sentinel) → return null.
 *   7. Unknown / ping kind that is not in the known SessionEventKind set
 *      → return null (transparent to consumers).
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
  if (envelope.idx === null) return null;

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
