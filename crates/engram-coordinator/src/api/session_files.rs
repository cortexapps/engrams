//! Streaming session-disk file operations (ADR 0113).

use engram_core::types::sandbox::{
    SessionFileMetadata, SessionFileSpec, SessionFileStream, MAX_SESSION_FILE_BYTES,
};
use engram_core::SessionId;

use crate::error::ApiError;
use crate::state::SharedState;

pub(crate) fn validate_guest_file_path(path: &str) -> Result<(), ApiError> {
    if path.len() > 4096 || !path.starts_with('/') || path == "/" || path.contains('\0') {
        return Err(ApiError::BadRequest(
            "path must be a normalized absolute guest file path".into(),
        ));
    }
    if path[1..]
        .split('/')
        .any(|component| component.is_empty() || matches!(component, "." | ".."))
    {
        return Err(ApiError::BadRequest(
            "path must be a normalized absolute guest file path".into(),
        ));
    }
    Ok(())
}

fn validate_spec(spec: &SessionFileSpec) -> Result<(), ApiError> {
    validate_guest_file_path(&spec.path)?;
    if spec.size_bytes > MAX_SESSION_FILE_BYTES {
        return Err(ApiError::PayloadTooLarge(format!(
            "file exceeds {MAX_SESSION_FILE_BYTES} bytes"
        )));
    }
    if spec.sha256.len() != 64
        || !spec
            .sha256
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(ApiError::BadRequest(
            "sha256 must be 64 lower-case hexadecimal characters".into(),
        ));
    }
    if spec.mode.is_some_and(|mode| mode > 0o777) {
        return Err(ApiError::BadRequest(
            "mode must contain only Unix permission bits".into(),
        ));
    }
    Ok(())
}

async fn active_sandbox(
    state: &SharedState,
    session_id: SessionId,
) -> Result<engram_core::SandboxId, ApiError> {
    crate::api::snapshot::ensure_active(state, session_id).await?;
    state
        .resolve_sandbox(session_id)
        .await
        .ok_or_else(|| ApiError::Conflict("session has no live sandbox after resume".into()))
}

pub(crate) async fn write_file_core(
    state: &SharedState,
    session_id: SessionId,
    spec: SessionFileSpec,
    bytes: SessionFileStream,
) -> Result<SessionFileMetadata, ApiError> {
    validate_spec(&spec)?;
    let sandbox_id = active_sandbox(state, session_id).await?;
    state
        .services
        .host
        .write_file(sandbox_id, spec, bytes)
        .await
        .map_err(ApiError::from)
}

pub(crate) async fn read_file_core(
    state: &SharedState,
    session_id: SessionId,
    path: String,
) -> Result<(SessionFileMetadata, SessionFileStream), ApiError> {
    validate_guest_file_path(&path)?;
    let sandbox_id = active_sandbox(state, session_id).await?;
    state
        .services
        .host
        .read_file(sandbox_id, path)
        .await
        .map_err(ApiError::from)
}

pub(crate) async fn copy_files_core(
    state: &SharedState,
    source_session_id: SessionId,
    target_session_id: SessionId,
    paths: Vec<String>,
) -> Result<Vec<SessionFileMetadata>, ApiError> {
    if paths.is_empty() {
        return Err(ApiError::BadRequest("paths must not be empty".into()));
    }
    for path in &paths {
        validate_guest_file_path(path)?;
    }
    // Resume both ends before the first byte moves. In particular, the target
    // must be active before the caller is allowed to send its prompt.
    active_sandbox(state, source_session_id).await?;
    active_sandbox(state, target_session_id).await?;
    let mut copied = Vec::with_capacity(paths.len());
    for path in paths {
        let mut last_error = None;
        for attempt in 0..2 {
            let source_sandbox_id = active_sandbox(state, source_session_id).await?;
            let target_sandbox_id = active_sandbox(state, target_session_id).await?;
            let (metadata, bytes) = match state
                .services
                .host
                .read_file(source_sandbox_id, path.clone())
                .await
            {
                Ok(result) => result,
                Err(error) if attempt == 0 && transfer_may_have_moved(&error) => {
                    last_error = Some(error);
                    continue;
                }
                Err(error) => return Err(error.into()),
            };
            match state
                .services
                .host
                .write_file(
                    target_sandbox_id,
                    SessionFileSpec {
                        path: path.clone(),
                        size_bytes: metadata.size_bytes,
                        sha256: metadata.sha256,
                        mode: None,
                    },
                    bytes,
                )
                .await
            {
                Ok(result) => {
                    copied.push(result);
                    last_error = None;
                    break;
                }
                Err(error) if attempt == 0 && transfer_may_have_moved(&error) => {
                    last_error = Some(error);
                }
                Err(error) => return Err(error.into()),
            }
        }
        if let Some(error) = last_error {
            return Err(error.into());
        }
    }
    Ok(copied)
}

fn transfer_may_have_moved(error: &engram_core::SandboxError) -> bool {
    matches!(
        error,
        engram_core::SandboxError::NotFound
            | engram_core::SandboxError::HostLost
            | engram_core::SandboxError::Unavailable(_)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_normalized_absolute_guest_file_paths() {
        for path in [
            "/tmp/numbers.txt",
            "/workspace/results/map output.json",
            "/tmp/uploads/019fe2ff-0464-75f3-bb20-a8c1844579b9/file.txt",
        ] {
            assert!(validate_guest_file_path(path).is_ok(), "rejected {path}");
        }
    }

    #[test]
    fn rejects_relative_and_non_normalized_guest_file_paths() {
        for path in [
            "/tmp/uploads/019fe2ff-0464-75f3-bb20-a8c1844579b9/../secret",
            "/tmp//numbers.txt",
            "/tmp/./numbers.txt",
            "tmp/numbers.txt",
            "/",
        ] {
            assert!(validate_guest_file_path(path).is_err(), "accepted {path}");
        }
    }
}
