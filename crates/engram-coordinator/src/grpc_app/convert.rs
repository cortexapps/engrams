//! Dumb, total field copies between the axum/serde-facing api types
//! (`crate::api::sessions`, `engram_core::types::Session`) and the
//! generated proto types (`engram_protocol::app`). No business logic
//! lives here — every function is a mechanical field-by-field copy.
//!
//! The proto `Session` is the JSON wire shape minus `user_id` (ADR 0051
//! §2.1: attribution leaves the contract). Timestamps cross as ISO-8601
//! strings, exactly as the JSON wire serializes them; the string-literal
//! status/mode unions stay strings via the core types' `as_str()`.
//!
//! **Totality is enforced in BOTH directions.** Every converter that reads
//! from a source struct must exhaustively destructure it so that a field
//! added later breaks the build here instead of being silently dropped on
//! the floor. Deliberately-unused fields are bound as `field: _` with a
//! comment explaining the intentional drop.

use engram_protocol::app;

use crate::api::exec::ExecRequest as ApiExecRequest;
use crate::api::sessions::{CreateSessionRequest, ListSessionsResponse, SessionListItem};
use crate::cow_state::CowStateView;
use crate::error::ApiError;
use engram_core::types::sandbox::{WriteFileResult, WriteFileSpec};
use engram_core::types::session::SessionMode;

/// `engram_core::types::Session` → proto `Session`. Drops `user_id`
/// (off-contract per ADR 0051 §2.1: attribution leaves the contract) and
/// `live_disk_manifest` (internal coord state, not on the wire shape).
///
/// The exhaustive destructure below is the totality guard — if a field is
/// added to `engram_core::types::Session` without updating this converter,
/// the build will fail here rather than silently drop the new field.
pub(crate) fn session_to_proto(s: &engram_core::types::Session) -> app::Session {
    let engram_core::types::Session {
        id,
        status,
        host_id,
        sandbox_id,
        image,
        mode,
        created_at,
        last_active_at,
        last_event_at,
        live_disk_manifest: _, // Internal coord state (ADR 0016 Phase B); not on the wire shape.
        park_rung: _,          // Internal parking-ladder state (ADR 0074); not on the wire shape.
        parked_at: _,          // Internal parking-ladder state (ADR 0074); not on the wire shape.
        suggested_title,
    } = s;
    app::Session {
        id: id.to_string(),
        status: status.as_str().to_string(),
        host_id: host_id.map(|h| h.to_string()),
        sandbox_id: sandbox_id.map(|sb| sb.to_string()),
        image: image.clone(),
        mode: mode.as_str().to_string(),
        // ISO-8601, matching the JSON wire (chrono's Serialize is RFC3339).
        created_at: created_at.to_rfc3339(),
        last_active_at: last_active_at.to_rfc3339(),
        last_event_at: last_event_at.map(|t| t.to_rfc3339()),
        suggested_title: suggested_title.clone(),
    }
}

/// api `SessionListItem` → proto `SessionListItem`. `owner_kind` is a
/// TS-mirror-only field — left unset from Rust (the api type has no such
/// field).
///
/// The exhaustive destructure below is the totality guard — if a field is
/// added to `SessionListItem` without updating this converter, the build
/// will fail here rather than silently drop the new field.
pub(crate) fn session_list_item_to_proto(item: SessionListItem) -> app::SessionListItem {
    let SessionListItem {
        session,
        owner_email,
        owner_name,
    } = item;
    app::SessionListItem {
        session: Some(session_to_proto(&session)),
        owner_email,
        owner_name,
        owner_kind: None, // TS-mirror-only field; no Rust equivalent in SessionListItem.
    }
}

/// api `ListSessionsResponse` → proto `ListSessionsResponse`.
///
/// The exhaustive destructure below is the totality guard — if a field is
/// added to `ListSessionsResponse` without updating this converter, the
/// build will fail here.
pub(crate) fn list_sessions_to_proto(resp: ListSessionsResponse) -> app::ListSessionsResponse {
    let ListSessionsResponse { sessions } = resp;
    app::ListSessionsResponse {
        sessions: sessions
            .into_iter()
            .map(session_list_item_to_proto)
            .collect(),
    }
}

/// proto `CreateSessionRequest` → api `CreateSessionRequest`. The
/// `mode` string parses to [`SessionMode`] (empty defaults to `Agent`,
/// matching the axum `#[serde(default)]`); an unknown mode is a
/// `BadRequest`.
///
/// `harness_env` (ADR 0051 Drip A) is handled at the RPC layer (in
/// `grpc_app/session.rs`) before this function is called — the RPC
/// handler folds it into `identity_env`, which `create_session_core`
/// injects AND persists into `session_secrets` for resume. It is NOT
/// bound in the destructure here (the caller has already handled it);
/// to keep the totality guard intact the proto struct is destructured
/// exhaustively, binding `harness_env: _` as the signal that the
/// caller has taken responsibility for it.
///
/// The exhaustive destructure below is the totality guard — if a field is
/// added to `app::CreateSessionRequest` without updating this converter,
/// the build will fail here rather than silently drop the new field.
pub(crate) fn create_request_from_proto(
    r: app::CreateSessionRequest,
) -> Result<CreateSessionRequest, ApiError> {
    // Totality guard: destructure ALL proto fields. `harness_env` is
    // handled at the RPC layer (folded into identity_env); bind it as `_`
    // here to acknowledge the drop.
    let app::CreateSessionRequest {
        image_uri,
        mode,
        prompt,
        harness_env: _, // Handled at RPC layer (folded into identity_env).
        secrets,
        // ADR 0055: profile-selected skill names; the coordinator resolves
        // these to reserved-slot mounts (name -> staged sha) at prepare time.
        selected_skills,
        // ADR 0056: profile-granted "provider:action[@resource]" capabilities;
        // parsed + validated + bound to the session at create (the broker later
        // clamps requests to them).
        capabilities,
        // ADR 0056 (B′): the orchestrator-compiled per-session integration
        // policy (JSON). Parsed here; the coordinator resolves its inject refs
        // into the egress policy at boot.
        integration_policy_json,
        // ADR 0062: the per-session harness selection (a catalog key).
        harness,
        // ADR 0107: the initial prompt's mode directive (e.g. "plan").
        harness_mode,
        requested_session_id,
        // Phase 1b: the initial prompt's client prompt_id. The create path
        // delivers the initial prompt via send_prompt (which mints one when
        // empty), so threading the client id for the FIRST message is a
        // deferred refinement; acknowledge the drop here.
        prompt_id: _,
        oauth_credential,
    } = r;
    let mode = match mode.as_str() {
        "" | "agent" => SessionMode::Agent,
        "dev_vm" => SessionMode::DevVm,
        other => {
            return Err(ApiError::BadRequest(format!(
                "unknown session mode {other:?}; expected \"agent\" or \"dev_vm\""
            )))
        }
    };
    let secrets = if secrets.is_empty() {
        None
    } else {
        Some(secrets.into_iter().collect())
    };
    // ADR 0056: a malformed integration policy is a create-time 400 (like a
    // malformed capability), not a silent drop.
    let integration_policy = engram_core::types::IntegrationPolicy::parse(&integration_policy_json)
        .map_err(|e| ApiError::BadRequest(format!("invalid integration_policy_json: {e}")))?;
    let oauth_credential = oauth_credential
        .map(|binding| {
            let subject = binding.subject.ok_or_else(|| {
                ApiError::BadRequest("oauth_credential.subject is required".into())
            })?;
            if subject.id.trim().is_empty() || binding.provider.trim().is_empty() {
                return Err(ApiError::BadRequest(
                    "oauth credential subject id and provider must not be empty".into(),
                ));
            }
            let subject_kind = match app::OauthSubjectKind::try_from(subject.kind) {
                Ok(app::OauthSubjectKind::User) => {
                    engram_core::types::oauth::OAuthSubjectKind::User
                }
                Ok(app::OauthSubjectKind::Connector) => {
                    engram_core::types::oauth::OAuthSubjectKind::Connector
                }
                Ok(app::OauthSubjectKind::Mcp) => engram_core::types::oauth::OAuthSubjectKind::Mcp,
                _ => {
                    return Err(ApiError::BadRequest(
                        "oauth credential subject kind is required".into(),
                    ))
                }
            };
            Ok(engram_core::types::oauth::OAuthCredentialKey {
                subject_kind,
                subject_id: subject.id,
                provider: binding.provider,
            })
        })
        .transpose()?;
    let requested_session_id = requested_session_id
        .map(|value| {
            uuid::Uuid::parse_str(&value)
                .map(engram_core::types::SessionId::from)
                .map_err(|_| ApiError::BadRequest("requested_session_id must be a UUID".into()))
        })
        .transpose()?;
    Ok(CreateSessionRequest {
        requested_session_id,
        image: image_uri,
        mode,
        prompt,
        harness_mode,
        secrets,
        selected_skills,
        capabilities,
        integration_policy,
        selected_harness: harness,
        oauth_credential,
    })
}

/// proto `ExecRequest` → api `ExecRequest`.
///
/// `argv` wins when non-empty; `command` wins otherwise — matching the
/// `build_exec` validation in `api/exec.rs` which errors on neither-or-both.
///
/// The exhaustive destructure below is the totality guard — if a field is
/// added to `app::ExecRequest` without updating this converter, the build
/// will fail here rather than silently drop the new field.
pub(crate) fn exec_request_from_proto(r: app::ExecRequest) -> ApiExecRequest {
    let app::ExecRequest {
        session_id: _, // Routing field consumed by the caller before conversion.
        command,
        argv,
        env,
        workdir,
        timeout_secs,
        exec_id,
        stdout_offset,
        stderr_offset,
        wake,
    } = r;
    ApiExecRequest {
        command,
        argv: if argv.is_empty() { None } else { Some(argv) },
        env,
        workdir,
        timeout_secs,
        exec_id,
        stdout_offset,
        stderr_offset,
        wake,
    }
}

/// proto `WriteFilesRequest` → engine file specs.
///
/// The routing `session_id` is consumed by the RPC handler. Exhaustive
/// destructuring of both request levels keeps the converter total.
pub(crate) fn write_files_request_from_proto(r: app::WriteFilesRequest) -> Vec<WriteFileSpec> {
    let app::WriteFilesRequest {
        session_id: _,
        files,
    } = r;
    files
        .into_iter()
        .map(|file| {
            let app::WriteFileSpec {
                path,
                content,
                mode,
            } = file;
            WriteFileSpec {
                path,
                content,
                mode,
            }
        })
        .collect()
}

/// Engine per-file results → proto `WriteFilesResponse`.
pub(crate) fn write_files_response_to_proto(
    results: Vec<WriteFileResult>,
) -> app::WriteFilesResponse {
    app::WriteFilesResponse {
        results: results
            .into_iter()
            .map(|result| {
                let WriteFileResult { path, ok, error } = result;
                app::WriteFileResult { path, ok, error }
            })
            .collect(),
    }
}

/// [`CowStateView`] → proto [`CowStateView`] (session.proto).
///
/// Exhaustive destructure below is the totality guard — a new field on
/// `CowStateView` must be handled here or the build fails.
pub(crate) fn cow_state_to_proto(v: &CowStateView) -> app::CowStateView {
    let CowStateView {
        sandbox_id,
        session_id,
        disk_manifest_id,
        disk_manifest_version,
        dirty_chunks,
        dirty_bytes,
        last_flush_at,
        base_chunks,
        base_chunks_local,
        memory_manifest_id,
        memory_manifest_version,
        last_snapshot_at,
    } = v;
    app::CowStateView {
        sandbox_id: sandbox_id.to_string(),
        session_id: session_id.map(|s| s.to_string()),
        disk_manifest_id: disk_manifest_id.clone(),
        disk_manifest_version: *disk_manifest_version,
        dirty_chunks: *dirty_chunks,
        dirty_bytes: *dirty_bytes,
        last_flush_at: last_flush_at.map(|t| t.to_rfc3339()),
        base_chunks: *base_chunks,
        base_chunks_local: *base_chunks_local,
        memory_manifest_id: memory_manifest_id.clone(),
        memory_manifest_version: *memory_manifest_version,
        last_snapshot_at: last_snapshot_at.map(|t| t.to_rfc3339()),
    }
}

/// api `ConversationEntry` → proto `ConversationEntry`.
///
/// The exhaustive destructure below is the totality guard — if a field is
/// added to `ConversationEntry` without updating this converter, the build
/// will fail here rather than silently drop the new field.
pub(crate) fn conversation_entry_to_proto(
    e: crate::api::sessions_inspect::ConversationEntry,
) -> app::ConversationEntry {
    let crate::api::sessions_inspect::ConversationEntry {
        idx,
        kind,
        at,
        payload,
    } = e;
    app::ConversationEntry {
        idx,
        kind,
        at: at.to_rfc3339(),
        // `payload` is a serde_json::Value; serialize to a JSON string for the
        // proto `payload_json` field, matching the SSE wire shape.
        payload_json: payload.to_string(),
    }
}

/// api `CheckpointSummary` → proto `CheckpointSummary`.
///
/// The exhaustive destructure below is the totality guard — if a field is
/// added to `CheckpointSummary` without updating this converter, the build
/// will fail here rather than silently drop the new field.
pub(crate) fn checkpoint_summary_to_proto(
    s: crate::api::sessions_inspect::CheckpointSummary,
) -> app::CheckpointSummary {
    let crate::api::sessions_inspect::CheckpointSummary {
        snapshot_id,
        created_at,
        size_bytes,
        events_cursor,
        recoverable,
        is_latest,
    } = s;
    app::CheckpointSummary {
        snapshot_id,
        created_at: created_at.to_rfc3339(),
        size_bytes,
        events_cursor,
        recoverable,
        is_latest,
    }
}

/// api `ExecRusage` → proto `ExecRusage`.
///
/// The exhaustive destructure below is the totality guard — if a field is
/// added to `ExecRusage` without updating this converter, the build will
/// fail here rather than silently drop the new field.
pub(crate) fn exec_rusage_to_proto(r: engram_core::types::ExecRusage) -> app::ExecRusage {
    let engram_core::types::ExecRusage {
        wall_ms,
        peak_rss_kb,
        user_cpu_ms,
        sys_cpu_ms,
    } = r;
    app::ExecRusage {
        wall_ms,
        peak_rss_kb,
        user_cpu_ms,
        sys_cpu_ms,
    }
}

/// api `HostView` → proto `HostView`.
///
/// The exhaustive destructure below is the totality guard — if a field is
/// added to `HostView` without updating this converter, the build will fail.
pub(crate) fn host_view_to_proto(v: &crate::api::hosts::HostView) -> app::HostView {
    let crate::api::hosts::HostView {
        id,
        hostname,
        status,
        capacity_total_mib,
        capacity_used_mib,
        running_sandboxes,
        ready_images,
        ready_image_digests,
        util_disk_total_mib,
        util_disk_used_mib,
        util_mem_total_mib,
        util_mem_used_mib,
        util_cpu_pct,
        last_heartbeat_at,
        // ADR 0048 scheduler budget — now exposed on the proto (the
        // follow-up contract change this totality guard flagged). The
        // host-operator's scale-down wave reads them over gRPC ListHosts.
        cordoned,
        allocatable_mib,
        reserved_mib,
        free_mib,
        total_vcpus,
        cpu_budget_vcpus,
        reserved_vcpus,
        free_vcpus,
        // ADR 0068: the capability-vector fleet-view surface.
        failing_capabilities,
        fc_snapshot_version,
        capabilities_schema,
        // Issue #540: the RAM ledger's attribution fields.
        util_base_shm_mib,
        util_parked_pss_mib,
        util_running_pss_mib,
        // ADR 0112: the committed-swap admission term.
        util_committed_swap_mib,
        // ADR 0088: in-flight enable work (the operator's roll/drain gates).
        live_materializes,
        live_capture_jobs,
    } = v;
    app::HostView {
        id: id.to_string(),
        hostname: hostname.clone(),
        status: status.to_string(),
        capacity_total_mib: *capacity_total_mib,
        capacity_used_mib: *capacity_used_mib,
        running_sandboxes: *running_sandboxes,
        ready_images: *ready_images as u64,
        ready_image_digests: ready_image_digests.clone(),
        util_disk_total_mib: *util_disk_total_mib,
        util_disk_used_mib: *util_disk_used_mib,
        util_mem_total_mib: *util_mem_total_mib,
        util_mem_used_mib: *util_mem_used_mib,
        util_committed_swap_mib: *util_committed_swap_mib,
        util_cpu_pct: *util_cpu_pct,
        last_heartbeat_at: last_heartbeat_at.to_rfc3339(),
        cordoned: *cordoned,
        allocatable_mib: *allocatable_mib,
        reserved_mib: *reserved_mib,
        free_mib: *free_mib,
        total_vcpus: *total_vcpus,
        cpu_budget_vcpus: *cpu_budget_vcpus,
        reserved_vcpus: *reserved_vcpus,
        free_vcpus: *free_vcpus,
        failing_capabilities: failing_capabilities.clone(),
        fc_snapshot_version: fc_snapshot_version.clone().unwrap_or_default(),
        capabilities_schema: *capabilities_schema,
        util_base_shm_mib: *util_base_shm_mib,
        util_parked_pss_mib: *util_parked_pss_mib,
        util_running_pss_mib: *util_running_pss_mib,
        live_materializes: *live_materializes,
        live_capture_jobs: *live_capture_jobs,
    }
}

/// api `StorageSummaryResponse` → proto `GetStorageSummaryResponse`.
///
/// The exhaustive destructure below is the totality guard.
pub(crate) fn storage_summary_to_proto(
    r: crate::api::storage::StorageSummaryResponse,
) -> app::GetStorageSummaryResponse {
    let crate::api::storage::StorageSummaryResponse {
        snapshots,
        snapshot_bytes,
        gc_pending,
        tracked_sandboxes,
        dirty_chunks,
        unflushed_bytes,
        avg_locality_pct,
        rows,
    } = r;
    app::GetStorageSummaryResponse {
        snapshots,
        snapshot_bytes,
        gc_pending,
        tracked_sandboxes,
        dirty_chunks,
        unflushed_bytes,
        avg_locality_pct,
        rows: rows.into_iter().map(durability_row_to_proto).collect(),
    }
}

fn durability_row_to_proto(r: crate::api::storage::DurabilityRow) -> app::DurabilityRow {
    let crate::api::storage::DurabilityRow {
        sandbox_id,
        session_id,
        host_id,
        dirty_chunks,
        dirty_bytes,
        base_chunks,
        base_chunks_local,
        last_flush_at,
    } = r;
    app::DurabilityRow {
        sandbox_id: sandbox_id.to_string(),
        session_id: session_id.map(|s| s.to_string()),
        host_id: host_id.to_string(),
        dirty_chunks,
        dirty_bytes,
        base_chunks,
        base_chunks_local,
        last_flush_at: last_flush_at.map(|t| t.to_rfc3339()),
    }
}

/// api `FlushNowResult` → proto `FlushSessionResponse`.
///
/// The exhaustive destructure below is the totality guard.
pub(crate) fn flush_now_result_to_proto(
    r: crate::api::admin::FlushNowResult,
) -> app::FlushSessionResponse {
    let crate::api::admin::FlushNowResult {
        outcome,
        manifest_version,
    } = r;
    app::FlushSessionResponse {
        outcome: match outcome {
            crate::api::admin::FlushNowOutcome::Applied => "applied".to_string(),
            crate::api::admin::FlushNowOutcome::Idle => "idle".to_string(),
            crate::api::admin::FlushNowOutcome::Stale => "stale".to_string(),
        },
        manifest_version,
    }
}

/// `ChunkGcSweepResult` → proto `ChunkGcResponse`.
///
/// The exhaustive destructure below is the totality guard.
pub(crate) fn chunk_gc_result_to_proto(
    r: crate::api::admin::ChunkGcSweepResult,
) -> app::ChunkGcResponse {
    let crate::api::admin::ChunkGcSweepResult {
        listed_chunks,
        malformed_keys,
        pin_set_size,
        candidates_marked,
        restart_count,
        restart_budget_exhausted,
        promoted_deletes,
        promote_delete_errors,
        grace_secs,
    } = r;
    app::ChunkGcResponse {
        listed_chunks: listed_chunks as u64,
        malformed_keys: malformed_keys as u64,
        pin_set_size: pin_set_size as u64,
        candidates_marked: candidates_marked as u64,
        restart_count,
        restart_budget_exhausted,
        promoted_deletes: promoted_deletes as u64,
        promote_delete_errors: promote_delete_errors as u64,
        grace_secs,
    }
}

/// `BundleSweepReport` → proto `BundleGcResponse`.
///
/// The exhaustive destructure below is the totality guard.
pub(crate) fn bundle_gc_result_to_proto(
    r: crate::bundle_gc::BundleSweepReport,
) -> app::BundleGcResponse {
    let crate::bundle_gc::BundleSweepReport {
        listed,
        pin_set_size,
        candidates_marked,
        promoted_deletes,
        promote_delete_errors,
        restart_count,
    } = r;
    app::BundleGcResponse {
        listed: listed as u64,
        pin_set_size: pin_set_size as u64,
        candidates_marked: candidates_marked as u64,
        promoted_deletes: promoted_deletes as u64,
        promote_delete_errors: promote_delete_errors as u64,
        restart_count,
    }
}

/// `SnapshotBlobSweepReport` → proto `SnapshotBlobGcResponse`.
///
/// The exhaustive destructure below is the totality guard.
pub(crate) fn snapshot_blob_gc_result_to_proto(
    r: crate::snapshot_blob_gc::SnapshotBlobSweepReport,
) -> app::SnapshotBlobGcResponse {
    let crate::snapshot_blob_gc::SnapshotBlobSweepReport {
        listed,
        malformed,
        pin_set_size,
        candidates_marked,
        promoted_deletes,
        promote_repinned_skips,
        promote_delete_errors,
        restart_count,
    } = r;
    app::SnapshotBlobGcResponse {
        listed: listed as u64,
        malformed: malformed as u64,
        pin_set_size: pin_set_size as u64,
        candidates_marked: candidates_marked as u64,
        promoted_deletes: promoted_deletes as u64,
        promote_repinned_skips: promote_repinned_skips as u64,
        promote_delete_errors: promote_delete_errors as u64,
        restart_count,
    }
}

/// `EnabledImageSummary` → proto `EnabledImageSummary`.
///
/// The exhaustive destructure below is the totality guard.
pub(crate) fn enabled_image_summary_to_proto(
    s: &engram_core::types::EnabledImageSummary,
) -> app::EnabledImageSummary {
    let engram_core::types::EnabledImageSummary {
        id,
        image_uri,
        manifest_digest,
        config,
        last_refreshed_at,
        created_at,
    } = s;
    app::EnabledImageSummary {
        id: id.to_string(),
        image_uri: image_uri.clone(),
        manifest_digest: manifest_digest.clone(),
        config: Some(image_config_to_proto(config)),
        last_refreshed_at: last_refreshed_at.to_rfc3339(),
        created_at: created_at.to_rfc3339(),
    }
}

/// Core [`engram_core::types::image::ImageConfig`] → proto (ADR 0080).
/// Secret refs are shown by name only — `CaptureEnvEntry` never carries a
/// resolved value in either direction, so nothing to redact.
pub(crate) fn image_config_to_proto(
    c: &engram_core::types::image::ImageConfig,
) -> app::ImageConfig {
    app::ImageConfig {
        name: c.name.clone(),
        description: c.description.clone(),
        env: c.env.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
        workdir: c.workdir.clone(),
        resources: Some(app::ImageResources {
            suggested_memory_mib: c.resources.suggested_memory_mib,
            suggested_vcpus: c.resources.suggested_vcpus,
            suggested_disk_gib: c.resources.suggested_disk_gib,
            suggested_swap_mib: c.resources.suggested_swap_mib,
        }),
        warm: c.warm.as_ref().map(|w| app::ImageWarmConfig {
            command: w.command.clone(),
            timeout_secs: w.timeout_secs,
            workdir: w.workdir.clone(),
            env: w.env.iter().map(capture_env_to_proto).collect(),
            network: w.network.as_ref().map(network_policy_to_proto),
        }),
    }
}

/// Proto `ImageConfig` → core, validating shape (name/vcpus/warm argv are
/// validated by `ImageConfig::validate` at the call site — this only
/// rejects structurally malformed entries). `Err` is `invalid_argument`.
pub(crate) fn image_config_from_proto(
    c: &app::ImageConfig,
) -> Result<engram_core::types::image::ImageConfig, String> {
    let resources = c
        .resources
        .as_ref()
        .map(|r| engram_core::types::image::ResourceHints {
            suggested_memory_mib: r.suggested_memory_mib,
            suggested_vcpus: r.suggested_vcpus,
            suggested_disk_gib: r.suggested_disk_gib,
            suggested_swap_mib: r.suggested_swap_mib,
        });
    let warm = match &c.warm {
        Some(w) => Some(engram_core::types::image::WarmConfig {
            command: w.command.clone(),
            timeout_secs: w.timeout_secs,
            workdir: w.workdir.clone(),
            env: capture_env_from_proto(&w.env)?,
            network: w
                .network
                .as_ref()
                .map(network_policy_from_proto)
                .transpose()?,
        }),
        None => None,
    };
    Ok(engram_core::types::image::ImageConfig {
        name: c.name.clone(),
        description: c.description.clone(),
        env: c.env.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
        workdir: c.workdir.clone(),
        resources: resources.unwrap_or_default(),
        warm,
    })
}

/// Core [`engram_core::types::image::NetworkPolicy`] → the shared
/// `ProfileNetwork` proto shape (the warm-egress editor reuses the
/// profile network editor's wire type).
fn network_policy_to_proto(n: &engram_core::types::image::NetworkPolicy) -> app::ProfileNetwork {
    app::ProfileNetwork {
        default: match n.default {
            engram_core::types::image::NetworkDefault::Allow => "allow".to_string(),
            engram_core::types::image::NetworkDefault::Deny => "deny".to_string(),
        },
        allow_hosts: n.allow_hosts.clone(),
        allow_host_patterns: n.allow_host_patterns.clone(),
    }
}

fn network_policy_from_proto(
    n: &app::ProfileNetwork,
) -> Result<engram_core::types::image::NetworkPolicy, String> {
    let default = match n.default.as_str() {
        "allow" => engram_core::types::image::NetworkDefault::Allow,
        // Empty = proto default = the safe posture.
        "deny" | "" => engram_core::types::image::NetworkDefault::Deny,
        other => {
            return Err(format!(
                "network default must be \"allow\" or \"deny\", got {other:?}"
            ))
        }
    };
    Ok(engram_core::types::image::NetworkPolicy {
        default,
        allow_hosts: n.allow_hosts.clone(),
        allow_host_patterns: n.allow_host_patterns.clone(),
    })
}

/// One core [`engram_core::types::CaptureEnvEntry`] → proto.
pub(crate) fn capture_env_to_proto(
    e: &engram_core::types::CaptureEnvEntry,
) -> app::CaptureEnvEntry {
    use engram_core::types::CaptureEnvValue;
    let value = Some(match &e.value {
        CaptureEnvValue::Literal { value } => app::capture_env_entry::Value::Literal(value.clone()),
        CaptureEnvValue::SecretRef { secret_ref } => {
            app::capture_env_entry::Value::SecretRef(secret_ref.clone())
        }
    });
    app::CaptureEnvEntry {
        name: e.name.clone(),
        value,
    }
}

/// Proto `CaptureEnvEntry`s → core, validating each (non-empty name, a value
/// set). `Err` is an `invalid_argument` at the call site.
pub(crate) fn capture_env_from_proto(
    entries: &[app::CaptureEnvEntry],
) -> Result<Vec<engram_core::types::CaptureEnvEntry>, String> {
    use engram_core::types::{CaptureEnvEntry, CaptureEnvValue};
    entries
        .iter()
        .map(|e| {
            if e.name.trim().is_empty() {
                return Err("capture_env entry has an empty name".to_string());
            }
            let value = match &e.value {
                Some(app::capture_env_entry::Value::Literal(v)) => {
                    CaptureEnvValue::Literal { value: v.clone() }
                }
                Some(app::capture_env_entry::Value::SecretRef(r)) => CaptureEnvValue::SecretRef {
                    secret_ref: r.clone(),
                },
                None => return Err(format!("capture_env entry `{}` has no value set", e.name)),
            };
            Ok(CaptureEnvEntry {
                name: e.name.clone(),
                value,
            })
        })
        .collect()
}

/// `EnableJob` → proto `EnableJob`.
///
/// The exhaustive destructure below is the totality guard.
pub(crate) fn enable_job_to_proto(j: &engram_core::types::EnableJob) -> app::EnableJob {
    let engram_core::types::EnableJob {
        id,
        image_uri,
        manifest_digest,
        state,
        chunks_total,
        chunks_done,
        attempts,
        error,
        // The full image config rides the job internally (ADR 0080:
        // captured under, stamped onto the row at ready); it is not
        // surfaced on the job's API response — the operator sees it on
        // EnabledImageSummary.config.
        image_config: _,
        // Internal scanner hint; the RPC response does not need it today.
        force_recapture: _,
        prestage_hosts,
        capture_phase,
        warm_stage,
        warm_stage_started_at,
        warm_stages,
        materialize_stages,
        materialize_host_id,
        output_tail,
        // ADR 0084 (c): the capture placement reservation moved onto
        // `capture_jobs` (no longer bookkept on this enable-job row).
        created_at,
        updated_at,
    } = j;
    app::EnableJob {
        id: id.to_string(),
        image_uri: image_uri.clone(),
        manifest_digest: manifest_digest.clone(),
        state: state.as_str().to_string(),
        chunks_total: *chunks_total,
        chunks_done: *chunks_done,
        attempts: *attempts,
        error: error.clone(),
        created_at: created_at.to_rfc3339(),
        updated_at: updated_at.to_rfc3339(),
        capture_phase: capture_phase.map(|p| p.as_str().to_string()),
        warm_stage: warm_stage.clone(),
        warm_stage_started_at: warm_stage_started_at.map(|t| t.to_rfc3339()),
        output_tail: output_tail.clone(),
        // ADR 0036 amendment (issue #538): JSON-encoded per-host prestage
        // outcome map. `prestage_hosts` is NOT NULL DEFAULT '{}'::jsonb
        // (migration 0081), so `to_string()` always yields valid JSON.
        prestage_hosts: prestage_hosts.to_string(),
        // ADR 0088 UI follow-up: both stage timelines, JSON-encoded (the
        // Vec<WarmStageRecord> serde shape — same as the JSONB columns).
        // Vec-of-Serialize can't fail to encode; fall back to "[]" rather
        // than poisoning the whole list response.
        warm_stages: serde_json::to_string(warm_stages).unwrap_or_else(|_| "[]".to_string()),
        materialize_stages: serde_json::to_string(materialize_stages)
            .unwrap_or_else(|_| "[]".to_string()),
        materialize_host_id: materialize_host_id.map(|h| h.to_string()),
    }
}

/// `RegistryCredentialSummary` → proto `RegistryCredentialSummary`.
///
/// The exhaustive destructure below is the totality guard.
pub(crate) fn registry_credential_summary_to_proto(
    s: &engram_core::types::RegistryCredentialSummary,
) -> app::RegistryCredentialSummary {
    let engram_core::types::RegistryCredentialSummary {
        id,
        registry_host,
        auth_kind,
        auth_principal,
        created_at,
        updated_at,
    } = s;
    app::RegistryCredentialSummary {
        id: id.to_string(),
        registry_host: registry_host.clone(),
        auth_kind: auth_kind.clone(),
        auth_principal: auth_principal.clone(),
        created_at: created_at.to_rfc3339(),
        updated_at: updated_at.map(|t| t.to_rfc3339()),
    }
}

/// proto `AddRegistryRequest` → api `AddRegistryRequest`.
/// The `oneof auth` is mapped exhaustively to the serde-tagged `AddRegistryAuth` enum.
pub(crate) fn add_registry_request_from_proto(
    r: app::AddRegistryRequest,
) -> Result<crate::api::registries::AddRegistryRequest, crate::error::ApiError> {
    use app::add_registry_request::Auth;
    let app::AddRegistryRequest { host, auth } = r;
    let auth = match auth {
        Some(Auth::Static(s)) => {
            let app::StaticRegistryAuth { username, password } = s;
            crate::api::registries::AddRegistryAuth::Static { username, password }
        }
        Some(Auth::GcpWorkloadIdentity(g)) => {
            let app::GcpWorkloadIdentityRegistryAuth { impersonate_sa } = g;
            crate::api::registries::AddRegistryAuth::GcpWorkloadIdentity { impersonate_sa }
        }
        Some(Auth::Anonymous(_a)) => crate::api::registries::AddRegistryAuth::Anonymous,
        None => {
            return Err(crate::error::ApiError::BadRequest(
                "AddRegistryRequest.auth is required (oneof unset)".into(),
            ))
        }
    };
    Ok(crate::api::registries::AddRegistryRequest { host, auth })
}

/// api `ArtifactMeta` → proto `ArtifactMetadata`.
///
/// The exhaustive destructure below is the totality guard — if a field is
/// added to `ArtifactMeta` without updating this converter, the build will
/// fail here rather than silently drop the new field.
pub(crate) fn artifact_meta_to_proto(m: crate::api::upload::ArtifactMeta) -> app::ArtifactMetadata {
    let crate::api::upload::ArtifactMeta {
        media_type,
        size_bytes,
        file_name,
    } = m;
    app::ArtifactMetadata {
        media_type,
        // size_bytes comes from the DB as i64; DB constraint ensures non-negative.
        // 0 = corrupt row, which is better than a huge wrapping value.
        size_bytes: size_bytes.try_into().unwrap_or(0),
        file_name,
    }
}

#[cfg(test)]
// tests drive a live system; wall clock/OS entropy here is input, not a decision source (ADR 0098 D1)
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;
    use engram_core::types::session::SessionMode;
    use engram_core::types::{Session, SessionState};
    use engram_core::{HostId, SandboxId, SessionId};

    #[test]
    fn capture_env_proto_round_trips() {
        use engram_core::types::{CaptureEnvEntry, CaptureEnvValue};
        let core = vec![
            CaptureEnvEntry {
                name: "FLAG".into(),
                value: CaptureEnvValue::Literal {
                    value: "true".into(),
                },
            },
            CaptureEnvEntry {
                name: "OP_SERVICE_ACCOUNT_TOKEN".into(),
                value: CaptureEnvValue::SecretRef {
                    secret_ref: "gcp-sm://projects/p/secrets/op/versions/latest".into(),
                },
            },
        ];
        let proto: Vec<_> = core.iter().map(capture_env_to_proto).collect();
        let back = capture_env_from_proto(&proto).expect("valid round-trip");
        assert_eq!(back, core);
    }

    #[test]
    fn capture_env_from_proto_rejects_empty_name_and_unset_value() {
        // Empty name → error.
        let bad_name = vec![app::CaptureEnvEntry {
            name: "  ".into(),
            value: Some(app::capture_env_entry::Value::Literal("x".into())),
        }];
        assert!(capture_env_from_proto(&bad_name).is_err());
        // No value set (a proto with neither oneof arm) → error.
        let no_value = vec![app::CaptureEnvEntry {
            name: "NAME".into(),
            value: None,
        }];
        assert!(capture_env_from_proto(&no_value).is_err());
    }

    fn populated_session() -> Session {
        Session {
            id: SessionId::new(),
            status: SessionState::Active,
            host_id: Some(HostId::new()),
            sandbox_id: Some(SandboxId::new()),
            image: "localhost:5001/demo:warm".into(),
            mode: SessionMode::DevVm,
            created_at: chrono::Utc::now(),
            last_active_at: chrono::Utc::now(),
            // Deliberately offset from `last_active_at` so a converter that
            // crossed the two clocks fails this fixture instead of passing on
            // two identical `Utc::now()` values.
            last_event_at: Some(chrono::Utc::now() + chrono::Duration::seconds(61)),
            live_disk_manifest: None,
            park_rung: 0,
            parked_at: None,
            suggested_title: None,
        }
    }

    #[test]
    fn session_round_trips_every_field() {
        let s = populated_session();
        let p = session_to_proto(&s);

        assert_eq!(p.id, s.id.to_string());
        assert_eq!(p.status, "active");
        assert_eq!(p.host_id, Some(s.host_id.unwrap().to_string()));
        assert_eq!(p.sandbox_id, Some(s.sandbox_id.unwrap().to_string()));
        assert_eq!(p.image, s.image);
        assert_eq!(p.mode, "dev_vm");
        assert_eq!(p.created_at, s.created_at.to_rfc3339());
        assert_eq!(p.last_active_at, s.last_active_at.to_rfc3339());
        assert_eq!(p.last_event_at, Some(s.last_event_at.unwrap().to_rfc3339()));

        // The activity clock is nullable on both sides — a session that has
        // not emitted an event must cross as unset, not as an epoch string.
        let never_evented = Session {
            last_event_at: None,
            ..populated_session()
        };
        assert_eq!(session_to_proto(&never_evented).last_event_at, None);
    }

    #[test]
    fn session_optionals_unset_when_none() {
        let mut s = populated_session();
        s.host_id = None;
        s.sandbox_id = None;
        let p = session_to_proto(&s);
        assert_eq!(p.host_id, None);
        assert_eq!(p.sandbox_id, None);
    }

    #[test]
    fn list_item_carries_owner_identity_and_leaves_kind_unset() {
        let item = SessionListItem {
            session: populated_session(),
            owner_email: Some("a@b.com".into()),
            owner_name: Some("Ada".into()),
        };
        let p = session_list_item_to_proto(item);
        assert!(p.session.is_some());
        assert_eq!(p.owner_email.as_deref(), Some("a@b.com"));
        assert_eq!(p.owner_name.as_deref(), Some("Ada"));
        // owner_kind is TS-mirror-only — never set from Rust.
        assert_eq!(p.owner_kind, None);
    }

    #[test]
    fn list_response_maps_all_rows() {
        let resp = ListSessionsResponse {
            sessions: vec![
                SessionListItem {
                    session: populated_session(),
                    owner_email: None,
                    owner_name: None,
                },
                SessionListItem {
                    session: populated_session(),
                    owner_email: Some("x@y.z".into()),
                    owner_name: None,
                },
            ],
        };
        let p = list_sessions_to_proto(resp);
        assert_eq!(p.sessions.len(), 2);
        assert_eq!(p.sessions[1].owner_email.as_deref(), Some("x@y.z"));
    }

    #[test]
    fn conversation_entry_round_trips() {
        let now = chrono::Utc::now();
        let entry = crate::api::sessions_inspect::ConversationEntry {
            idx: 42,
            kind: "status_changed".to_string(),
            at: now,
            payload: serde_json::json!({"status": "active"}),
        };
        let p = conversation_entry_to_proto(entry);
        assert_eq!(p.idx, 42);
        assert_eq!(p.kind, "status_changed");
        assert_eq!(p.at, now.to_rfc3339());
        assert!(
            p.payload_json.contains("active"),
            "payload_json must contain payload data"
        );
    }

    #[test]
    fn checkpoint_summary_round_trips() {
        let now = chrono::Utc::now();
        let s = crate::api::sessions_inspect::CheckpointSummary {
            snapshot_id: "snap-abc".to_string(),
            created_at: now,
            size_bytes: 1024,
            events_cursor: Some(7),
            recoverable: true,
            is_latest: true,
        };
        let p = checkpoint_summary_to_proto(s);
        assert_eq!(p.snapshot_id, "snap-abc".to_string());
        assert_eq!(p.created_at, now.to_rfc3339());
        assert_eq!(p.size_bytes, 1024);
        assert_eq!(p.events_cursor, Some(7));
        assert!(p.recoverable);
        assert!(p.is_latest);
    }

    #[test]
    fn exec_rusage_round_trips() {
        let r = engram_core::types::ExecRusage {
            wall_ms: 150,
            peak_rss_kb: Some(2048),
            user_cpu_ms: Some(100),
            sys_cpu_ms: Some(50),
        };
        let p = exec_rusage_to_proto(r);
        assert_eq!(p.wall_ms, 150);
        assert_eq!(p.peak_rss_kb, Some(2048));
        assert_eq!(p.user_cpu_ms, Some(100));
        assert_eq!(p.sys_cpu_ms, Some(50));
    }

    #[test]
    fn artifact_meta_round_trips() {
        let m = crate::api::upload::ArtifactMeta {
            media_type: "image/png".to_string(),
            size_bytes: 4096,
            file_name: "abc123.png".to_string(),
        };
        let p = artifact_meta_to_proto(m);
        assert_eq!(p.media_type, "image/png");
        assert_eq!(p.size_bytes, 4096u64);
        assert_eq!(p.file_name, "abc123.png");
    }

    /// `exec_request_from_proto` maps all fields and applies the
    /// argv-wins-when-non-empty rule. Population test per Fix 1.
    #[test]
    fn exec_request_from_proto_maps_all_fields() {
        let mut env = std::collections::HashMap::new();
        env.insert("FOO".to_string(), "bar".to_string());

        // argv non-empty: argv wins, command is still passed through.
        let r = app::ExecRequest {
            session_id: "ignored-by-converter".to_string(),
            command: Some("sh -c echo".to_string()),
            argv: vec!["echo".to_string(), "hello".to_string()],
            env: env.clone(),
            workdir: Some("/tmp".to_string()),
            timeout_secs: Some(30),
            exec_id: Some("exec:caller".into()),
            stdout_offset: Some(12),
            stderr_offset: Some(34),
            wake: Some(true),
        };
        let api = exec_request_from_proto(r);
        assert_eq!(
            api.argv,
            Some(vec!["echo".to_string(), "hello".to_string()])
        );
        assert_eq!(api.command, Some("sh -c echo".to_string()));
        assert_eq!(api.env.get("FOO").map(|s| s.as_str()), Some("bar"));
        assert_eq!(api.workdir.as_deref(), Some("/tmp"));
        assert_eq!(api.timeout_secs, Some(30));
        assert_eq!(api.exec_id.as_deref(), Some("exec:caller"));
        assert_eq!(api.stdout_offset, Some(12));
        assert_eq!(api.stderr_offset, Some(34));
        assert_eq!(api.wake, Some(true));

        // argv empty: argv maps to None.
        let r2 = app::ExecRequest {
            session_id: "ignored".to_string(),
            command: Some("ls".to_string()),
            argv: vec![],
            env: std::collections::HashMap::new(),
            workdir: None,
            timeout_secs: None,
            exec_id: None,
            stdout_offset: None,
            stderr_offset: None,
            wake: None,
        };
        let api2 = exec_request_from_proto(r2);
        assert_eq!(api2.argv, None);
        assert_eq!(api2.command, Some("ls".to_string()));
        assert_eq!(api2.workdir, None);
        assert_eq!(api2.timeout_secs, None);
    }
}
