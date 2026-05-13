// Mirrors the wire format produced by engram-coordinator. Hand-written
// rather than generated — kept narrow to what the UI consumes.
//
// Shapes traced from:
//   crates/engram-core/src/types/session.rs       (Session, SessionStatus)
//   crates/engram-coordinator/src/api/hosts.rs    (HostView)
//   crates/engram-coordinator/src/state.rs        (SessionEvent enum)
//   crates/engram-harness-proto/src/lib.rs        (AgentRole)

export type SessionStatus =
  | 'pending'
  | 'active'
  | 'idle'
  | 'cold_evicted'
  | 'completed'
  | 'failed'
  | 'dead';

// Stage B1 wire shape: a session's image is now a flat OCI URI
// (`<host>[:port]/<repo>:<tag>`). The earlier discriminated
// `{ kind, repo, tag }` shape is gone — the backend resolves the
// URI against `enabled_images` at session-create time.
export type ImageRef = string;

export type HarnessSpec =
  | { kind: 'none' }
  | { kind: 'builtin'; name: string };

export interface Session {
  id: string;
  user_id: string | null;
  status: SessionStatus;
  host_id: string | null;
  sandbox_id: string | null;
  image: ImageRef;
  harness: HarnessSpec;
  created_at: string;
  last_active_at: string;
}

export interface ListSessionsResponse {
  sessions: Session[];
}

export type HostStatus = 'ready' | 'draining' | 'dead';

export interface HostView {
  id: string;
  hostname: string;
  status: HostStatus;
  capacity_total_mib: number;
  capacity_used_mib: number;
  running_sandboxes: number;
  local_snapshots: number;
  last_heartbeat_at: string;
}

export interface ListHostsResponse {
  hosts: HostView[];
}

// ---- Image registry (for the create-session form) ---------------------

/** Harness available on this deployment — read from `/api/harnesses`,
 * a Postgres-backed list of registry-pulled packs. */
export interface HarnessDescriptor {
  name: string;
  description: string | null;
}

// ---- Session creation -------------------------------------------------

export interface CreateSessionResponse {
  session_id: string;
  status: string;
  image_version: string;
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

// ---- Settings · Registries ---------------------------------------------
//
// Polymorphic on `auth_kind`. `Static` carries a username (the
// password is sealed coordinator-side; ciphertext never leaves the
// server). `GcpWorkloadIdentity` stores no secret material — runtime
// IAM identity is the credential. Future siblings (AwsInstanceRole,
// GcpImpersonateSa, ...) slot in here as new variants without
// reshaping anything.
export type RegistryAuthKind =
  | 'static'
  | 'gcp_workload_identity'
  | 'anonymous';

/** Variant-discriminated request body for `POST /api/registries`. The
 * server `serde(tag = "kind")` decoder matches on these. */
export type AddRegistryAuth =
  | { kind: 'static'; username: string; password: string }
  | { kind: 'gcp_workload_identity'; impersonate_sa?: string | null }
  | { kind: 'anonymous' };

export interface AddRegistryRequest {
  host: string;
  auth: AddRegistryAuth;
}

export interface AddRegistryResponse {
  id: string;
  host: string;
  auth_kind: RegistryAuthKind;
  /** For `static`: the username. For `gcp_workload_identity`: the
   * impersonated SA email if any, else null. Always non-secret. */
  auth_principal: string | null;
}

/** Wire shape of `GET /api/registries` rows. Never carries secret
 * material — the coordinator's `RegistryCredentialSummary` redacts
 * the cipher payload before it reaches the wire. */
export interface RegistryCredentialSummary {
  id: string;
  registry_host: string;
  auth_kind: RegistryAuthKind;
  auth_principal: string | null;
  created_at: string;
  updated_at: string | null;
}

export interface ListRegistriesResponse {
  registries: RegistryCredentialSummary[];
}

// ---- Settings · Harness packs ------------------------------------------

/** Wire shape of `GET /api/harnesses` rows. `registry_uri` non-null
 * means the pack is registered in Postgres (Phase 5+); null means it
 * came from the legacy host-resident scan. */
export interface HarnessPackSummary {
  name: string;
  description: string | null;
  registry_uri: string | null;
}

export interface AddHarnessPackRequest {
  name: string;
  registry_uri: string;
  description?: string | null;
}

// ---- Settings · Enabled images -----------------------------------------
//
// Stage C: operators curate the set of OCI image URIs sessions may
// reference. The coordinator caches each enabled URI's manifest.toml
// at enable time so session-create has zero registry I/O on the hot
// path. The dashboard's image picker reads this list (not the legacy
// filesystem-walking /api/images endpoint).

/** Wire shape of one row from `GET /api/enabled-images`. The raw
 * manifest.toml is intentionally omitted — clients render via the
 * lifted `manifest_name` / `manifest_description` fields. */
export interface EnabledImageSummary {
  id: string;
  image_uri: string;
  manifest_digest: string;
  manifest_name: string | null;
  manifest_description: string | null;
  last_refreshed_at: string;
  created_at: string;
}

export interface ListEnabledImagesResponse {
  images: EnabledImageSummary[];
}
