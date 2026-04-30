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
    Unsupported(String),
    Internal(String),
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
            Self::Gone(_) => StatusCode::GONE,
            Self::Unsupported(_) => StatusCode::NOT_IMPLEMENTED,
            Self::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    fn slug(&self) -> &'static str {
        match self {
            Self::BadRequest(_) => "bad_request",
            Self::NotFound(_) => "not_found",
            Self::Conflict(_) => "conflict",
            Self::Gone(_) => "snapshot_invalidated",
            Self::Unsupported(_) => "unsupported",
            Self::Internal(_) => "internal",
        }
    }

    fn message(&self) -> &str {
        match self {
            Self::BadRequest(m)
            | Self::NotFound(m)
            | Self::Conflict(m)
            | Self::Gone(m)
            | Self::Unsupported(m)
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
}
