use axum::middleware;
use axum::routing::{delete, get, post};
use axum::Router;

use crate::state::SharedState;

mod admin;
pub mod auth;
mod enabled_images;
mod events;
mod exec;
pub(crate) mod forge;
// ADR 0021 P1.5a retired `mod harnesses;` — the harness_packs
// registry doesn't exist anymore (the harness is an image property
// baked at image-bake time).
mod health;
mod host_http;
mod hosts;
mod prompt;
mod registries;
pub(crate) mod session_auth;
mod sessions;
mod sessions_inspect;
mod shell;
pub mod snapshot;
pub(crate) mod upload;

pub fn router(state: SharedState) -> Router {
    // The protected sub-router gets the bearer-token layer.
    // `/healthz` (liveness) and `/readyz` (readiness — pings
    // Postgres) are grafted on outside the layer so k8s and GCP LB
    // probes don't have to be told a token.
    let auth_state = auth::AuthState::new(state.cfg.auth_tokens.clone());
    let protected = Router::new()
        .route(
            "/sessions",
            get(sessions::list_sessions).post(sessions::create_session),
        )
        .route(
            "/sessions/:id",
            get(sessions::get_session).delete(sessions::delete_session),
        )
        .route("/sessions/:id/exec", post(exec::exec))
        .route("/sessions/:id/exec/stream", post(exec::exec_stream))
        .route("/sessions/:id/events", get(events::events))
        // ADR 0026: serve a shared artifact to the dashboard. In the
        // protected group so it inherits IAP/bearer gating (the browser
        // hits it via the IAP cookie + nginx-stamped bearer); never
        // world-readable. Hardened headers live in the handler.
        .route(
            "/sessions/:id/artifacts/:artifact_id",
            get(upload::serve_artifact),
        )
        // ADR 0026: trusted operator file pull — capture any file by
        // path from the session (no MIME restriction). Bearer/IAP-authed
        // (in the protected group), distinct from the untrusted in-guest
        // push. Static `from-path` segment takes priority over the
        // `:artifact_id` param above; artifact ids are UUIDs, no clash.
        .route(
            "/sessions/:id/artifacts/from-path",
            post(upload::create_from_path),
        )
        .route("/sessions/:id/snapshot", post(snapshot::snapshot))
        .route("/sessions/:id/resume", post(snapshot::resume))
        .route("/sessions/:id/local", delete(snapshot::evict_local))
        .route("/sessions/:id/prompt", post(prompt::prompt))
        .route("/sessions/:id/shell", get(shell::shell))
        .route("/sessions/:id/log", get(sessions_inspect::log))
        // ADR 0016 Phase A: per-session COW diagnostic.
        .route("/sessions/:id/cow-state", get(sessions_inspect::cow_state))
        // ADR 0013 host → coord HTTP endpoints. The old
        // `/api/hosts/connect` WS handler has been retired —
        // host-agents register over HTTP, heartbeat over HTTP,
        // forward harness events over HTTP, and the coord
        // dispatches back to them over gRPC.
        .route("/api/hosts/register", post(host_http::register))
        .route("/api/hosts/:id/heartbeat", post(host_http::heartbeat))
        // ADR 0023 split-mode forge forwarding: FC hosts proxy each
        // in-guest forge request here (the forge sink can't run on the
        // remote host). Host-authed; the broker token rides in the body.
        .route("/api/hosts/forge", post(forge::forge_forward))
        // ADR 0026 split-mode artifact forwarding: FC hosts relay each
        // in-guest upload here (the upload sink can't run on the remote
        // host). Host-authed; the broker token rides in the base64'd
        // `X-Engram-Upload` header and is validated by `process_upload`.
        .route("/api/hosts/upload", post(upload::upload_forward))
        .route(
            "/api/hosts/:id/auth/resolve-registry",
            post(host_http::resolve_registry_auth),
        )
        .route(
            "/api/hosts/:id/idle-eviction-candidates",
            post(host_http::idle_eviction_candidates),
        )
        // ADR 0016 Phase B: host's FlushScheduler publishes
        // freshly-flushed disk manifests so cow-state +
        // effective_resume_disk_manifest can see the latest disk
        // lineage between snapshot boundaries.
        .route(
            "/api/hosts/:id/live-manifest",
            post(host_http::live_manifest_publish),
        )
        .route(
            "/sessions/:id/harness-events",
            post(host_http::harness_event_ingest),
        )
        .route("/api/hosts", get(hosts::list))
        .route("/api/hosts/:id", get(hosts::get))
        .route("/api/hosts/:id/drain", post(hosts::drain))
        // ADR 0016 Phase A: per-host COW diagnostic.
        .route("/api/hosts/:id/cow-state", get(hosts::cow_state))
        // ADR 0021 P1.5a retired `/api/harnesses` — see migration
        // 0040 + the deleted `mod harnesses` above.
        .route(
            "/api/registries",
            get(registries::list_registries).post(registries::add_registry),
        )
        .route("/api/registries/:host", delete(registries::delete_registry))
        .route(
            "/api/enabled-images",
            get(enabled_images::list_enabled_images).post(enabled_images::enable_image),
        )
        .route(
            "/api/enabled-images/refresh",
            post(enabled_images::refresh_enabled_image),
        )
        .route(
            "/api/enabled-images/disable",
            post(enabled_images::disable_enabled_image),
        )
        .route(
            "/api/admin/reap-materialize-dir",
            post(admin::reap_materialize_dir),
        )
        // ADR 0016 Phase B commit 4a: explicit admin trigger for the
        // FlushScheduler primitive. E2E tests + ops use this to drive
        // an immediate flush + publish round-trip without sleeping a
        // 30s scheduler tick.
        .route("/api/admin/sessions/:id/flush-now", post(admin::flush_now))
        // ADR 0018 Phase C: explicit operator + test trigger for the
        // alive-source evacuation primitive. Auto-triggers
        // (dead_host.rs, nbd_loss_trigger) fire the same shape on
        // host-loss / NBD-loss; this endpoint exposes the operator
        // drain path. Async shape (commit 12): returns 202 once the
        // session is marked `Evacuating`; the `evac_resumer` scanner
        // completes the resume on a peer.
        .route(
            "/api/admin/sessions/:id/evacuate",
            post(admin::evacuate_session),
        )
        // ADR 0018 commit 12e: host-level operator drain. Cordon
        // flips `HostState.draining` + PG `hosts.status = draining`
        // so the picker excludes the host. Drain extends cordon by
        // firing Evacuating for every Active session on the host.
        .route("/api/admin/hosts/:id/cordon", post(admin::cordon_host))
        .route("/api/admin/hosts/:id/uncordon", post(admin::uncordon_host))
        .route("/api/admin/hosts/:id/drain", post(admin::drain_host))
        // ADR 0016 Phase C commit 5: explicit admin triggers for the
        // chunk-GC sweep + candidate-table inspection. The background
        // loop is the implicit production driver
        // (per [explicit_admin_triggers_for_testability]); these are
        // the test seam + operator-driven counterparts. Both POSTs
        // accept `?grace_secs=N` so the e2e test in commit 6a can
        // knock the 24h grace down to 0 without env juggling.
        .route("/api/admin/chunk-gc/dry-run", post(admin::chunk_gc_dry_run))
        .route("/api/admin/chunk-gc/sweep", post(admin::chunk_gc_sweep))
        .route(
            "/api/admin/chunk-gc/candidates",
            get(admin::chunk_gc_candidates),
        )
        .layer(middleware::from_fn_with_state(
            auth_state,
            auth::require_bearer,
        ));

    Router::new()
        .route("/healthz", get(health::healthz))
        .route("/readyz", get(health::readyz))
        // ADR 0023 in-session forge seam. Authenticated in-handler by
        // the per-session credential-broker token (not the deployment
        // bearer), so these live OUTSIDE the `protected` layer — the
        // in-guest helper holds only its session-scoped token.
        .route("/sessions/:id/git-credential", get(forge::git_credential))
        .route(
            "/sessions/:id/pull-request",
            post(forge::create_pull_request),
        )
        .merge(protected)
        .with_state(state)
}
