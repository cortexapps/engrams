//! Orchestrator-facing app gRPC surface (ADR 0039 §2.3). Lives beside
//! the axum API during the migration; the axum web routes retire in
//! Phase 5.
//!
//! Task 8 scaffold: every RPC answers `Code::Unimplemented`. Tasks
//! 10-13 replace the stubs service-by-service with extract-and-delegate
//! implementations over the same `AppState` the axum handlers use.
//! The caller is a single trusted service (the orchestrator),
//! authenticated per-RPC by a static bearer ([`auth::BearerAuth`],
//! ADR 0039 §5 — fail closed when no tokens are configured); per-user
//! authz lives over there, not here.

pub mod auth;
mod convert;
mod fleet;
mod image;
mod integration_op;
mod mint;
mod mount_catalog;
mod org_secret;
mod session;
mod shell_relay;

use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use engram_protocol::app;
use tonic::Status;

pub use fleet::AppFleetService;
pub use image::AppImageService;
pub use integration_op::AppIntegrationOpService;
pub use mint::AppMintService;
pub use mount_catalog::AppMountCatalogService;
pub use org_secret::AppOrgSecretService;
pub use session::AppSessionService;
pub use shell_relay::AppShellRelayService;

use crate::error::ApiError;
use crate::state::SharedState;

/// Boxed response stream for the server-streaming RPCs. The concrete
/// streams arrive with the real implementations (Tasks 11-12); the
/// stubs only need the associated types to satisfy the traits.
pub(crate) type BoxStream<T> = Pin<Box<dyn tokio_stream::Stream<Item = Result<T, Status>> + Send>>;

/// Parse a wire session id (a UUID string) into a [`SessionId`], mapping a
/// malformed id to `INVALID_ARGUMENT` rather than an opaque 500. Shared by
/// every SessionService RPC that takes a `session_id`.
//
// `clippy::result_large_err`: `tonic::Status` is the unavoidable RPC error
// type — the `?` at each call site flows straight into a
// `Result<_, Status>`, so boxing here would just force an unbox. Same
// rationale as `auth::BearerAuth::check`.
#[allow(clippy::result_large_err)]
pub(crate) fn parse_session_id(s: &str) -> Result<engram_core::SessionId, Status> {
    s.parse()
        .map_err(|_| Status::invalid_argument(format!("malformed session_id: {s:?}")))
}

/// Map an [`ApiError`] to a gRPC [`Status`], exhaustively over every
/// variant (so a new `ApiError` arm is a compile error here, not a
/// silent `internal`). The HTTP→gRPC code mapping follows the standard
/// google.rpc.Code correspondence; the [`ApiError::slug`] rides along as
/// `engram-error-slug` Status metadata so the web can distinguish slugs
/// that share an HTTP code (`snapshot_invalidated` vs `host_lost`, both
/// 410) and the orchestrator can relay it.
///
/// Placed in `mod.rs` (not `session.rs`) because Tasks 11-13 import it
/// from every per-service file; it is service-agnostic.
//
// `clippy::result_large_err`: `tonic::Status` is the unavoidable RPC error
// type — the `?` at each call site flows straight into a
// `Result<_, Status>`, so boxing here would just force an unbox. Same
// rationale as `auth::BearerAuth::check`.
#[allow(clippy::result_large_err)]
pub(crate) fn into_status(err: ApiError) -> Status {
    use tonic::Code;
    let code = match err {
        ApiError::NotFound(_) => Code::NotFound,
        ApiError::Forbidden(_) => Code::PermissionDenied,
        ApiError::Unauthorized(_) => Code::Unauthenticated,
        ApiError::BadRequest(_) => Code::InvalidArgument,
        ApiError::Conflict(_) => Code::FailedPrecondition,
        ApiError::Gone(_) | ApiError::HostLost(_) => Code::FailedPrecondition,
        ApiError::Unavailable(_) => Code::Unavailable,
        ApiError::Unsupported(_) => Code::Unimplemented,
        ApiError::PayloadTooLarge(_) | ApiError::TooManyRequests(_) => Code::ResourceExhausted,
        // ADR 0051: BadGateway (a failed upstream hop, e.g. a host RPC) maps to
        // Unavailable — transient/retryable from the caller's view, like the
        // 502 it is on the axum side.
        ApiError::BadGateway(_) => Code::Unavailable,
        ApiError::Internal(_) => Code::Internal,
    };
    let slug = err.slug();
    let mut status = Status::new(code, err.message().to_string());
    // Best-effort: the slug is a static ASCII identifier, so the parse
    // never fails; guard anyway so a future non-ASCII slug can't panic
    // the RPC.
    if let Ok(val) = slug.parse() {
        status.metadata_mut().insert("engram-error-slug", val);
    }
    status
}

/// ADR 0039 §9.4: keepalive PINGs so a dead orchestrator's streams
/// are detected and torn down (releasing leases/subscriptions)
/// instead of leaking until TCP gives up.
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(20);
const KEEPALIVE_TIMEOUT: Duration = Duration::from_secs(10);

/// Build the tonic server for the app surface: all four services on
/// one listener, HTTP/2 keepalives per [`KEEPALIVE_INTERVAL`] /
/// [`KEEPALIVE_TIMEOUT`].
///
/// Returns the un-bound router; the caller binds the listener and
/// picks the serve strategy (`serve_with_incoming_shutdown` over an
/// eagerly bound listener in lib.rs, `serve_with_incoming` on an
/// ephemeral port in tests).
pub fn server(state: SharedState) -> tonic::transport::server::Router {
    // One shared allow-list snapshot for all four services, taken from
    // config at construction time (no hot-reload; rotation = overlap
    // both tokens, restart, drop the old one).
    let auth = Arc::new(auth::BearerAuth::new(state.cfg.app_grpc_tokens.clone()));

    // gRPC server reflection (grpcurl `list` / `describe`, proto-less
    // calls). Schema only — exposes no data and rides the SAME
    // network-private boundary as the rest of app-gRPC (ClusterIP /
    // port-forward only; never internet-facing). Built from the
    // `FileDescriptorSet` that `engram-protocol`'s build.rs emits. The
    // builder is infallible here (the descriptor bytes are baked in), but
    // it returns a `Result`; a failure means the codegen is broken, so
    // panic loudly at startup rather than serve a half-built surface.
    let reflection = tonic_reflection::server::Builder::configure()
        .register_encoded_file_descriptor_set(app::FILE_DESCRIPTOR_SET)
        .build_v1()
        .expect("app-gRPC reflection descriptor is malformed (engram-protocol build.rs)");

    tonic::transport::Server::builder()
        .http2_keepalive_interval(Some(KEEPALIVE_INTERVAL))
        .http2_keepalive_timeout(Some(KEEPALIVE_TIMEOUT))
        .add_service(reflection)
        .add_service(app::session_service_server::SessionServiceServer::new(
            AppSessionService {
                state: state.clone(),
                auth: auth.clone(),
            },
        ))
        .add_service(
            app::shell_relay_service_server::ShellRelayServiceServer::new(
                shell_relay::AppShellRelayService {
                    state: state.clone(),
                    auth: auth.clone(),
                },
            ),
        )
        .add_service(app::fleet_service_server::FleetServiceServer::new(
            AppFleetService {
                state: state.clone(),
                auth: auth.clone(),
            },
        ))
        .add_service(app::image_service_server::ImageServiceServer::new(
            AppImageService {
                state: state.clone(),
                auth: auth.clone(),
            },
        ))
        // ADR 0055 P2: the org-shared user-uploaded skill catalog.
        .add_service(
            app::mount_catalog_service_server::MountCatalogServiceServer::new(
                AppMountCatalogService {
                    state: state.clone(),
                    auth: auth.clone(),
                },
            ),
        )
        // ADR 0057 C3: the read-only mint-kind registry (Plane-A form metadata).
        .add_service(app::mint_service_server::MintServiceServer::new(
            AppMintService {
                state: state.clone(),
                auth: auth.clone(),
            },
        ))
        // Server-side, sessionless integration invocation (the "IntegrationOp" seam):
        // RunIntegrationOp (coordinator executes) + ResolveIntegrationCredential (Mode B).
        .add_service(
            app::integration_op_service_server::IntegrationOpServiceServer::new(
                AppIntegrationOpService {
                    state: state.clone(),
                    auth: auth.clone(),
                },
            ),
        )
        // ADR 0057: the admin-managed, KEK-sealed org secret store.
        .add_service(app::org_secret_service_server::OrgSecretServiceServer::new(
            AppOrgSecretService { state, auth },
        ))
}

#[cfg(test)]
mod reflection_tests {
    //! The reflection service is built from the `FileDescriptorSet` that
    //! `engram-protocol`'s build.rs emits. If that path breaks (empty
    //! descriptor, codegen drift), `server()` would panic at startup — so
    //! assert here, at test time, that the descriptor is non-empty and
    //! that `tonic_reflection::server::Builder::build_v1()` accepts it.

    #[test]
    fn reflection_descriptor_is_present_and_buildable() {
        assert!(
            !engram_protocol::app::FILE_DESCRIPTOR_SET.is_empty(),
            "app FILE_DESCRIPTOR_SET is empty — build.rs `file_descriptor_set_path` \
             didn't emit the descriptor"
        );
        tonic_reflection::server::Builder::configure()
            .register_encoded_file_descriptor_set(engram_protocol::app::FILE_DESCRIPTOR_SET)
            .build_v1()
            .expect("reflection builder must accept the emitted app descriptor set");
    }

    /// The descriptor must actually carry the app services — a guard
    /// against compiling a descriptor for the wrong/empty proto set. We
    /// decode it and look for the service names grpcurl would `list`.
    #[test]
    fn reflection_descriptor_lists_the_app_services() {
        use prost::Message;
        let fds = prost_types::FileDescriptorSet::decode(engram_protocol::app::FILE_DESCRIPTOR_SET)
            .expect("descriptor bytes decode as a FileDescriptorSet");
        let service_names: Vec<String> = fds
            .file
            .iter()
            .flat_map(|f| {
                let pkg = f.package.clone().unwrap_or_default();
                f.service
                    .iter()
                    .map(move |s| format!("{}.{}", pkg, s.name.clone().unwrap_or_default()))
            })
            .collect();
        for expected in [
            "engram.app.v1.SessionService",
            "engram.app.v1.FleetService",
            "engram.app.v1.ImageService",
        ] {
            assert!(
                service_names.iter().any(|n| n == expected),
                "reflection descriptor is missing {expected}; got {service_names:?}"
            );
        }
    }
}

#[cfg(test)]
mod into_status_tests {
    use super::*;

    #[test]
    fn into_status_maps_codes_and_attaches_slug() {
        use tonic::Code;
        let cases = [
            (ApiError::NotFound("x".into()), Code::NotFound, "not_found"),
            (
                ApiError::Forbidden("x".into()),
                Code::PermissionDenied,
                "forbidden",
            ),
            (
                ApiError::Unauthorized("x".into()),
                Code::Unauthenticated,
                "unauthorized",
            ),
            (
                ApiError::BadRequest("x".into()),
                Code::InvalidArgument,
                "bad_request",
            ),
            (
                ApiError::Conflict("x".into()),
                Code::FailedPrecondition,
                "conflict",
            ),
            (
                ApiError::Gone("x".into()),
                Code::FailedPrecondition,
                "snapshot_invalidated",
            ),
            (
                ApiError::HostLost("x".into()),
                Code::FailedPrecondition,
                "host_lost",
            ),
            (
                ApiError::Unavailable("x".into()),
                Code::Unavailable,
                "unavailable",
            ),
            (
                ApiError::Unsupported("x".into()),
                Code::Unimplemented,
                "unsupported",
            ),
            (
                ApiError::PayloadTooLarge("x".into()),
                Code::ResourceExhausted,
                "payload_too_large",
            ),
            (
                ApiError::TooManyRequests("x".into()),
                Code::ResourceExhausted,
                "too_many_requests",
            ),
            (ApiError::Internal("x".into()), Code::Internal, "internal"),
        ];
        for (err, code, slug) in cases {
            let st = into_status(err);
            assert_eq!(st.code(), code, "code for slug {slug}");
            assert_eq!(
                st.metadata().get("engram-error-slug").map(|v| v.as_bytes()),
                Some(slug.as_bytes()),
                "slug metadata for {slug}",
            );
            assert_eq!(st.message(), "x");
        }
    }

    // `Gone` and `HostLost` share a gRPC code but carry distinct slugs —
    // the whole reason the slug rides in metadata.
    #[test]
    fn gone_and_host_lost_share_code_but_differ_by_slug() {
        let gone = into_status(ApiError::Gone("g".into()));
        let lost = into_status(ApiError::HostLost("l".into()));
        assert_eq!(gone.code(), lost.code());
        assert_ne!(
            gone.metadata().get("engram-error-slug").unwrap().as_bytes(),
            lost.metadata().get("engram-error-slug").unwrap().as_bytes(),
        );
    }

    // Regression for the enable-image incident: a registry pull / OCI
    // auth failure surfaces from `fetch_and_seal_manifest` as
    // `ApiError::BadRequest(<human message>)`. `into_status` MUST keep
    // that message in `Status::message()` (so it survives the
    // orchestrator passthrough to the browser) AND pick a meaningful,
    // non-`Internal` code — a bare `Internal` is what collapses to an
    // opaque "[internal] HTTP 400" in the UI. `BadRequest` →
    // `InvalidArgument` satisfies both.
    #[test]
    fn registry_pull_failure_preserves_message_and_is_not_internal() {
        use tonic::Code;
        let msg = "registry pull for `reg.example/x:tag` failed: OCI distribution \
                   error: response status 401 Unauthorized: Not authorized. Check \
                   that a matching registry credential exists.";
        let st = into_status(ApiError::BadRequest(msg.to_string()));
        assert_eq!(
            st.code(),
            Code::InvalidArgument,
            "registry-pull failures must map to a meaningful code, not Internal"
        );
        assert_ne!(st.code(), Code::Internal);
        // The human-readable message must survive verbatim — this is the
        // text the operator needs to see in the UI.
        assert_eq!(st.message(), msg);
        assert!(st.message().contains("Not authorized"));
    }
}

#[cfg(test)]
mod convention {
    //! Source-scan guard for the fail-closed auth convention (ADR 0039
    //! §5): EVERY app-gRPC RPC body must begin with `self.auth.check(&req)?`
    //! before it does anything else. The service impls hand-roll
    //! that line in each method (no tower layer — see `auth::BearerAuth`),
    //! so nothing structural stops a future RPC from silently skipping it.
    //! This test counts, in the source, one auth check per `async fn` and
    //! fails loudly if they ever diverge.
    //!
    //! A second test enumerates every `.rs` file under `src/grpc_app/` at
    //! runtime and asserts each is either in `SOURCES` (scanned for auth)
    //! or in `NON_RPC_HELPERS` (explicitly allowlisted non-RPC files). The
    //! allowlisted files are additionally checked to contain no `_server::`
    //! token — a service-impl token in a helper file defeats the point of
    //! the allowlist.
    //!
    //! ==========================================================
    //! IMPLEMENTERS, READ THIS — Tasks 10-13 rewrite all 43 stub
    //! bodies and WILL split the services into per-file modules.
    //! When you add a new file under `src/grpc_app/` that holds RPC
    //! `async fn`s, ADD IT to `SOURCES` below (one `include_str!`
    //! line per file). The scan only sees files listed here; a new
    //! service file that isn't listed is invisible to this guard and
    //! its missing auth checks will NOT be caught.
    //! ==========================================================

    /// Every grpc_app source file that contains RPC `async fn`s. Each
    /// entry is `include_str!`'d and scanned. Append new per-service
    /// files here as Tasks 10-13 split them out of `mod.rs`.
    const SOURCES: &[(&str, &str)] = &[
        ("mod.rs", include_str!("mod.rs")),
        ("session.rs", include_str!("session.rs")),
        ("shell_relay.rs", include_str!("shell_relay.rs")),
        ("fleet.rs", include_str!("fleet.rs")),
        ("image.rs", include_str!("image.rs")),
        ("mount_catalog.rs", include_str!("mount_catalog.rs")),
        ("org_secret.rs", include_str!("org_secret.rs")),
        ("mint.rs", include_str!("mint.rs")),
        ("integration_op.rs", include_str!("integration_op.rs")),
    ];

    /// Files under `src/grpc_app/` that are deliberately NOT listed in
    /// `SOURCES` because they contain zero RPC `async fn`s. Every file
    /// in the directory must be in `SOURCES` OR here — the `file_sweep`
    /// test below enforces this. Files here are additionally checked to
    /// contain no `_server::` token (a service-impl token in a helper
    /// file would defeat the allowlist).
    const NON_RPC_HELPERS: &[&str] = &["convert.rs", "auth.rs"];

    /// The auth line that must open every RPC body. The trailing `;` is
    /// load-bearing: it distinguishes a real call site from the prose
    /// banner comment above each impl block (which writes the same
    /// expression followed by ` — see auth.rs …`, no semicolon) so the
    /// banners don't inflate the count.
    const AUTH_CHECK: &str = "self.auth.check(&req)?;";

    /// Marker that begins this test module. We scan only the source
    /// *above* it so the literals in this very file (the `AUTH_CHECK`
    /// const, the failure messages, the `async fn` in any future test
    /// helpers) don't pollute the counts.
    const TEST_MODULE_MARKER: &str = "#[cfg(test)]\nmod convention";

    #[test]
    fn every_rpc_starts_with_an_auth_check() {
        for (name, full_src) in SOURCES {
            // Scan only the non-test portion of each file.
            let src = match full_src.find(TEST_MODULE_MARKER) {
                Some(idx) => &full_src[..idx],
                None => full_src,
            };

            let rpc_count = src.matches("async fn ").count();
            let check_count = src.matches(AUTH_CHECK).count();

            assert_eq!(
                check_count, rpc_count,
                "AUTH CONVENTION VIOLATED in src/grpc_app/{name}: found {rpc_count} \
                 `async fn ` RPC method(s) but {check_count} `{AUTH_CHECK}` call(s). \
                 Every app-gRPC RPC body MUST begin with `{AUTH_CHECK}` before any \
                 other logic (ADR 0039 §5, fail-closed machine auth — see \
                 src/grpc_app/auth.rs). Open src/grpc_app/{name}, find the RPC `async fn` \
                 that is missing the leading `{AUTH_CHECK}`, and add it. If you added a \
                 NEW per-service file, also add it to the `SOURCES` list in this test \
                 (src/grpc_app/mod.rs, mod convention)."
            );
        }
    }

    /// Every `.rs` file under `src/grpc_app/` must be accounted for:
    /// either listed in `SOURCES` (scanned for auth) or in `NON_RPC_HELPERS`
    /// (explicitly allowlisted). Files in `NON_RPC_HELPERS` must not contain
    /// `_server::` — an RPC service impl hiding there would bypass the auth scan.
    ///
    /// `read_dir` is non-recursive (it sees only the flat `src/grpc_app/`
    /// directory). This test asserts that no subdirectories exist so we
    /// never silently miss a nested service file. If you ever add one,
    /// convert the read_dir loop to a recursive walk and update this guard.
    #[test]
    fn file_sweep_every_grpc_app_file_is_accounted_for() {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let grpc_app_dir = std::path::Path::new(manifest_dir).join("src/grpc_app");

        let source_names: std::collections::HashSet<&str> =
            SOURCES.iter().map(|(name, _)| *name).collect();
        let helper_names: std::collections::HashSet<&str> =
            NON_RPC_HELPERS.iter().copied().collect();

        let entries = std::fs::read_dir(&grpc_app_dir)
            .unwrap_or_else(|e| panic!("cannot read src/grpc_app/: {e}"));

        for entry in entries {
            let entry = entry.expect("dir entry");
            let path = entry.path();

            // Guard: read_dir is non-recursive. Assert no subdirectories
            // exist so we never silently miss a nested service file.
            if path.is_dir() {
                panic!(
                    "src/grpc_app/ has a subdirectory {:?} — \
                     the file_sweep test uses non-recursive read_dir and would miss \
                     any service files inside it. Either remove the subdirectory or \
                     convert the loop to a recursive walk.",
                    path.file_name().unwrap_or_default()
                );
            }

            if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            let file_name = path
                .file_name()
                .and_then(|n| n.to_str())
                .expect("utf-8 filename");

            assert!(
                source_names.contains(file_name) || helper_names.contains(file_name),
                "src/grpc_app/{file_name} is not accounted for: add it to SOURCES (if it \
                 contains RPC `async fn`s) or NON_RPC_HELPERS (if it is a pure helper). \
                 Unaccounted files are invisible to the auth-check scan."
            );

            // Allowlisted helpers must contain no service-impl token.
            if helper_names.contains(file_name) {
                let content = std::fs::read_to_string(&path)
                    .unwrap_or_else(|e| panic!("cannot read src/grpc_app/{file_name}: {e}"));
                assert!(
                    !content.contains("_server::"),
                    "src/grpc_app/{file_name} is in NON_RPC_HELPERS but contains `_server::` — \
                     a service impl token. Either move the impl to a file in SOURCES (and add \
                     the auth check) or remove the service impl from the helper."
                );
            }
        }
    }
}
