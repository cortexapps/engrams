// Mirrors the wire format produced by engram-coordinator. Hand-written
// rather than generated — kept narrow to what the UI consumes.
//
// Shapes traced from:
//   crates/engram-core/src/types/session.rs       (Session, SessionStatus)
//   crates/engram-coordinator/src/api/hosts.rs    (HostView, WarmPoolView)
//   crates/engram-coordinator/src/state.rs        (SessionEvent enum)
//   crates/engram-harness-proto/src/lib.rs        (AgentRole)

export type SessionStatus =
  | 'pending'
  | 'active'
  | 'idle'
  | 'completed'
  | 'failed'
  | 'dead';

export type SessionKind = 'git' | 'local' | 'readonly';

export type RepoUrl =
  | { kind: 'git'; url: string }
  | { kind: 'local'; name: string };

export interface Session {
  id: string;
  repo: string;
  branch: string;
  user_id: string | null;
  status: SessionStatus;
  image_version: string;
  host_id: string | null;
  sandbox_id: string | null;
  session_kind: SessionKind;
  repo_url: RepoUrl | null;
  checkpoint_branch: string | null;
  created_at: string;
  last_active_at: string;
}

export interface ListSessionsResponse {
  sessions: Session[];
}

export interface WarmPoolView {
  repo: string;
  image_version: string;
  ready: number;
  target: number;
}

export type HostStatus = 'ready' | 'draining' | 'dead';

export interface HostView {
  id: string;
  hostname: string;
  status: HostStatus;
  capacity_total_mib: number;
  capacity_used_mib: number;
  running_sandboxes: number;
  warm_pools: WarmPoolView[];
  local_snapshots: number;
  last_heartbeat_at: string;
}

export interface ListHostsResponse {
  hosts: HostView[];
}

// ---- Session events (SSE) ----------------------------------------------

export type AgentRole = 'assistant' | 'user' | 'system';

export interface ExecRusage {
  duration_ms: number;
  // Other rusage fields exist on the wire but the UI doesn't read them.
  [k: string]: unknown;
}

// `serde(tag = "type", rename_all = "snake_case")` produces a discriminated
// union with `type` as the discriminant.
export type SessionEvent =
  | {
      type: 'status_changed';
      from: SessionStatus;
      to: SessionStatus;
      at: string;
    }
  | {
      type: 'exec_started';
      exec_id: string;
      command: string[];
      at: string;
    }
  | {
      type: 'exec_completed';
      exec_id: string;
      exit_status: number | null;
      rusage: ExecRusage;
      at: string;
    }
  | { type: 'stdout'; exec_id: string; chunk: string }
  | { type: 'stderr'; exec_id: string; chunk: string }
  | {
      type: 'snapshot_taken';
      snapshot_id: string;
      size_bytes: number;
      at: string;
    }
  | { type: 'evicted'; at: string }
  | { type: 'resumed'; snapshot_id: string; at: string }
  | {
      type: 'checkpoint_pushed';
      commit_sha: string;
      harness_acked: boolean;
      at: string;
    }
  | { type: 'checkpoint_failed'; reason: string; at: string }
  | {
      type: 'run_started';
      run_id: string;
      prompt_summary: string | null;
      at: string;
    }
  | {
      type: 'agent_message';
      run_id: string;
      message_id: string;
      role: AgentRole;
      text: string;
      at: string;
    }
  | {
      type: 'tool_call_started';
      run_id: string;
      tool_call_id: string;
      tool_name: string;
      args_summary: string | null;
      at: string;
    }
  | {
      type: 'tool_call_completed';
      run_id: string;
      tool_call_id: string;
      tool_name: string;
      ok: boolean;
      duration_ms: number;
      result_summary: string | null;
      at: string;
    }
  | { type: 'run_completed'; run_id: string; ok: boolean; at: string }
  | { type: 'harness_idle'; at: string };

export type SessionEventKind = SessionEvent['type'];

/** An event together with its monotonic per-session index (the SSE id). */
export interface IndexedEvent {
  idx: number;
  event: SessionEvent;
}
