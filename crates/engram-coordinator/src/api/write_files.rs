//! App-gRPC `SessionService.WriteFiles` core (ADR 0097).
//!
//! File staging is a control-plane setup primitive: it auto-resumes the
//! session and routes to the owning host, but deliberately emits no session
//! bus events.

use engram_core::types::sandbox::{WriteFileResult, WriteFileSpec};
use engram_core::SessionId;

use crate::error::ApiError;
use crate::state::SharedState;

/// Keep the unary app-gRPC request comfortably below tonic's 4 MiB default
/// after protobuf framing and path/mode overhead.
pub(crate) const MAX_WRITE_FILES_CONTENT_BYTES: usize = 3 * 1024 * 1024;

fn validate_write_files(files: &[WriteFileSpec]) -> Result<(), ApiError> {
    if files.is_empty() {
        return Err(ApiError::BadRequest(
            "`files` must contain at least one file".into(),
        ));
    }

    let mut total = 0usize;
    for file in files {
        if file.path.is_empty() {
            return Err(ApiError::BadRequest("file path must not be empty".into()));
        }
        total = total.checked_add(file.content.len()).ok_or_else(|| {
            ApiError::PayloadTooLarge(format!(
                "total file content exceeds {} bytes",
                MAX_WRITE_FILES_CONTENT_BYTES
            ))
        })?;
        if total > MAX_WRITE_FILES_CONTENT_BYTES {
            return Err(ApiError::PayloadTooLarge(format!(
                "total file content exceeds {} bytes",
                MAX_WRITE_FILES_CONTENT_BYTES
            )));
        }
    }
    Ok(())
}

pub(crate) async fn write_files_core(
    state: &SharedState,
    id: SessionId,
    files: Vec<WriteFileSpec>,
) -> Result<Vec<WriteFileResult>, ApiError> {
    validate_write_files(&files)?;
    crate::api::snapshot::ensure_active(state, id).await?;
    let sandbox_id = state.resolve_sandbox(id).await.ok_or_else(|| {
        ApiError::Conflict(
            "session has no live sandbox — create a new session or resume from snapshot".into(),
        )
    })?;
    state
        .services
        .host
        .write_files(sandbox_id, files)
        .await
        .map_err(ApiError::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str, size: usize) -> WriteFileSpec {
        WriteFileSpec {
            path: path.into(),
            content: vec![0; size],
            mode: None,
        }
    }

    #[test]
    fn rejects_empty_file_list() {
        let err = validate_write_files(&[]).expect_err("empty batch must fail");
        assert!(matches!(err, ApiError::BadRequest(_)));
    }

    #[test]
    fn rejects_empty_path() {
        let err = validate_write_files(&[file("", 1)]).expect_err("empty path must fail");
        assert!(matches!(err, ApiError::BadRequest(_)));
    }

    #[test]
    fn rejects_oversize_total_content() {
        let err = validate_write_files(&[file("a", MAX_WRITE_FILES_CONTENT_BYTES), file("b", 1)])
            .expect_err("oversize batch must fail");
        assert!(matches!(err, ApiError::PayloadTooLarge(_)));
    }
}
