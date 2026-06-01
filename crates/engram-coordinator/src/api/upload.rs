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
use engram_harness_proto::{
    read_msg, write_msg, UploadOp, UploadRequest, UploadResponse, MAX_ARTIFACT_BYTES,
};
use futures::StreamExt;

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
    /// Constructed by the `from-path` endpoint (ADR 0026 phase 7).
    #[allow(dead_code)]
    Trusted,
}

fn err(message: String) -> UploadResponse {
    UploadResponse::Error { message }
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
pub async fn process_upload(
    state: &SharedState,
    session: SessionId,
    mut src: ByteStream,
    _ext: &str,
    caption: Option<String>,
    trust: Trust,
) -> UploadResponse {
    // Pre-write quota check against existing usage.
    let (count, total) = match state.services.meta.artifact_usage(session).await {
        Ok(u) => u,
        Err(e) => return err(format!("artifact usage lookup: {e}")),
    };
    if count >= MAX_ARTIFACTS_PER_SESSION {
        return err(format!(
            "per-session artifact count limit reached ({MAX_ARTIFACTS_PER_SESSION})"
        ));
    }
    if total >= MAX_ARTIFACT_TOTAL_BYTES_PER_SESSION {
        return err("per-session artifact storage limit reached".into());
    }

    // Pull a sniff prefix before deciding anything (so an untrusted
    // reject writes nothing).
    let mut prefix = BytesMut::new();
    let mut hit_eof = false;
    while prefix.len() < SNIFF_LEN {
        match src.next().await {
            Some(Ok(chunk)) => prefix.extend_from_slice(&chunk),
            Some(Err(e)) => return err(format!("read artifact body: {e}")),
            None => {
                hit_eof = true;
                break;
            }
        }
    }
    if prefix.is_empty() {
        return err("empty artifact body".into());
    }
    let media_type = match (trust, detect_media_type(&prefix)) {
        (Trust::Untrusted, Some(mt)) if is_media(mt) => mt.to_string(),
        (Trust::Untrusted, _) => {
            return err(
                "unsupported media type: only image (png/jpeg/gif/webp) and video \
                 (mp4/webm) may be shared from the sandbox"
                    .into(),
            )
        }
        (Trust::Trusted, Some(mt)) => mt.to_string(),
        (Trust::Trusted, None) => "application/octet-stream".to_string(),
    };

    // Server-generated key — the guest never influences the storage path.
    let artifact_id = uuid::Uuid::new_v4();
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
                yield Err(BlobError::Protocol("artifact exceeds size limit".into()));
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
                            yield Err(BlobError::Protocol("artifact exceeds size limit".into()));
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
            return err(format!("store artifact: {e}"));
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
        return err(format!("record artifact: {e}"));
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
                at: chrono::Utc::now(),
            },
        )
        .await
    {
        tracing::warn!(session = %session, error = %e, "emit FileShared failed (artifact stored)");
    }

    UploadResponse::Shared {
        artifact_id: id_str,
        media_type,
        size_bytes: size,
    }
}

/// Authorize an untrusted in-guest [`UploadRequest`] (broker token) and
/// run the shared core with its body stream. Shared by the vsock and
/// split-mode HTTP transports.
async fn authorized_untrusted_upload(
    state: &SharedState,
    header: UploadRequest,
    body: ByteStream,
) -> UploadResponse {
    if !crate::api::session_auth::authorize_broker_token(
        state,
        header.session_id,
        &header.broker_token,
    ) {
        return err("invalid or missing upload token".into());
    }
    let UploadOp::ShareFile { ext, caption, .. } = header.op;
    process_upload(
        state,
        header.session_id,
        body,
        &ext,
        sanitize_caption(caption),
        Trust::Untrusted,
    )
    .await
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
        Err(message) => return Json(err(message)),
    };
    let src = ByteStream::new(
        body.into_data_stream()
            .map(|r| r.map_err(|e| BlobError::Protocol(format!("recv artifact body: {e}")))),
    );
    Json(authorized_untrusted_upload(&state, header, src).await)
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
