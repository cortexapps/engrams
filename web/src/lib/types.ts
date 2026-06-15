// Canonical UI type definitions (ADR 0039 Task 28: moved from web/src/types.ts).
//
// These are UI-VIEW shapes (mapping TARGETS), not wire mirrors: hooks in
// hooks/ convert generated proto types into these before handing them to
// consumers. Do NOT add raw wire/response types here — the generated
// bindings in src/gen are the only wire contract (ADR 0039 §7).
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
 *   queued      — accepted but no host had capacity; waiting FIFO for
 *                 scale-up (ADR 0048). Resolves to created/active once
 *                 placed, or failed on a long timeout.
 *   created     — sandbox bound; agentd not yet started
 *   guest_ready — agentd reachable; harness not yet running
 *   active      — agentd reachable AND harness running (or
 *                 harness=none and agentd is ready). Only state in
 *                 which /exec, /shell, /prompt proceed.
 *   idle        — snapshotted; /resume rehydrates
 *   host_lost   — heartbeat-loss against the bound host. The
 *                 reconciler resolves this to `idle` (if a
 *                 recoverable snapshot exists) or `dead`.
 *   evacuating  — mid-relocation to a peer host (ADR 0018); the
 *                 evac_resumer scanner drives it back to active.
 *   evicting    — idle-eviction in flight (ADR 0034); the eviction
 *                 scanner snapshots + suspends it to `idle` within
 *                 a couple of minutes. /prompt and /resume 409
 *                 (retryable) while here.
 *   completed   — terminal (user-deleted)
 *   failed      — terminal (create failed mid-flight)
 *   dead        — terminal (chunked manifests gone or never were)
 */
export type SessionState =
  | "pending"
  | "queued"
  | "created"
  | "guest_ready"
  | "active"
  | "idle"
  | "host_lost"
  | "evacuating"
  | "evicting"
  | "completed"
  | "failed"
  | "dead";

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
export type SessionMode = "agent" | "dev_vm";

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

/** ADR 0031: a session list row — the session plus the owner's identity
 * (present only in the admin "all" view; `null` in the "mine" view). */
export interface SessionListItem extends Session {
  owner_email: string | null;
  owner_name: string | null;
  /** `'system'` for warm-pool / automated sessions; `'user'` or absent for
   * human-launched sessions. Drives the OwnerCell badge choice. */
  owner_kind?: "user" | "system" | null;
}

// ---- ADR 0031: identity ------------------------------------------------

export type Role = "admin" | "member";

/** The current principal, from `GET /me`. */
export interface Principal {
  email: string;
  display_name: string | null;
  role: Role;
  is_admin: boolean;
  /** Whether a Claude Code OAuth token is saved (drives create-session
   * gating). The token itself is never returned. */
  has_claude_token: boolean;
  /** Whether interactive sign-out is meaningful (OIDC mode only). Behind an
   * edge proxy (IAP) or in dev synthetic-admin there's no app session to
   * revoke, so the UI hides the Sign-out control. */
  can_sign_out: boolean;
  /** ADR 0031: how the role was assigned. `claim` = IdP claim on sign-in;
   * `scim` = SCIM push; `manual` = admin promoted/revoked in the Members UI. */
  role_source?: "manual" | "scim" | "claim";
}

/** A row from `GET /admin/users`. */
export interface AdminUser {
  id: string;
  email: string;
  display_name: string | null;
  role: Role;
  role_source: "manual" | "scim" | "claim";
  active: boolean;
}

export type HostStatus = "ready" | "draining" | "dead";

export interface HostView {
  id: string;
  hostname: string;
  status: HostStatus;
  capacity_total_mib: number;
  capacity_used_mib: number;
  running_sandboxes: number;
  local_snapshots: number;
  /** Observed utilization from the latest heartbeat — disk/mem in
   *  MiB, cpu as a 0–100 percentage. 0 until the host's first
   *  heartbeat after the migration; rendered as an empty bar. */
  util_disk_total_mib: number;
  util_disk_used_mib: number;
  util_mem_total_mib: number;
  util_mem_used_mib: number;
  util_cpu_pct: number;
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

// ---- ADR 0029: Storage surface summary --------------------------------
//
// Mirrors `engram_coordinator::api::storage::StorageSummaryResponse`.
// Fleet-wide COW/chunk rollups + a per-sandbox durability ledger,
// aggregated server-side from the per-host cow-state plus cheap
// Postgres counts (snapshots, gc-candidates). "chunks stored" and
// "dedup ratio" are intentionally absent — they'd need an O(objects)
// blob-store walk, which we don't run on a polled endpoint.

export interface DurabilityRow {
  sandbox_id: string;
  session_id: string | null;
  host_id: string;
  dirty_chunks: number;
  dirty_bytes: number;
  base_chunks: number;
  base_chunks_local: number;
  /** ISO-8601 of the last successful flush; `null` = never flushed. */
  last_flush_at: string | null;
}

export interface StorageSummaryResponse {
  snapshots: number;
  snapshot_bytes: number;
  gc_pending: number;
  tracked_sandboxes: number;
  dirty_chunks: number;
  unflushed_bytes: number;
  avg_locality_pct: number;
  rows: DurabilityRow[];
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

// ---- Session events (SSE) ----------------------------------------------
//
// Canonical types live in ./events.ts (moved Task 26); re-exported here
// so existing consumers need no import changes.
export type {
  AgentRole,
  ExecRusage,
  IndexedEvent,
  SessionEvent,
  SessionEventKind,
} from "../events";

// ADR 0028 A.log: one checkpoint in a session's chain.
export interface CheckpointSummary {
  snapshot_id: string;
  created_at: string;
  size_bytes: number;
  /** The transcript cursor a rung-1 rewind / fork would cut at. */
  events_cursor: number | null;
  /** HEAD-verified durable in object storage (rung-1-eligible). */
  recoverable: boolean;
  /** The newest checkpoint — the always-pinned rung-1 recovery anchor. */
  is_latest: boolean;
}

export interface CheckpointsResponse {
  session_id: string;
  /** Newest first; bounded to the forkable-history retention window. */
  checkpoints: CheckpointSummary[];
}

// ---- Settings · Registries ---------------------------------------------
//
// Polymorphic on `auth_kind`. `Static` carries a username (the
// password is sealed coordinator-side; ciphertext never leaves the
// server). `GcpWorkloadIdentity` stores no secret material — runtime
// IAM identity is the credential. Future siblings (AwsInstanceRole,
// GcpImpersonateSa, ...) slot in here as new variants without
// reshaping anything.
export type RegistryAuthKind = "static" | "gcp_workload_identity" | "anonymous";

/** Variant-discriminated request body for `POST /api/registries`. The
 * server `serde(tag = "kind")` decoder matches on these. */
export type AddRegistryAuth =
  | { kind: "static"; username: string; password: string }
  | { kind: "gcp_workload_identity"; impersonate_sa?: string | null }
  | { kind: "anonymous" };

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

/** ADR 0036: state of an async image-enable job. */
export type EnableJobState = "pending" | "materializing" | "capturing" | "ready" | "failed";

/** ADR 0036: one row of `GET /api/enable-jobs` — an asynchronous
 * image enable in flight (or terminal). `chunks_done/chunks_total`
 * drive the progress bar. */
export interface EnableJob {
  id: string;
  image_uri: string;
  manifest_digest: string | null;
  state: EnableJobState;
  chunks_total: number | null;
  chunks_done: number;
  attempts: number;
  error: string | null;
  created_at: string;
  updated_at: string;
}

export interface ListEnableJobsResponse {
  jobs: EnableJob[];
}

export interface ListEnabledImagesResponse {
  images: EnabledImageSummary[];
}
