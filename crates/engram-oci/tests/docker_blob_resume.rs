//! Transient-resume tests for the standard-docker layer pull
//! (`pull_docker_blob_to_file`) — the `dev-brain` incident.
//!
//! A fat layer's long single-stream GB download through GKE Cloud NAT
//! gets reset mid-body (reqwest: `error decoding response body`). The
//! pull must keep the bytes already on disk and resume via
//! `Range: bytes=<offset>-` instead of failing the layer (and, upstream,
//! the whole image). These tests drive an in-memory axum registry whose
//! blob handler can cut a body mid-stream, honor or ignore `Range`, or
//! 401 once — the `per_chunk_roundtrip.rs` / `materialize.rs` pattern.
//!
//! Each property is proved with the least data: a 2 KB synthetic blob,
//! a millisecond backoff, no network, no Docker.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::{Path as AxPath, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use bytes::Bytes;
use engram_oci::{AnonymousResolver, BlobRetryConfig, DockerBlobRef, OciClient};
use futures::stream;
use parking_lot::Mutex;
use sha2::Digest as _;
use tokio::sync::oneshot;

// ---------------------------------------------------------------
// Fake registry whose blob handler can misbehave on cue.
// ---------------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    /// Cut the first response mid-body; answer the resume with a clean 206.
    ResumeThenPartial,
    /// Cut the first response mid-body; IGNORE the resume's `Range` and
    /// 200 the whole body from zero.
    ResumeThenFull,
    /// Every response cuts after one byte — never completes.
    AlwaysCut,
    /// 401 the first blob GET (token expired), serve cleanly after.
    Fail401ThenServe,
}

#[derive(Clone)]
struct Registry {
    blob: Bytes,
    mode: Mode,
    /// Number of blob GETs served (also the 0-based index of the current
    /// call).
    gets: Arc<AtomicUsize>,
    /// Every blob GET's `Range` header value, in order — the assertions
    /// read these.
    ranges: Arc<Mutex<Vec<Option<String>>>>,
}

fn sha256_of(bytes: &[u8]) -> String {
    format!("sha256:{}", hex::encode(sha2::Sha256::digest(bytes)))
}

/// Parse the start offset out of a `bytes=<start>-` range header.
fn range_offset(range: Option<&str>) -> usize {
    range
        .and_then(|r| r.strip_prefix("bytes="))
        .and_then(|r| r.split('-').next())
        .and_then(|n| n.parse().ok())
        .unwrap_or(0)
}

/// A 206 that delivers `full[start..start+len]` as one chunk, then errors
/// the body stream — hyper flushes the data frame, then resets the
/// connection, so reqwest yields the partial bytes and THEN an
/// incomplete-body decode error (the prod mid-stream reset).
fn cut(full: &Bytes, start: usize, len: usize) -> Response {
    let end = (start + len).min(full.len());
    let delivered = Bytes::copy_from_slice(&full[start..end]);
    // Yield the data frame, then an await point (so hyper flushes it to
    // the socket), then error — otherwise the two ready-immediately items
    // coalesce and the partial bytes never reach the client.
    let s = stream::unfold(0u8, move |step| {
        let delivered = delivered.clone();
        async move {
            match step {
                0 => Some((Ok::<Bytes, std::io::Error>(delivered), 1u8)),
                1 => {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    Some((
                        Err(std::io::Error::new(
                            std::io::ErrorKind::ConnectionReset,
                            "simulated mid-stream reset",
                        )),
                        2u8,
                    ))
                }
                _ => None,
            }
        }
    });
    Response::builder()
        .status(StatusCode::PARTIAL_CONTENT)
        .body(Body::from_stream(s))
        .unwrap()
}

/// A clean 206 for the remainder `full[start..]`.
fn partial(full: &Bytes, start: usize) -> Response {
    let chunk = Bytes::copy_from_slice(&full[start..]);
    Response::builder()
        .status(StatusCode::PARTIAL_CONTENT)
        .header(axum::http::header::CONTENT_LENGTH, chunk.len())
        .body(Body::from(chunk))
        .unwrap()
}

/// A 200 with the whole body from byte 0 — the range-ignoring registry.
fn full_ok(full: &Bytes) -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header(axum::http::header::CONTENT_LENGTH, full.len())
        .body(Body::from(full.clone()))
        .unwrap()
}

async fn v2_root() -> StatusCode {
    // No WWW-Authenticate → `client.auth` treats the registry as
    // anonymous and returns without minting a token, which is all the
    // re-auth path needs to proceed.
    StatusCode::OK
}

async fn get_blob(
    State(reg): State<Registry>,
    AxPath((_repo, _digest)): AxPath<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let n = reg.gets.fetch_add(1, Ordering::SeqCst);
    let range = headers
        .get(axum::http::header::RANGE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    reg.ranges.lock().push(range.clone());
    let offset = range_offset(range.as_deref());
    let full = &reg.blob;
    let half = full.len() / 2;

    match reg.mode {
        Mode::ResumeThenPartial => {
            if n == 0 {
                cut(full, 0, half)
            } else {
                partial(full, offset)
            }
        }
        Mode::ResumeThenFull => {
            if n == 0 {
                cut(full, 0, half)
            } else {
                full_ok(full)
            }
        }
        Mode::AlwaysCut => cut(full, offset, 1),
        Mode::Fail401ThenServe => {
            if n == 0 {
                (StatusCode::UNAUTHORIZED, "token expired").into_response()
            } else {
                partial(full, offset)
            }
        }
    }
}

async fn spawn(mode: Mode, blob: Bytes) -> (SocketAddr, Registry, oneshot::Sender<()>) {
    let reg = Registry {
        blob,
        mode,
        gets: Arc::new(AtomicUsize::new(0)),
        ranges: Arc::new(Mutex::new(Vec::new())),
    };
    let app = Router::new()
        .route("/v2/", get(v2_root))
        .route("/v2/{repo}/blobs/{digest}", get(get_blob))
        .with_state(reg.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = oneshot::channel::<()>();
    tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = rx.await;
            })
            .await
            .unwrap();
    });
    (addr, reg, tx)
}

/// A client whose blob-retry backoff is milliseconds, so the exhaustion
/// path doesn't sleep for real seconds.
fn fast_client() -> OciClient {
    OciClient::new(Arc::new(AnonymousResolver)).with_blob_retry(BlobRetryConfig {
        max_attempts: 5,
        base_backoff: Duration::from_millis(1),
        rate_limit_backoff: Duration::from_millis(1),
    })
}

fn blob_ref(blob: &Bytes) -> DockerBlobRef {
    DockerBlobRef {
        media_type: "application/vnd.oci.image.layer.v1.tar+gzip".to_string(),
        digest: sha256_of(blob),
        size: blob.len() as u64,
    }
}

// ---------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------

/// (a) A mid-body cut is resumed via `Range` — exactly one resume request
/// at the correct offset, a 206 appended onto the bytes already written,
/// the whole-file digest verifies, and the file is byte-exact.
#[tokio::test]
async fn mid_stream_cut_resumes_via_range() {
    let blob = Bytes::from(vec![7u8; 2000]);
    let (addr, reg, _sd) = spawn(Mode::ResumeThenPartial, blob.clone()).await;
    let uri = format!("127.0.0.1:{}/repo:t", addr.port());
    let dest = tempfile::tempdir().unwrap();
    let path = dest.path().join("layer");

    fast_client()
        .pull_docker_blob_to_file(&uri, &blob_ref(&blob), &path)
        .await
        .expect("resume must recover the cut layer");

    assert_eq!(std::fs::read(&path).unwrap(), blob.as_ref(), "byte-exact");

    // Two GETs: the initial `bytes=0-` (cut at 1000) and the resume.
    assert_eq!(reg.gets.load(Ordering::SeqCst), 2);
    let ranges = reg.ranges.lock();
    let resumes = ranges
        .iter()
        .filter(|r| r.as_deref() == Some("bytes=1000-"))
        .count();
    assert_eq!(
        resumes, 1,
        "exactly one resume request at the post-cut offset; saw {ranges:?}"
    );
}

/// (b) A registry that ignores `Range` and 200s the whole body: the layer
/// file is restarted from zero (never a doubled prefix) and verifies.
#[tokio::test]
async fn range_ignored_restarts_layer() {
    let blob = Bytes::from((0u8..=255).cycle().take(2000).collect::<Vec<_>>());
    let (addr, reg, _sd) = spawn(Mode::ResumeThenFull, blob.clone()).await;
    let uri = format!("127.0.0.1:{}/repo:t", addr.port());
    let dest = tempfile::tempdir().unwrap();
    let path = dest.path().join("layer");

    fast_client()
        .pull_docker_blob_to_file(&uri, &blob_ref(&blob), &path)
        .await
        .expect("range-ignoring 200 must restart + verify");

    // The keystone: exactly the blob, not blob-prefix ++ blob.
    assert_eq!(
        std::fs::read(&path).unwrap(),
        blob.as_ref(),
        "restarted file is byte-exact (no doubled prefix)"
    );
    assert_eq!(reg.gets.load(Ordering::SeqCst), 2);
}

/// (c) A persistently-cutting registry exhausts the attempt budget and
/// errors — the message names the layer digest and the attempt count.
#[tokio::test]
async fn persistent_failure_exhausts_attempts() {
    let blob = Bytes::from(vec![3u8; 2000]);
    let digest = sha256_of(&blob);
    let (addr, reg, _sd) = spawn(Mode::AlwaysCut, blob.clone()).await;
    let uri = format!("127.0.0.1:{}/repo:t", addr.port());
    let dest = tempfile::tempdir().unwrap();
    let path = dest.path().join("layer");

    let err = fast_client()
        .pull_docker_blob_to_file(&uri, &blob_ref(&blob), &path)
        .await
        .expect_err("must give up after N attempts");

    let msg = err.to_string();
    assert!(msg.contains(&digest), "error names the layer digest: {msg}");
    assert!(
        msg.contains("5 attempt"),
        "error names the exhausted attempt count: {msg}"
    );
    assert_eq!(
        reg.gets.load(Ordering::SeqCst),
        5,
        "exactly max_attempts blob GETs"
    );
}

/// (d) A 401 mid-run is still recovered by one re-auth + retry — and it
/// does NOT consume a transient-retry attempt.
#[tokio::test]
async fn reauth_on_401_still_works() {
    let blob = Bytes::from(vec![9u8; 2000]);
    let (addr, reg, _sd) = spawn(Mode::Fail401ThenServe, blob.clone()).await;
    let uri = format!("127.0.0.1:{}/repo:t", addr.port());
    let dest = tempfile::tempdir().unwrap();
    let path = dest.path().join("layer");

    fast_client()
        .pull_docker_blob_to_file(&uri, &blob_ref(&blob), &path)
        .await
        .expect("re-auth must recover the expired token");

    assert_eq!(std::fs::read(&path).unwrap(), blob.as_ref());
    // GET #0 → 401, GET #1 → served. The re-auth is free of the budget.
    assert_eq!(reg.gets.load(Ordering::SeqCst), 2);
}
