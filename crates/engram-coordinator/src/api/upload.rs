//! ADR 0026 artifact upload seam.
//!
//! Streams a shared file into `BlobStorage` under the GC-safe
//! `artifacts/<session>/<id>` prefix, records an `artifacts` row, and
//! emits a [`SessionEvent::FileShared`]. Two trust surfaces share the
//! [`process_upload`] core (the *caller* does auth):
//!
//! - **Untrusted** in-guest push (the `share-file` skill, a potentially
//!   malicious agent): magic-byte image/video allowlist enforced. Reaches
//!   here over vsock ([`handle_vsock_connection`]) or, in split mode, the
//!   host-relayed [`upload_forward`] (`POST /api/hosts/upload`).
//! - **Trusted** operator pull (`POST /sessions/:id/artifacts/from-path`,
//!   Phase 7): any media type, no allowlist.
//!
//! The serve endpoint `GET /sessions/:id/artifacts/:artifact_id` and the
//! ProcessBackend loopback land in later phases.

use axum::body::Body;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::Json;
use base64::Engine as _;
use bytes::{Bytes, BytesMut};
use engram_core::error::BlobError;
use engram_core::traits::storage::ByteStream;
use engram_core::traits::HarnessByteStream;
use engram_core::types::ids::SessionId;
use engram_core::types::sandbox::{ExecEvent, ExecEventStream, ExecRequest};
use engram_harness_proto::{
    read_msg, write_msg, UploadOp, UploadRequest, UploadResponse, MAX_ARTIFACT_BYTES,
};
use futures::StreamExt;

use crate::error::ApiError;
use crate::state::{SessionEvent, SharedState};

/// Magic-byte sniff window. The longest signature we check (WebP / MP4)
/// needs 12 bytes; 16 leaves headroom.
const SNIFF_LEN: usize = 16;
/// Per-session quota: at most this many artifacts.
const MAX_ARTIFACTS_PER_SESSION: i64 = 200;
/// Per-session quota: at most this many total artifact bytes (2 GiB).
const MAX_ARTIFACT_TOTAL_BYTES_PER_SESSION: i64 = 2 * 1024 * 1024 * 1024;
/// Captions are untrusted text; cap length and strip control chars.
const MAX_CAPTION_CHARS: usize = 280;

/// Whether the *upload authorization* layer should constrain the media
/// type. The serve-side hardening is MIME-agnostic regardless.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Trust {
    /// In-guest push from a potentially malicious agent — only
    /// magic-byte-verified image/video is accepted.
    Untrusted,
    /// Operator pull of any file by path — any type accepted.
    Trusted,
}

/// A stored artifact (the success of [`process_upload`]).
pub struct SharedArtifact {
    pub artifact_id: String,
    pub media_type: String,
    pub size_bytes: u64,
}

/// Typed failure of [`process_upload`], so the untrusted transports can
/// render `UploadResponse::Error` while the trusted HTTP `from-path`
/// handler maps to the right status (413 / 429 / 400 / 500).
pub enum UploadError {
    /// Per-session count/byte quota hit (→ 429).
    Quota(String),
    /// Per-file size cap exceeded mid-stream (→ 413).
    TooLarge(String),
    /// Disallowed media type on the untrusted path, or empty body (→ 400).
    BadMedia(String),
    /// Storage / metadata / read failure (→ 500).
    Internal(String),
}

impl UploadError {
    fn message(&self) -> &str {
        match self {
            Self::Quota(m) | Self::TooLarge(m) | Self::BadMedia(m) | Self::Internal(m) => m,
        }
    }
}

impl From<UploadError> for ApiError {
    fn from(e: UploadError) -> Self {
        match e {
            UploadError::Quota(m) => ApiError::TooManyRequests(m),
            UploadError::TooLarge(m) => ApiError::PayloadTooLarge(m),
            UploadError::BadMedia(m) => ApiError::BadRequest(m),
            UploadError::Internal(m) => ApiError::Internal(m),
        }
    }
}

/// Detect a media type from leading magic bytes. Never trusts a
/// guest-supplied MIME. Returns `None` for anything unrecognized
/// (notably SVG/HTML, which are rejected on the untrusted path).
fn detect_media_type(b: &[u8]) -> Option<&'static str> {
    if b.len() >= 8 && b[..8] == [0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n'] {
        return Some("image/png");
    }
    if b.len() >= 3 && b[..3] == [0xff, 0xd8, 0xff] {
        return Some("image/jpeg");
    }
    if b.len() >= 6 && (&b[..6] == b"GIF87a" || &b[..6] == b"GIF89a") {
        return Some("image/gif");
    }
    if b.len() >= 12 && &b[..4] == b"RIFF" && &b[8..12] == b"WEBP" {
        return Some("image/webp");
    }
    if b.len() >= 8 && &b[4..8] == b"ftyp" {
        return Some("video/mp4");
    }
    if b.len() >= 4 && b[..4] == [0x1a, 0x45, 0xdf, 0xa3] {
        return Some("video/webm");
    }
    None
}

fn is_media(media_type: &str) -> bool {
    media_type.starts_with("image/") || media_type.starts_with("video/")
}

/// Strip control characters (incl. newlines, so a caption can't corrupt
/// SSE framing or log lines) and cap the length. `None` / empty → `None`.
fn sanitize_caption(caption: Option<String>) -> Option<String> {
    let s: String = caption?
        .chars()
        .filter(|ch| !ch.is_control())
        .take(MAX_CAPTION_CHARS)
        .collect();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// The shared core. `src` is the raw file body; `ext` is informational
/// only (the persisted media type comes from sniffing). Streams into
/// blob storage with a hard size cap + per-session byte budget, records
/// the row, and emits the event. Auth is the caller's responsibility.
/// Sentinel embedded in the mid-stream cap-exceeded `BlobError` so the
/// caller can map it back to a 413 (vs a generic 500 storage failure).
const SIZE_CAP_SENTINEL: &str = "artifact exceeds size limit";

pub async fn process_upload(
    state: &SharedState,
    session: SessionId,
    mut src: ByteStream,
    _ext: &str,
    caption: Option<String>,
    trust: Trust,
) -> Result<SharedArtifact, UploadError> {
    // Pre-write quota check against existing usage.
    let (count, total) = state
        .services
        .meta
        .artifact_usage(session)
        .await
        .map_err(|e| UploadError::Internal(format!("artifact usage lookup: {e}")))?;
    if count >= MAX_ARTIFACTS_PER_SESSION {
        return Err(UploadError::Quota(format!(
            "per-session artifact count limit reached ({MAX_ARTIFACTS_PER_SESSION})"
        )));
    }
    if total >= MAX_ARTIFACT_TOTAL_BYTES_PER_SESSION {
        return Err(UploadError::Quota(
            "per-session artifact storage limit reached".into(),
        ));
    }

    // Pull a sniff prefix before deciding anything (so an untrusted
    // reject writes nothing).
    let mut prefix = BytesMut::new();
    let mut hit_eof = false;
    while prefix.len() < SNIFF_LEN {
        match src.next().await {
            Some(Ok(chunk)) => prefix.extend_from_slice(&chunk),
            Some(Err(e)) => return Err(UploadError::Internal(format!("read artifact body: {e}"))),
            None => {
                hit_eof = true;
                break;
            }
        }
    }
    if prefix.is_empty() {
        return Err(UploadError::BadMedia("empty artifact body".into()));
    }
    let media_type = match (trust, detect_media_type(&prefix)) {
        (Trust::Untrusted, Some(mt)) if is_media(mt) => mt.to_string(),
        (Trust::Untrusted, _) => {
            return Err(UploadError::BadMedia(
                "unsupported media type: only image (png/jpeg/gif/webp) and video \
                 (mp4/webm) may be shared from the sandbox"
                    .into(),
            ))
        }
        (Trust::Trusted, Some(mt)) => mt.to_string(),
        (Trust::Trusted, None) => "application/octet-stream".to_string(),
    };

    // Server-generated key — the guest never influences the storage path.
    let artifact_id = state.services.entropy.uuid();
    let key = format!("artifacts/{session}/{}", artifact_id.simple());

    // Cap = min(per-file, remaining session budget).
    let remaining_session = (MAX_ARTIFACT_TOTAL_BYTES_PER_SESSION - total).max(0) as u64;
    let cap = MAX_ARTIFACT_BYTES.min(remaining_session);

    let prefix = prefix.freeze();
    let body = async_stream::stream! {
        let mut written: u64 = 0;
        if !prefix.is_empty() {
            written += prefix.len() as u64;
            if written > cap {
                yield Err(BlobError::Protocol(SIZE_CAP_SENTINEL.into()));
                return;
            }
            yield Ok(prefix);
        }
        if !hit_eof {
            while let Some(item) = src.next().await {
                match item {
                    Ok(chunk) => {
                        written += chunk.len() as u64;
                        if written > cap {
                            yield Err(BlobError::Protocol(SIZE_CAP_SENTINEL.into()));
                            return;
                        }
                        yield Ok(chunk);
                    }
                    Err(e) => {
                        yield Err(e);
                        return;
                    }
                }
            }
        }
    };

    let size = match state
        .services
        .blob
        .put_streaming(&key, ByteStream::new(body))
        .await
    {
        Ok(n) => n,
        Err(e) => {
            // Abort: best-effort delete the (possibly partial) object.
            let _ = state.services.blob.delete(&key).await;
            let msg = e.to_string();
            return Err(if msg.contains(SIZE_CAP_SENTINEL) {
                UploadError::TooLarge(format!(
                    "artifact exceeds the size limit ({} MiB) or the session's \
                     remaining storage budget",
                    MAX_ARTIFACT_BYTES / (1024 * 1024)
                ))
            } else {
                UploadError::Internal(format!("store artifact: {e}"))
            });
        }
    };

    if let Err(e) = state
        .services
        .meta
        .insert_artifact(
            artifact_id,
            session,
            &key,
            &media_type,
            size as i64,
            caption.as_deref(),
        )
        .await
    {
        let _ = state.services.blob.delete(&key).await;
        return Err(UploadError::Internal(format!("record artifact: {e}")));
    }

    let id_str = artifact_id.simple().to_string();
    if let Err(e) = state
        .emit(
            session,
            SessionEvent::FileShared {
                artifact_id: id_str.clone(),
                media_type: media_type.clone(),
                size_bytes: size,
                caption,
                at: state.services.clock.now_utc(),
            },
        )
        .await
    {
        tracing::warn!(session = %session, error = %e, "emit FileShared failed (artifact stored)");
    }

    Ok(SharedArtifact {
        artifact_id: id_str,
        media_type,
        size_bytes: size,
    })
}

/// Authorize an untrusted in-guest [`UploadRequest`] (broker token) and
/// run the shared core with its body stream, rendering the wire
/// [`UploadResponse`]. Shared by the vsock and split-mode HTTP transports.
async fn authorized_untrusted_upload(
    state: &SharedState,
    header: UploadRequest,
    body: ByteStream,
) -> UploadResponse {
    if !crate::api::session_auth::authorize_broker_token(
        state,
        header.session_id,
        &header.broker_token,
    )
    .await
    {
        return UploadResponse::Error {
            message: "invalid or missing upload token".into(),
        };
    }
    let UploadOp::ShareFile { ext, caption, .. } = header.op;
    match process_upload(
        state,
        header.session_id,
        body,
        &ext,
        sanitize_caption(caption),
        Trust::Untrusted,
    )
    .await
    {
        Ok(a) => UploadResponse::Shared {
            artifact_id: a.artifact_id,
            media_type: a.media_type,
            size_bytes: a.size_bytes,
        },
        Err(e) => UploadResponse::Error {
            message: e.message().to_string(),
        },
    }
}

/// Adapt a capped `AsyncRead` (the vsock body after the header frame)
/// into a [`ByteStream`], reading at most `max` bytes.
fn reader_to_bytestream<R>(reader: R, max: u64) -> ByteStream
where
    R: tokio::io::AsyncRead + Send + Unpin + 'static,
{
    let s = async_stream::stream! {
        let mut reader = reader;
        let mut remaining = max;
        let mut buf = vec![0u8; 64 * 1024];
        while remaining > 0 {
            let want = (buf.len() as u64).min(remaining) as usize;
            match tokio::io::AsyncReadExt::read(&mut reader, &mut buf[..want]).await {
                Ok(0) => break,
                Ok(n) => {
                    remaining -= n as u64;
                    yield Ok(Bytes::copy_from_slice(&buf[..n]));
                }
                Err(e) => {
                    yield Err(BlobError::Io(e));
                    return;
                }
            }
        }
    };
    ByteStream::new(s)
}

// ---- vsock transport (Firecracker, in-proc) ----------------------------

/// Handle one in-guest upload connection: read the [`UploadRequest`]
/// header, stream the capped raw body into the shared core, write the
/// [`UploadResponse`]. Wired as the FC backend's `UploadSink` in
/// `lib.rs`. Errors (incl. auth) come back as `UploadResponse::Error`.
pub async fn handle_vsock_connection(state: SharedState, stream: HarnessByteStream) {
    let (mut read_half, mut write_half) = tokio::io::split(stream);
    let header: UploadRequest = match read_msg(&mut read_half).await {
        Ok(h) => h,
        Err(e) => {
            tracing::debug!(error = %e, "upload vsock: malformed header");
            return;
        }
    };
    let UploadOp::ShareFile { size_bytes, .. } = &header.op;
    let body = reader_to_bytestream(read_half, *size_bytes);
    let resp = authorized_untrusted_upload(&state, header, body).await;
    if let Err(e) = write_msg(&mut write_half, &resp).await {
        tracing::debug!(error = %e, "upload vsock: response write failed");
    }
}

// ---- host-facing forwarder (Firecracker split mode) --------------------

/// `POST /api/hosts/upload` — ADR 0026 split-mode artifact forwarding.
///
/// The FC host can't run the `UploadSink` locally (it needs the coord's
/// BlobStorage + broker map). So the host-agent reads the in-guest
/// [`UploadRequest`] header off the vsock stream, then streams the raw
/// body here; the header rides a base64'd bincode `X-Engram-Upload`
/// request header. Mounted in the bearer-authed `/api/hosts/*` group;
/// the per-session broker token still rides in the header and is
/// validated by the shared core (same two-factor check as in-proc).
pub async fn upload_forward(
    State(state): State<SharedState>,
    headers: HeaderMap,
    body: Body,
) -> Json<UploadResponse> {
    let header = match decode_upload_header(&headers) {
        Ok(h) => h,
        Err(message) => return Json(UploadResponse::Error { message }),
    };
    let src = ByteStream::new(
        body.into_data_stream()
            .map(|r| r.map_err(|e| BlobError::Protocol(format!("recv artifact body: {e}")))),
    );
    Json(authorized_untrusted_upload(&state, header, src).await)
}

// ---- trusted operator pull (any file by path) --------------------------

fn ext_from_path(path: &str) -> String {
    std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("bin")
        .to_ascii_lowercase()
}

/// Adapt an exec event stream (a `cat` of the target file) into a
/// [`ByteStream`]: stdout becomes body bytes; a non-zero exit becomes a
/// stream error so [`process_upload`] aborts + deletes (e.g. the path was
/// missing/unreadable). Ending without an Exit is transport loss, not clean
/// EOF, and also becomes a stream error so truncated bytes are never stored.
/// Stderr is dropped.
fn exec_stdout_bytestream(events: ExecEventStream) -> ByteStream {
    let s = async_stream::stream! {
        let mut events = events;
        let mut saw_exit = false;
        while let Some(ev) = events.next().await {
            match ev {
                ExecEvent::Stdout(b) => yield Ok(b),
                ExecEvent::Stderr(_) => {}
                ExecEvent::Exit(Some(0)) => {
                    saw_exit = true;
                    break;
                }
                ExecEvent::Exit(_) => {
                    yield Err(BlobError::Protocol(
                        "reading file from guest failed (cat exited non-zero — \
                         missing or unreadable path?)"
                            .into(),
                    ));
                    return;
                }
            }
        }
        if !saw_exit {
            yield Err(BlobError::Protocol(
                "reading file from guest lost transport mid-read before an Exit frame; \
                 refusing a potentially truncated artifact"
                    .into(),
            ));
        }
    };
    ByteStream::new(s)
}

// ----------------------------------------------------------------
// ADR 0051: transport-agnostic artifact cores for the app-gRPC
// SessionService (`GetArtifact` / `CreateArtifactFromPath`).
//
// Artifacts are served back to the dashboard with MIME-agnostic
// hardening (server-detected `Content-Type`, `X-Content-Type-Options:
// nosniff`, `Content-Disposition: inline`, `Content-Security-Policy:
// sandbox`, `no-store`) applied by the caller — that, not the upload
// allowlist, is what makes serving attacker-controlled bytes to
// operators safe.
// ----------------------------------------------------------------

/// Artifact metadata the gRPC `GetArtifact` stream emits in its first
/// frame (the bytes follow as chunk frames). `size_bytes` is the DB i64;
/// the converter clamps it for the proto u64.
pub struct ArtifactMeta {
    pub media_type: String,
    pub size_bytes: i64,
    pub file_name: String,
}

/// gRPC `GetArtifact` core: resolve the (session-scoped) artifact row and
/// open its blob stream. Mirrors `serve_artifact`'s lookup + the detected
/// `Content-Type` / filename, minus the HTTP response framing (the gRPC
/// handler re-chunks the `ByteStream` into proto frames).
pub(crate) async fn get_artifact_core(
    state: &SharedState,
    session: SessionId,
    artifact_id: &str,
) -> Result<(ArtifactMeta, ByteStream), ApiError> {
    let aid = uuid::Uuid::parse_str(artifact_id)
        .map_err(|_| ApiError::BadRequest("invalid artifact id".into()))?;
    // Scoped to the session: a valid-but-mismatched pair 404s.
    let row = state
        .services
        .meta
        .get_artifact(session, aid)
        .await?
        .ok_or_else(|| ApiError::NotFound("artifact not found".into()))?;
    let stream = state
        .services
        .blob
        .get_streaming(&row.blob_key)
        .await
        .map_err(|e| match e {
            BlobError::NotFound => ApiError::NotFound("artifact blob missing".into()),
            other => ApiError::Internal(format!("read artifact blob: {other}")),
        })?;
    let file_name = format!("{}.{}", aid.simple(), ext_for_media(&row.media_type));
    Ok((
        ArtifactMeta {
            media_type: row.media_type,
            size_bytes: row.size_bytes,
            file_name,
        },
        stream,
    ))
}

/// gRPC `CreateArtifactFromPath` core: the trusted operator file pull.
/// Extracted from `create_from_path` — auto-resume, stream `cat` of the
/// guest path through the shared `process_upload` core (any media type).
pub(crate) async fn create_artifact_from_path_core(
    state: &SharedState,
    session: SessionId,
    path: &str,
    caption: Option<String>,
) -> Result<SharedArtifact, ApiError> {
    crate::api::snapshot::ensure_active(state, session).await?;
    let sandbox_id = state.resolve_sandbox(session).await.ok_or_else(|| {
        ApiError::Conflict(
            "session has no live sandbox — create a new session or resume from snapshot".into(),
        )
    })?;
    let exec_req = ExecRequest {
        command: vec!["cat".into(), "--".into(), path.to_string()],
        stdin: None,
        env: std::collections::HashMap::new(),
        workdir: None,
        timeout: Some(std::time::Duration::from_secs(600)),
        exec_id: None,
        stdout_offset: None,
        stderr_offset: None,
        wake: None,
    };
    let stream = state
        .services
        .host
        .exec_stream(sandbox_id, exec_req)
        .await?;
    let body = exec_stdout_bytestream(stream.events);
    let ext = ext_from_path(path);
    Ok(process_upload(
        state,
        session,
        body,
        &ext,
        sanitize_caption(caption),
        Trust::Trusted,
    )
    .await?)
}

/// File extension for a stored media type — used only for the
/// `Content-Disposition` filename (not for sniffing). Unknown → `bin`.
fn ext_for_media(media_type: &str) -> &'static str {
    match media_type {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "video/mp4" => "mp4",
        "video/webm" => "webm",
        _ => "bin",
    }
}

fn decode_upload_header(headers: &HeaderMap) -> Result<UploadRequest, String> {
    let raw = headers
        .get("x-engram-upload")
        .ok_or_else(|| "missing X-Engram-Upload header".to_string())?
        .to_str()
        .map_err(|_| "X-Engram-Upload not ASCII".to_string())?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(raw)
        .map_err(|e| format!("X-Engram-Upload base64: {e}"))?;
    bincode::deserialize(&bytes).map_err(|e| format!("X-Engram-Upload bincode: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn exec_bytestream_reports_transport_loss_after_delivered_bytes() {
        let events = futures::stream::iter([
            ExecEvent::Stdout(Bytes::from_static(b"hello ")),
            ExecEvent::Stdout(Bytes::from_static(b"world")),
        ]);
        let mut body = exec_stdout_bytestream(Box::pin(events));

        assert!(matches!(
            body.next().await,
            Some(Ok(bytes)) if bytes == Bytes::from_static(b"hello ")
        ));
        assert!(matches!(
            body.next().await,
            Some(Ok(bytes)) if bytes == Bytes::from_static(b"world")
        ));
        match body.next().await {
            Some(Err(BlobError::Protocol(message))) => {
                assert!(message.contains("transport"));
                assert!(message.contains("truncated"));
            }
            other => panic!("expected missing-Exit protocol error, got {other:?}"),
        }
        assert!(body.next().await.is_none());
    }

    #[tokio::test]
    async fn exec_bytestream_zero_exit_ends_cleanly() {
        let events = futures::stream::iter([
            ExecEvent::Stdout(Bytes::from_static(b"complete")),
            ExecEvent::Exit(Some(0)),
        ]);
        let mut body = exec_stdout_bytestream(Box::pin(events));

        assert!(matches!(
            body.next().await,
            Some(Ok(bytes)) if bytes == Bytes::from_static(b"complete")
        ));
        assert!(body.next().await.is_none());
    }

    #[test]
    fn detects_image_and_video_signatures() {
        assert_eq!(
            detect_media_type(b"\x89PNG\r\n\x1a\n....."),
            Some("image/png")
        );
        assert_eq!(
            detect_media_type(b"\xff\xd8\xff\xe0...."),
            Some("image/jpeg")
        );
        assert_eq!(detect_media_type(b"GIF89a......"), Some("image/gif"));
        assert_eq!(
            detect_media_type(b"RIFF\x00\x00\x00\x00WEBPVP8 "),
            Some("image/webp")
        );
        assert_eq!(
            detect_media_type(b"\x00\x00\x00\x18ftypmp42"),
            Some("video/mp4")
        );
        assert_eq!(
            detect_media_type(b"\x1a\x45\xdf\xa3...."),
            Some("video/webm")
        );
    }

    #[test]
    fn rejects_non_media_signatures() {
        // SVG / HTML / arbitrary text are not media.
        assert_eq!(detect_media_type(b"<svg xmlns=\"http"), None);
        assert_eq!(detect_media_type(b"<!DOCTYPE html>."), None);
        assert_eq!(detect_media_type(b"#!/bin/sh\necho ."), None);
    }

    #[test]
    fn sanitize_caption_strips_control_and_caps_length() {
        assert_eq!(
            sanitize_caption(Some("hello\nworld\t!".into())).as_deref(),
            Some("helloworld!")
        );
        assert_eq!(sanitize_caption(Some("".into())), None);
        assert_eq!(sanitize_caption(None), None);
        let long: String = "x".repeat(1000);
        assert_eq!(sanitize_caption(Some(long)).unwrap().chars().count(), 280);
    }
}
