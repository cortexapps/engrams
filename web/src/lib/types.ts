// Canonical UI type definitions (ADR 0051 Task 28: moved from web/src/types.ts).
//
// These are UI-VIEW shapes (mapping TARGETS), not wire mirrors: hooks in
// hooks/ convert generated proto types into these before handing them to
// consumers. Do NOT add raw wire/response types here — the generated
// bindings in src/gen are the only wire contract (ADR 0051 §7).
//
// Shapes traced from:
//   crates/engram-core/src/types/session.rs       (Session, SessionState)
//   crates/engram-coordinator/src/api/hosts.rs    (HostView)
//   crates/engram-coordinator/src/state.rs        (SessionEvent enum)
//   crates/engram-harness-proto/src/lib.rs        (AgentRole)

/**
 * ADR 0015 M2 lifecycle. Matches the Rust `SessionState` enum
 * exactly. Persistence: `pending` is code-level only and won't
 * appear on a row read from `GET /sessions/:id`; the server may
 * still emit it as the `from`/`to` of an early `status_changed`
 * event during create.
 *
 *   pending     — request accepted, scheduler not yet returned
 *   queued      — accepted but no host had capacity; waiting FIFO for
 *                 scale-up (ADR 0048). Resolves to created/active once
 *                 placed, or failed on a long timeout.
 *   created     — sandbox bound; agentd not yet started
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

/** ADR 0053: profile identity snapshot, as surfaced on a session list row.
 * Maps the ProfileSnapshot proto to a plain view object. */
export interface ProfileSnapshotView {
  id: string;
  name: string;
  icon: string;
  archived: boolean;
  imageUri: string;
  /** ADR 0064: the profile's selected skill bundle names. The session's
   * optional capabilities are derived from these (e.g. the BROWSER tab shows
   * iff this includes "browser"). */
  skills: string[];
}

/** ADR 0031: a session list row — the session plus the owner's identity
 * (present only in the admin "all" view; `null` in the "mine" view). */
export interface SessionListItem extends Session {
  owner_email: string | null;
  owner_name: string | null;
  /** `'system'` for warm-pool / automated sessions; `'user'` or absent for
   * human-launched sessions. Drives the OwnerCell badge choice. */
  owner_kind?: "user" | "system" | null;
  /** ADR 0053: resolved profile snapshot for the row's primary session; null
   * for legacy / profile-less sessions. */
  profile?: ProfileSnapshotView | null;
  /** The owning task's id — the rename target (UpdateTask). Distinct from `id`
   * (which is the primary SESSION id used for navigation). `null` for synthetic
   * `unattributed-*` admin rows, which have no backing task and can't be renamed. */
  taskId: string | null;
  /** The effective display title (custom rename ?? harness suggestion ??
   * truncated prompt), or `null` when none is set → fall back to the short id. */
  title: string | null;
  /** True when `title` is the user's sticky custom rename → the UI offers a
   * "reset to auto" affordance. */
  titleIsCustom: boolean;
}

// ---- ADR 0031: identity ------------------------------------------------

export type Role = "admin" | "member";

/** The current principal, from `GET /me`. */
export interface Principal {
  email: string;
  display_name: string | null;
  role: Role;
  is_admin: boolean;
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
  /** Observed utilization from the latest heartbeat — disk/mem in
   *  MiB, cpu as a 0–100 percentage. 0 until the host's first
   *  heartbeat after the migration; rendered as an empty bar. */
  util_disk_total_mib: number;
  util_disk_used_mib: number;
  util_mem_total_mib: number;
  util_mem_used_mib: number;
  util_cpu_pct: number;
  /** Issue #540 (host RAM ledger attribution): measured base-shm tmpfs
   *  residency, and the running/parked split of guest PSS. 0 until the
   *  host's first post-0078 heartbeat. */
  util_base_shm_mib: number;
  util_parked_pss_mib: number;
  util_running_pss_mib: number;
  last_heartbeat_at: string;
  /** ADR 0068: names of the capability-vector fields currently
   *  `Failed` (or `Unknown` once the host has reported a real
   *  vector) — empty on a healthy host. Kills the "no capacity with
   *  free hosts" mystery mode at the fleet view. */
  failing_capabilities: string[];
  /** ADR 0068: this host's `firecracker --snapshot-version`. Empty
   *  string means "off FC / not yet probed" — a real snapshot-version
   *  string is never empty, so this is unambiguous. */
  fc_snapshot_version: string;
  /** ADR 0068: `0` = this host has never reported a capability vector
   *  (pre-0068 row, or mid-roll) — the soft-pass posture. `>= 1` once
   *  it has reported a real vector. */
  capabilities_schema: number;
  /** ADR 0088: in-flight enable work bound to this host — live
   *  materializes (fresh-claimed materializing enable_jobs) and
   *  non-terminal capture_jobs. The operator's roll/drain gates wait on
   *  both reaching zero; nonzero here explains "why is the roll
   *  waiting on this host". */
  live_materializes: number;
  live_capture_jobs: number;
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
  EditHunk,
  ExecRusage,
  FileChange,
  IndexedEvent,
  SessionEvent,
  SessionEventKind,
  UserQuestion,
  UserQuestionOption,
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

/** One capture-time env entry attached to an enabled image, injected
 * into the image's `[warm]` hook at base-snapshot capture (NOT a session
 * secret). ADR 0080 moved these under `config.warm.env`. `kind` flattens
 * the proto `value` oneof: a `literal` is a plain non-secret value (a
 * flag, a host name); a `secret_ref` is a ref string (e.g. `gcp-sm://…`)
 * resolved server-side at capture — the ref itself is not secret and is
 * safe to display. */
export interface CaptureEnvVar {
  name: string;
  kind: "literal" | "secret_ref";
  value: string;
}

/** `config.warm.network` — the capture VM's egress policy while the
 * warm hook runs (ADR 0080; same shape as a profile's network
 * allow-list). Absent (or deny with no hosts) = egress-less capture. */
export interface WarmNetworkView {
  default: "deny" | "allow";
  allow_hosts: string[];
  allow_host_patterns: string[];
}

/** One enabled-image row, flattened from the proto's `config`
 * (the RPC-supplied ImageConfig, ADR 0080) for rendering + the edit
 * form's pre-fill. The edit form re-assembles the FULL ImageConfig from
 * these fields, so every config field must round-trip through here —
 * an absent optional stays `null`, never collapses to a default. */
export interface EnabledImageSummary {
  id: string;
  image_uri: string;
  manifest_digest: string;
  /** `config.name` — the operator-supplied display name. */
  name: string | null;
  description: string | null;
  /** `config.env` — non-secret env applied to every sandbox of this
   * image (merged over Dockerfile ENV, under session env). */
  env: Record<string, string>;
  /** `config.workdir` — default working directory override. */
  workdir: string | null;
  suggested_vcpus: number | null;
  suggested_memory_mib: number | null;
  suggested_disk_gib: number | null;
  /** `config.warm.command` argv; empty when the image has no warm hook. */
  warm_command: string[];
  /** `config.warm.timeout_secs` (proto uint64, safe as a number for any
   * plausible hook timeout). */
  warm_timeout_secs: number | null;
  warm_workdir: string | null;
  warm_network: WarmNetworkView | null;
  last_refreshed_at: string;
  created_at: string;
  /**
   * Capture-time env from `config.warm.env` (refs, never resolved
   * values — ADR 0080). Drives the edit form's pre-fill. Empty when
   * the image has none.
   */
  capture_env: CaptureEnvVar[];
}

/** ADR 0036: state of an async image-enable job. `prestaging` (issue
 * #538, ADR 0036 amendment) is a new non-terminal value between
 * `capturing` and `ready` — the fleet chunk-prestage wait. */
export type EnableJobState =
  | "pending"
  | "materializing"
  | "capturing"
  | "prestaging"
  | "ready"
  | "failed";

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
  /** Issue #539: live/last capture progress. `capture_phase` is
   * "boot" | "warm" | "snapshot" while `state === "capturing"`; unset
   * outside a capture. `warm_stage` is the current (or, on a failed
   * job, last-known) [warm]-hook stage name. `output_tail` is the
   * rolling last 16 KiB of the hook's combined stdout+stderr —
   * populated on success AND failure, so a failed job's diagnosis
   * needs no host-log access. */
  capture_phase: string | null;
  warm_stage: string | null;
  warm_stage_started_at: string | null;
  output_tail: string | null;
  /** ADR 0036 amendment (issue #538): per-host prestage outcome map,
   * JSON-encoded (`{"<host-uuid>": {"outcome":
   * "staged"|"timed_out"|"unschedulable", "waited_ms": <u64>}}`). `"{}"`
   * until the prestage stage runs. */
  prestage_hosts: string;
  /** ADR 0088 UI follow-up: the [warm]-hook stage history — JSON-encoded
   * array of {name, started_at, ended_at, outcome} records ("[]" before
   * a capture runs). */
  warm_stages: string;
  /** ADR 0088 UI follow-up: the materialize-side twin (pull/flatten/
   * pack/chunk records, same shape). An open record (ended_at null) on
   * a non-materializing job marks where a failed attempt died. */
  materialize_stages: string;
  /** ADR 0088: host UUID the (last) materialize ran on. */
  materialize_host_id: string | null;
}

/** One stage record of either enable timeline (`warm_stages` /
 * `materialize_stages`), as persisted server-side. */
export interface StageRecord {
  name: string;
  started_at: string;
  /** null while the stage is open (running, or abandoned by a failure). */
  ended_at: string | null;
  outcome: "running" | "done" | "failed";
}

export interface ListEnableJobsResponse {
  jobs: EnableJob[];
}

export interface ListEnabledImagesResponse {
  images: EnabledImageSummary[];
}
