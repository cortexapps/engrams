use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use engram_core::{BackendError, MetaError, SandboxError};
use serde::Serialize;

#[derive(Debug)]
pub enum ApiError {
    BadRequest(String),
    NotFound(String),
    Conflict(String),
    /// 410 Gone — the resource lived but is now permanently
    /// unavailable. Used for sessions whose FC snapshot was
    /// invalidated (Dead): the only affordance is to fork.
    Gone(String),
    /// 410 Gone — ADR 0015 M3: the bound host is no longer
    /// reachable. Same status as [`Self::Gone`] but a distinct
    /// machine-readable slug (`host_lost`) so clients can tell
    /// "snapshot lost forever" from "host disappeared, you may be
    /// able to resume on another host once M4 ships re-pick".
    HostLost(String),
    Unsupported(String),
    /// 503 — request was rejected because the system is temporarily
    /// unable to satisfy it. Used for capacity-fit failures at
    /// session create: no host has free capacity, no row is written
    /// to Postgres, retry can succeed. Distinguished from `Conflict`
    /// (resource exists in a state that rejects the op) and
    /// `Internal` (genuinely broken).
    Unavailable(String),
    /// 401 — the caller's credential is missing or invalid. Used by the
    /// ADR 0023 in-session forge endpoints, authenticated by the
    /// per-session credential-broker token, and by the ADR 0031 principal
    /// layer when no verifier authenticates the request.
    Unauthorized(String),
    /// 403 — the caller is authenticated but lacks the role for this
    /// operation. ADR 0031: a `member` hitting an admin-only route.
    Forbidden(String),
    /// 413 — the request body exceeded a hard size cap. ADR 0026:
    /// an artifact upload over `MAX_ARTIFACT_BYTES` (or the session's
    /// remaining byte budget).
    PayloadTooLarge(String),
    /// 429 — a per-session rate/quota limit was hit. ADR 0026: an
    /// artifact upload over the session's count/total-bytes quota.
    TooManyRequests(String),
    /// 502 — an upstream (a host-agent over gRPC) returned an
    /// unusable response. ADR 0050 B: an exec stream that ended
    /// without an `Exit` event — the host connection dropped mid-exec,
    /// so the partial stdout is NOT a completed command. Distinct from
    /// `Unavailable` (503, "couldn't reach it, retry") and `Internal`
    /// (the coord itself is broken).
    BadGateway(String),
    Internal(String),
    /// Issue #539: a structured base-snapshot capture failure — carries
    /// the [`engram_core::types::CaptureFailureKind`] so
    /// `classify_capture_error` (`enable_scanner.rs`) can decide
    /// retryable (`WarmExecTransport`) vs. deterministic bail-fast
    /// without string-matching the message. The failing stage + output
    /// tail aren't duplicated here — the last `CaptureProgress` write
    /// already persisted them onto the `enable_jobs` row before this
    /// error surfaced (even on a `WarmExecTransport` mid-stream death).
    CaptureFailed {
        kind: engram_core::types::CaptureFailureKind,
        message: String,
    },
    /// ADR 0080 phase 3b: a structured image-materialize failure —
    /// carries the [`engram_core::types::MaterializeFailureKind`] so
    /// `classify_materialize_error` (`enable_scanner.rs`) can decide
    /// retryable (busy/disk/registry/store/transport) vs. deterministic
    /// bail-fast (too-large/image-content) without string-matching.
    MaterializeFailed {
        kind: engram_core::types::MaterializeFailureKind,
        message: String,
    },
}

#[derive(Serialize)]
struct ErrorBody {
    error: String,
    message: String,
}

impl ApiError {
    pub fn status(&self) -> StatusCode {
        match self {
            Self::BadRequest(_) => StatusCode::BAD_REQUEST,
            Self::NotFound(_) => StatusCode::NOT_FOUND,
            Self::Conflict(_) => StatusCode::CONFLICT,
            Self::Gone(_) | Self::HostLost(_) => StatusCode::GONE,
            Self::Unsupported(_) => StatusCode::NOT_IMPLEMENTED,
            Self::Unavailable(_) => StatusCode::SERVICE_UNAVAILABLE,
            Self::Unauthorized(_) => StatusCode::UNAUTHORIZED,
            Self::Forbidden(_) => StatusCode::FORBIDDEN,
            Self::PayloadTooLarge(_) => StatusCode::PAYLOAD_TOO_LARGE,
            Self::TooManyRequests(_) => StatusCode::TOO_MANY_REQUESTS,
            Self::BadGateway(_) => StatusCode::BAD_GATEWAY,
            Self::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
            Self::CaptureFailed { .. } => StatusCode::INTERNAL_SERVER_ERROR,
            Self::MaterializeFailed { .. } => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    pub(crate) fn slug(&self) -> &'static str {
        match self {
            Self::BadRequest(_) => "bad_request",
            Self::NotFound(_) => "not_found",
            Self::Conflict(_) => "conflict",
            Self::Gone(_) => "snapshot_invalidated",
            Self::HostLost(_) => "host_lost",
            Self::Unsupported(_) => "unsupported",
            Self::Unavailable(_) => "unavailable",
            Self::Unauthorized(_) => "unauthorized",
            Self::Forbidden(_) => "forbidden",
            Self::PayloadTooLarge(_) => "payload_too_large",
            Self::TooManyRequests(_) => "too_many_requests",
            Self::BadGateway(_) => "bad_gateway",
            Self::Internal(_) => "internal",
            Self::CaptureFailed { .. } => "capture_failed",
            Self::MaterializeFailed { .. } => "materialize_failed",
        }
    }

    pub(crate) fn message(&self) -> &str {
        match self {
            Self::CaptureFailed { message, .. } | Self::MaterializeFailed { message, .. } => {
                message
            }
            Self::BadRequest(m)
            | Self::NotFound(m)
            | Self::Conflict(m)
            | Self::Gone(m)
            | Self::HostLost(m)
            | Self::Unsupported(m)
            | Self::Unavailable(m)
            | Self::Unauthorized(m)
            | Self::Forbidden(m)
            | Self::PayloadTooLarge(m)
            | Self::TooManyRequests(m)
            | Self::BadGateway(m)
            | Self::Internal(m) => m,
        }
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.slug(), self.message())
    }
}

impl std::error::Error for ApiError {}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = ErrorBody {
            error: self.slug().into(),
            message: self.message().to_string(),
        };
        (self.status(), Json(body)).into_response()
    }
}

impl From<MetaError> for ApiError {
    fn from(e: MetaError) -> Self {
        match e {
            MetaError::NotFound => Self::NotFound("metadata row not found".into()),
            MetaError::Conflict(msg) => Self::Conflict(msg),
            other => Self::Internal(other.to_string()),
        }
    }
}

impl From<BackendError> for ApiError {
    fn from(e: BackendError) -> Self {
        match e {
            BackendError::NotSupported(op) => Self::Unsupported(op.to_string()),
            other => Self::Internal(other.to_string()),
        }
    }
}

impl From<SandboxError> for ApiError {
    fn from(e: SandboxError) -> Self {
        match e {
            SandboxError::NotFound => Self::NotFound("sandbox not found".into()),
            SandboxError::AlreadyExists => Self::Conflict("sandbox already exists".into()),
            SandboxError::InvalidSpec(msg) => Self::BadRequest(msg),
            SandboxError::HostLost => Self::HostLost(
                "session host is no longer reachable; the sandbox is gone. \
                 Resume from a snapshot or fork the session."
                    .into(),
            ),
            // ADR 0050 C: transient — the host couldn't be reached but
            // isn't gone. Retryable (503), not a 500.
            SandboxError::Unavailable(msg) => Self::Unavailable(format!(
                "host temporarily unavailable: {msg}. Retry shortly."
            )),
            // Issue #229: a wire_version-skewed host (mid rolling deploy)
            // is a TRANSIENT, retryable condition — a 503, NEVER the 400
            // BadRequest a raw bincode decode error would have produced.
            // The scheduler drains the stale host as the roll finishes, so
            // a retry lands on a version-matched host.
            SandboxError::WireSkew { host, coord } => Self::Unavailable(format!(
                "host wire_version {host} != coordinator {coord} (rolling deploy in \
                 progress); retry shortly."
            )),
            other => Self::Internal(other.to_string()),
        }
    }
}

/// Issue #1012: the evict/evacuate pipeline's failures keep their typed
/// classification. Stringifying an [`EvictError`] into `Internal` turned
/// the wire-skew arm above into an opaque, non-retryable 500 exactly when
/// a rolling deploy made "retry shortly" the correct answer.
impl From<crate::idle_evictor::EvictError> for ApiError {
    fn from(e: crate::idle_evictor::EvictError) -> Self {
        use crate::idle_evictor::EvictError;
        match e {
            // Rides the SandboxError mapping (WireSkew/Unavailable → 503,
            // NotFound → 404, …). The variant's own message already names
            // the failing leg ("wire_version skew: …").
            EvictError::Sandbox(se) => se.into(),
            // Io/Meta are genuinely internal; their Display carries the
            // "idle evict io/meta:" context.
            other => Self::Internal(other.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use axum::response::IntoResponse;

    #[tokio::test]
    async fn into_response_emits_json_with_slug_and_message() {
        let err = ApiError::BadRequest("repo is required".into());
        let resp = err.into_response();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        let body = to_bytes(resp.into_body(), 8 * 1024).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["error"], "bad_request");
        assert_eq!(v["message"], "repo is required");
    }

    #[test]
    fn meta_not_found_maps_to_404() {
        let api: ApiError = MetaError::NotFound.into();
        assert_eq!(api.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn meta_conflict_maps_to_409() {
        let api: ApiError = MetaError::Conflict("dup".into()).into();
        assert_eq!(api.status(), StatusCode::CONFLICT);
    }

    #[test]
    fn meta_db_maps_to_500() {
        let inner: engram_core::error::BoxError = Box::new(std::io::Error::other("kaboom"));
        let api: ApiError = MetaError::Db(inner).into();
        assert_eq!(api.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn backend_not_supported_maps_to_501() {
        let api: ApiError = BackendError::NotSupported("provision_host").into();
        assert_eq!(api.status(), StatusCode::NOT_IMPLEMENTED);
        assert!(api.message().contains("provision_host"));
    }

    #[test]
    fn sandbox_invalid_spec_maps_to_400() {
        let api: ApiError = SandboxError::InvalidSpec("missing image".into()).into();
        assert_eq!(api.status(), StatusCode::BAD_REQUEST);
        assert!(api.message().contains("missing image"));
    }

    #[test]
    fn sandbox_already_exists_maps_to_409() {
        let api: ApiError = SandboxError::AlreadyExists.into();
        assert_eq!(api.status(), StatusCode::CONFLICT);
    }

    #[test]
    fn sandbox_host_lost_maps_to_410_with_host_lost_slug() {
        // ADR 0015 M3: HostLost is 410 Gone with a distinct slug from
        // snapshot-invalidation so clients can tell them apart.
        let api: ApiError = SandboxError::HostLost.into();
        assert_eq!(api.status(), StatusCode::GONE);
        assert_eq!(api.slug(), "host_lost");
    }

    #[test]
    fn sandbox_other_variants_default_to_500() {
        for e in [
            SandboxError::Timeout,
            SandboxError::LimitExceeded("cpu".into()),
            SandboxError::Snapshot("fc not running".into()),
        ] {
            let api: ApiError = e.into();
            assert_eq!(api.status(), StatusCode::INTERNAL_SERVER_ERROR);
        }
    }

    #[test]
    fn sandbox_unavailable_maps_to_retryable_503() {
        // ADR 0050 C: a transient host-unreachable must be a retryable
        // 503, NOT a 500 (which the client treats as a hard failure).
        let api: ApiError = SandboxError::Unavailable("tcp connect error".into()).into();
        assert_eq!(api.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(api.slug(), "unavailable");
    }

    #[test]
    fn sandbox_wire_skew_maps_to_retryable_503_never_400() {
        // Issue #229: a wire_version-skewed host (mixed-version fleet mid
        // rolling deploy) must surface as a RETRYABLE 503 — NEVER the 400
        // BadRequest that a raw bincode decode error produced (the bug).
        let api: ApiError = SandboxError::WireSkew { host: 2, coord: 3 }.into();
        assert_eq!(
            api.status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "wire skew must be a retryable 503, not a permanent 400",
        );
        assert_ne!(api.status(), StatusCode::BAD_REQUEST);
        assert_eq!(api.slug(), "unavailable");
        // The version numbers travel in the message so the operator can
        // see the skew without grepping host logs.
        assert!(api.message().contains('2') && api.message().contains('3'));
    }

    #[test]
    fn bad_gateway_maps_to_502() {
        // ADR 0050 B: a truncated exec stream is a 502 (the upstream host
        // gave an unusable response), distinct from 503 (couldn't reach it).
        let api = ApiError::BadGateway("exec stream truncated".into());
        assert_eq!(api.status(), StatusCode::BAD_GATEWAY);
        assert_eq!(api.slug(), "bad_gateway");
    }

    #[test]
    fn evict_error_keeps_the_sandbox_classification() {
        use crate::idle_evictor::EvictError;
        // Issue #1012: `engrams host evacuate` against a wire-skewed source
        // host returned an opaque 500 ("evac pipeline: …") mid-deploy. The
        // typed path must ride the SandboxError arms: skew → retryable 503.
        let api: ApiError = EvictError::Sandbox(SandboxError::WireSkew {
            host: 23,
            coord: 24,
        })
        .into();
        assert_eq!(api.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(api.slug(), "unavailable");
        assert!(api.message().contains("23") && api.message().contains("24"));

        // Unavailable hosts stay retryable through the same path…
        let api: ApiError =
            EvictError::Sandbox(SandboxError::Unavailable("dial timeout".into())).into();
        assert_eq!(api.status(), StatusCode::SERVICE_UNAVAILABLE);

        // …while Io/Meta remain genuinely internal.
        let api: ApiError = EvictError::Meta("row vanished".into()).into();
        assert_eq!(api.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert!(api.message().contains("idle evict meta"));
    }
}
