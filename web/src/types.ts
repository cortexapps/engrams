// Mirrors the wire format produced by engram-coordinator. Hand-written
// rather than generated — kept narrow to what the UI consumes.
//
// Shapes traced from:
//   crates/engram-core/src/types/session.rs       (Session, SessionState)
//   crates/engram-coordinator/src/api/hosts.rs    (HostView)
//   crates/engram-coordinator/src/state.rs        (SessionEvent enum)
//   crates/engram-harness-proto/src/lib.rs        (AgentRole)

/**
 * ADR 0015 M2 lifecycle. Matches the Rust `SessionState` enum
 * exactly. Persistence: `pending` and `guest_ready` are code-level
 * only and won't appear on a row read from `GET /sessions/:id`; the
 * server may still emit them as the `from`/`to` of an early
 * `status_changed` event during create.
 *
 *   pending     — request accepted, scheduler not yet returned
 *   created     — sandbox bound; agentd not yet started
 *   guest_ready — agentd reachable; harness not yet running
 *   active      — agentd reachable AND harness running (or
 *                 harness=none and agentd is ready). Only state in
 *                 which /exec, /shell, /prompt proceed.
 *   idle        — snapshotted; /resume rehydrates
 *   host_lost   — heartbeat-loss against the bound host. The
 *                 reconciler resolves this to `idle` (if a
 *                 recoverable snapshot exists) or `dead`.
 *   completed   — terminal (user-deleted)
 *   failed      — terminal (create failed mid-flight)
 *   dead        — terminal (chunked manifests gone or never were)
 */
export type SessionState =
  | 'pending'
  | 'created'
  | 'guest_ready'
  | 'active'
  | 'idle'
  | 'host_lost'
  | 'completed'
  | 'failed'
  | 'dead';

// Stage B1 wire shape: a session's image is now a flat OCI URI
// (`<host>[:port]/<repo>:<tag>`). The earlier discriminated
// `{ kind, repo, tag }` shape is gone — the backend resolves the
// URI against `enabled_images` at session-create time.
export type ImageRef = string;

/**
 * ADR 0021 P1.3 retired the per-session harness *selection*. Which
 * harness an image runs is now baked into the image manifest
 * (`[harness]` block). The session keeps a single mode axis:
 *
 *   `agent`  — default; drive the image's baked harness if any.
 *   `dev_vm` — boot the image as a shell-only dev VM; if the image
 *              has a baked harness, leave it resident-but-undriven.
 *
 * For a harness-less image, both modes look the same (there's no
 * harness to drive); we still send `mode` for wire uniformity.
 */
export type SessionMode = 'agent' | 'dev_vm';

export interface Session {
  id: string;
  user_id: string | null;
  status: SessionState;
  host_id: string | null;
  sandbox_id: string | null;
  image: ImageRef;
  mode: SessionMode;
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
  /** ADR 0014: sum of `available` warm slots across every template
   * this host keeps warm. Live-only, not persisted — reads 0 on a
   * coord replica that hasn't received a heartbeat from this host
   * yet. Surfaced in `VitalSigns` so operators can see the warm
   * pool's depth at a glance. */
  warm_pool_available: number;
  last_heartbeat_at: string;
}

export interface ListHostsResponse {
  hosts: HostView[];
}

// ---- ADR 0016 Phase A: COW state diagnostic ---------------------------
//
// Mirrors `engram_coordinator::cow_state::CowStateView` (Rust). One row
// per chunk-tracked sandbox; rendered by `components/CowState.tsx` in
// the host card and session detail page.

export interface CowStateView {
  sandbox_id: string;
  session_id: string | null;
  /** Current disk manifest version. Ticks on every `flush()`. */
  disk_manifest_id: string;
  disk_manifest_version: number;
  /** Dirty chunks resident in host RAM (not yet flushed to BlobStorage). */
  dirty_chunks: number;
  dirty_bytes: number;
  /** ISO-8601 timestamp of the last successful `flush()`. `null` =
   * never flushed since the backend was constructed. */
  last_flush_at: string | null;
  /** Total chunks the disk manifest references. */
  base_chunks: number;
  /** Of `base_chunks`, how many are resident on this host's local
   * NVMe cache vs. fetched on-demand from BlobStorage. */
  base_chunks_local: number;
  /** Most recent memory manifest captured in a snapshot. `null` if
   * the session has never been snapshotted. */
  memory_manifest_id: string | null;
  memory_manifest_version: number | null;
  /** ISO-8601 timestamp of the last successful snapshot. `null` =
   * never snapshotted. */
  last_snapshot_at: string | null;
}

export interface HostCowStateResponse {
  host_id: string;
  sessions: CowStateView[];
}

export interface SessionCowStateResponse {
  session_id: string;
  /** `null` when the session has no live sandbox (Idle, HostLost,
   * Pending, terminal) — the disk-tier numbers don't exist. The
   * memory-tier fields would still be projectable from PG but
   * we don't surface them here; consumers can read the
   * snapshot-row endpoint instead. */
  state: CowStateView | null;
}

// ADR 0021 P1.5a retired the `HarnessDescriptor` / `/api/harnesses`
// surface — harnesses aren't a deployment-wide registry anymore.

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
      from: SessionState;
      to: SessionState;
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

// ADR 0021 P1.5a retired the `HarnessPackSummary` / `AddHarnessPackRequest`
// surface — the `/api/harnesses` registry doesn't exist anymore.

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
  /**
   * ADR 0021: name of the harness baked into this image (lifted
   * from `manifest.harness.name`), or null for a harness-less image.
   * The session-create form reads this to decide whether to show the
   * Claude OAuth/API-key picker, etc.
   */
  harness_name: string | null;
  last_refreshed_at: string;
  created_at: string;
}

export interface ListEnabledImagesResponse {
  images: EnabledImageSummary[];
}
