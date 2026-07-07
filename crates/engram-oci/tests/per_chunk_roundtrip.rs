//! ADR 0036 integration test: per-chunk OCI artifact round-trip.
//!
//! Exercises the full wire protocol of the per-chunk image format
//! against a fake registry that speaks the OCI Distribution push +
//! pull verbs:
//!
//! 1. **Delta push** — `blob_exists` (HEAD) probes, `push_chunk_blob`
//!    uploads only missing chunks, `push_chunked_image_manifest`
//!    records one layer descriptor per chunk. A second push of the
//!    same content skips every chunk (the cross-bake dedup that
//!    makes deterministic re-bakes upload only their delta).
//! 2. **Metadata pull** — `pull_template_metadata` retrieves the
//!    small layers and downloads **zero** chunk bytes (the coord's
//!    enable path must stay RAM-bounded).
//! 3. **Chunk pull** — `pull_chunk` fetches each chunk by its own
//!    digest, verified against the digest by `oci-client`.
//! 4. **Image pull** — `pull_image` lands the metadata files on disk
//!    and likewise skips chunk layers.
//!
//! The fake registry is in-memory axum; the test runs in the plain
//! workspace nextest pass with zero external dependencies.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use axum::extract::{Path as AxPath, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::Router;
use bytes::Bytes;
use engram_chunk_store::ChunkHash;
use engram_oci::{AnonymousResolver, ChunkLayerRef, ChunkedImageLayers, OciClient};
use parking_lot::Mutex;
use tokio::sync::oneshot;

// ---------------------------------------------------------------
// Fake OCI registry with push support.
// ---------------------------------------------------------------

#[derive(Clone, Default)]
struct Registry {
    /// digest → bytes for completed blobs.
    blobs: Arc<Mutex<HashMap<String, Bytes>>>,
    /// upload session id → accumulated bytes.
    sessions: Arc<Mutex<HashMap<String, Vec<u8>>>>,
    /// tag → (content_type, manifest bytes).
    manifests: Arc<Mutex<HashMap<String, (String, Bytes)>>>,
    /// Counters the assertions read.
    blob_uploads: Arc<AtomicUsize>,
    chunk_blob_gets: Arc<AtomicUsize>,
}

async fn v2_root() -> StatusCode {
    StatusCode::OK
}

async fn get_blob(
    State(reg): State<Registry>,
    AxPath((_repo, digest)): AxPath<(String, String)>,
) -> Response {
    let Some(body) = reg.blobs.lock().get(&digest).cloned() else {
        return (StatusCode::NOT_FOUND, "no such blob").into_response();
    };
    reg.chunk_blob_gets.fetch_add(1, Ordering::Relaxed);
    Response::builder()
        .status(StatusCode::OK)
        .header(http::header::CONTENT_LENGTH, body.len())
        .header(http::header::CONTENT_TYPE, "application/octet-stream")
        .body(axum::body::Body::from(body))
        .unwrap()
}

async fn begin_upload(State(reg): State<Registry>, AxPath(repo): AxPath<String>) -> Response {
    let id = uuid::Uuid::new_v4().to_string();
    reg.sessions.lock().insert(id.clone(), Vec::new());
    Response::builder()
        .status(StatusCode::ACCEPTED)
        .header(
            http::header::LOCATION,
            format!("/v2/{repo}/blobs/uploads/{id}"),
        )
        .body(axum::body::Body::empty())
        .unwrap()
}

async fn patch_upload(
    State(reg): State<Registry>,
    AxPath((repo, id)): AxPath<(String, String)>,
    body: Bytes,
) -> Response {
    let mut sessions = reg.sessions.lock();
    let Some(buf) = sessions.get_mut(&id) else {
        return (StatusCode::NOT_FOUND, "no such session").into_response();
    };
    buf.extend_from_slice(&body);
    Response::builder()
        .status(StatusCode::ACCEPTED)
        .header(
            http::header::LOCATION,
            format!("/v2/{repo}/blobs/uploads/{id}"),
        )
        .body(axum::body::Body::empty())
        .unwrap()
}

async fn put_upload(
    State(reg): State<Registry>,
    AxPath((repo, id)): AxPath<(String, String)>,
    Query(q): Query<HashMap<String, String>>,
    body: Bytes,
) -> Response {
    let Some(mut buf) = reg.sessions.lock().remove(&id) else {
        return (StatusCode::NOT_FOUND, "no such session").into_response();
    };
    buf.extend_from_slice(&body);
    let Some(digest) = q.get("digest").cloned() else {
        return (StatusCode::BAD_REQUEST, "missing digest").into_response();
    };
    let actual = format!("sha256:{:x}", <sha2::Sha256 as sha2::Digest>::digest(&buf));
    if actual != digest {
        return (StatusCode::BAD_REQUEST, "digest mismatch").into_response();
    }
    reg.blobs.lock().insert(digest.clone(), Bytes::from(buf));
    reg.blob_uploads.fetch_add(1, Ordering::Relaxed);
    Response::builder()
        .status(StatusCode::CREATED)
        .header(http::header::LOCATION, format!("/v2/{repo}/blobs/{digest}"))
        .body(axum::body::Body::empty())
        .unwrap()
}

async fn put_manifest(
    State(reg): State<Registry>,
    AxPath((repo, tag)): AxPath<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let content_type = headers
        .get(http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/vnd.oci.image.manifest.v1+json")
        .to_string();
    reg.manifests.lock().insert(tag, (content_type, body));
    Response::builder()
        .status(StatusCode::CREATED)
        .header(http::header::LOCATION, format!("/v2/{repo}/manifests/x"))
        .body(axum::body::Body::empty())
        .unwrap()
}

async fn get_manifest(
    State(reg): State<Registry>,
    AxPath((_repo, tag)): AxPath<(String, String)>,
) -> Response {
    let Some((content_type, body)) = reg.manifests.lock().get(&tag).cloned() else {
        return (StatusCode::NOT_FOUND, "no such manifest").into_response();
    };
    let digest = format!("sha256:{:x}", <sha2::Sha256 as sha2::Digest>::digest(&body));
    Response::builder()
        .status(StatusCode::OK)
        .header(http::header::CONTENT_TYPE, content_type)
        .header("Docker-Content-Digest", digest)
        .header(http::header::CONTENT_LENGTH, body.len())
        .body(axum::body::Body::from(body))
        .unwrap()
}

async fn spawn_registry() -> (SocketAddr, Registry, oneshot::Sender<()>) {
    let reg = Registry::default();
    let app = Router::new()
        .route("/v2/", get(v2_root))
        .route("/v2/:repo/blobs/:digest", get(get_blob))
        .route("/v2/:repo/blobs/uploads/", post(begin_upload))
        .route(
            "/v2/:repo/blobs/uploads/:id",
            axum::routing::patch(patch_upload).put(put_upload),
        )
        .route(
            "/v2/:repo/manifests/:tag",
            put(put_manifest).get(get_manifest),
        )
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

// ---------------------------------------------------------------
// The round-trip.
// ---------------------------------------------------------------

#[tokio::test]
async fn per_chunk_artifact_push_pull_roundtrip() {
    let (addr, reg, _shutdown) = spawn_registry().await;
    let uri = format!("127.0.0.1:{}/roundtrip:warm-1", addr.port());
    let client = OciClient::new(Arc::new(AnonymousResolver));

    // Three "chunks" with distinct content.
    let chunks: Vec<&[u8]> = vec![b"chunk-alpha", b"chunk-beta-longer", b"chunk-gamma"];
    let digests: Vec<String> = chunks
        .iter()
        .map(|c| format!("sha256:{}", ChunkHash::of(c).to_hex()))
        .collect();

    // ---- 1. Delta push: HEAD-skip, then upload missing chunks. ----
    client.auth_for_push(&uri).await.expect("auth");
    let mut pushed = 0;
    for (c, d) in chunks.iter().zip(&digests) {
        if !client.blob_exists(&uri, d).await.expect("HEAD") {
            client
                .push_chunk_blob(&uri, d, c)
                .await
                .expect("push chunk");
            pushed += 1;
        }
    }
    assert_eq!(pushed, 3, "fresh registry: every chunk uploads");
    assert_eq!(reg.blob_uploads.load(Ordering::Relaxed), 3);

    let layers = ChunkedImageLayers {
        config_json: br#"{"kind":"engram-image-v1","runtime_defaults":{"env":{},"workdir":null}}"#
            .to_vec(),
        bundle_json: br#"{"schema_version":2,"bootstrap_disk_available":true}"#.to_vec(),
        disk_bootstrap_json: br#"{"fake":"bootstrap"}"#.to_vec(),
    };
    let chunk_refs: Vec<ChunkLayerRef> = chunks
        .iter()
        .zip(&digests)
        .map(|(c, d)| ChunkLayerRef {
            digest: d.clone(),
            size: c.len() as u64,
        })
        .collect();
    client
        .push_chunked_image_manifest(&uri, layers.clone(), &chunk_refs)
        .await
        .expect("push manifest");

    // ---- 2. Re-push of identical content skips every chunk. ----
    let uploads_before = reg.blob_uploads.load(Ordering::Relaxed);
    let mut skipped = 0;
    for d in &digests {
        if client.blob_exists(&uri, d).await.expect("HEAD") {
            skipped += 1;
        }
    }
    assert_eq!(skipped, 3, "re-push: every chunk HEAD-skips");
    assert_eq!(
        reg.blob_uploads.load(Ordering::Relaxed),
        uploads_before,
        "no blob uploads on the skip sweep"
    );

    // ---- 3. Metadata pull downloads zero chunk bytes. ----
    let gets_before = reg.chunk_blob_gets.load(Ordering::Relaxed);
    let meta = client
        .pull_template_metadata(&uri)
        .await
        .expect("pull metadata");
    assert_eq!(meta.config_json, layers.config_json);
    assert_eq!(meta.bundle_json.as_deref(), Some(&layers.bundle_json[..]));
    assert_eq!(
        meta.disk_bootstrap_json.as_deref(),
        Some(&layers.disk_bootstrap_json[..])
    );
    let metadata_gets = reg.chunk_blob_gets.load(Ordering::Relaxed) - gets_before;
    assert_eq!(
        metadata_gets, 3,
        "metadata pull fetches exactly the 3 small blobs (bundle, bootstrap, config), no chunks"
    );

    // ---- 4. Per-chunk pull, digest-verified by oci-client. ----
    for (c, d) in chunks.iter().zip(&digests) {
        let got = client
            .pull_chunk(&uri, d, c.len() as u64)
            .await
            .expect("pull chunk");
        assert_eq!(got.as_ref(), *c);
    }

    // ---- 5. pull_image lands metadata files, skips chunk layers. ----
    let dest = tempfile::tempdir().unwrap();
    let gets_before = reg.chunk_blob_gets.load(Ordering::Relaxed);
    let pulled = client
        .pull_image(&uri, dest.path())
        .await
        .expect("pull image");
    let bundle_path = pulled.bundle_path.as_ref().expect("bundle on disk");
    assert_eq!(std::fs::read(bundle_path).unwrap(), layers.bundle_json);
    let bs_path = pulled.disk_bootstrap_path.expect("bootstrap on disk");
    assert_eq!(std::fs::read(bs_path).unwrap(), layers.disk_bootstrap_json);
    assert!(pulled.rootfs_path.is_none(), "no rootfs layer pushed");
    let image_gets = reg.chunk_blob_gets.load(Ordering::Relaxed) - gets_before;
    assert_eq!(
        image_gets, 2,
        "pull_image fetches the 2 small layers (bundle, bootstrap) only — chunk layers skipped"
    );
}
