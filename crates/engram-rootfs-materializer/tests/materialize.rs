//! ADR 0080 §C integration tests against a tiny in-memory OCI
//! registry (axum — the `per_chunk_roundtrip.rs` pattern from
//! engram-oci): platform selection from a manifest index, the
//! fail-loud unknown-mediaType policy, scratch scrubbing, and the
//! keystone determinism property (two full materialize runs over the
//! same fixtures → the SAME content-derived manifest ref).
//!
//! Fixtures are KB-sized synthetic tar layers — least data that
//! proves each property. No Docker, no network, no root.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use axum::extract::{Path as AxPath, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use bytes::Bytes;
use engram_oci::{AnonymousResolver, OciClient};
use engram_rootfs_materializer::{
    pull_image, InitInjection, LayerCompression, Materializer, Platform, Transport,
};
use parking_lot::Mutex;
use sha2::Digest as _;
use tokio::sync::oneshot;

// ---------------------------------------------------------------
// Fake pull-only OCI registry.
// ---------------------------------------------------------------

#[derive(Clone, Default)]
struct Registry {
    /// digest → bytes.
    blobs: Arc<Mutex<HashMap<String, Bytes>>>,
    /// tag OR digest → (content_type, manifest bytes).
    manifests: Arc<Mutex<HashMap<String, (String, Bytes)>>>,
    blob_gets: Arc<AtomicUsize>,
}

fn sha256_of(bytes: &[u8]) -> String {
    format!("sha256:{:x}", sha2::Sha256::digest(bytes))
}

impl Registry {
    /// Store a blob, returning its digest.
    fn add_blob(&self, bytes: Vec<u8>) -> (String, u64) {
        let digest = sha256_of(&bytes);
        let size = bytes.len() as u64;
        self.blobs.lock().insert(digest.clone(), Bytes::from(bytes));
        (digest, size)
    }

    /// Store a manifest under its own digest (and optionally a tag).
    fn add_manifest(&self, tag: Option<&str>, content_type: &str, bytes: Vec<u8>) -> (String, u64) {
        let digest = sha256_of(&bytes);
        let size = bytes.len() as u64;
        let body = Bytes::from(bytes);
        let mut m = self.manifests.lock();
        m.insert(digest.clone(), (content_type.to_string(), body.clone()));
        if let Some(t) = tag {
            m.insert(t.to_string(), (content_type.to_string(), body));
        }
        (digest, size)
    }
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
    reg.blob_gets.fetch_add(1, Ordering::Relaxed);
    Response::builder()
        .status(StatusCode::OK)
        .header(http::header::CONTENT_LENGTH, body.len())
        .header(http::header::CONTENT_TYPE, "application/octet-stream")
        .body(axum::body::Body::from(body))
        .unwrap()
}

async fn get_manifest(
    State(reg): State<Registry>,
    AxPath((_repo, reference)): AxPath<(String, String)>,
) -> Response {
    let Some((content_type, body)) = reg.manifests.lock().get(&reference).cloned() else {
        return (StatusCode::NOT_FOUND, "no such manifest").into_response();
    };
    let digest = sha256_of(&body);
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
        .route("/v2/:repo/manifests/:reference", get(get_manifest))
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
// Fixture image: an OCI index with linux/arm64 + linux/amd64 entries
// (plus a buildx-style attestation entry that must never match).
// ---------------------------------------------------------------

fn tar_file(builder: &mut tar::Builder<Vec<u8>>, path: &str, mode: u32, body: &[u8]) {
    let mut h = tar::Header::new_gnu();
    h.set_mode(mode);
    h.set_uid(0);
    h.set_gid(0);
    h.set_size(body.len() as u64);
    h.set_mtime(946_684_800); // 2000-01-01: deterministic fixture mtimes
    h.set_entry_type(tar::EntryType::Regular);
    builder.append_data(&mut h, path, body).unwrap();
}

fn gzip(bytes: &[u8]) -> Vec<u8> {
    use std::io::Write;
    let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    enc.write_all(bytes).unwrap();
    enc.finish().unwrap()
}

/// Two tiny layers per platform: a gzip base + a zstd upper (exercises
/// both decompressors through the full pipeline), with arch-specific
/// content so platform selection is observable in the tree.
fn platform_layers(arch: &str) -> (Vec<u8>, Vec<u8>) {
    let mut base = tar::Builder::new(Vec::new());
    tar_file(&mut base, "bin/sh", 0o755, b"#!fixture-shell");
    tar_file(&mut base, "etc/arch", 0o644, format!("{arch}\n").as_bytes());
    base.finish().unwrap();
    let base = base.into_inner().unwrap();

    let mut upper = tar::Builder::new(Vec::new());
    tar_file(&mut upper, "etc/layer2", 0o600, b"upper");
    upper.finish().unwrap();
    let upper = upper.into_inner().unwrap();

    (gzip(&base), zstd::encode_all(&upper[..], 0).unwrap())
}

struct Fixture {
    /// `127.0.0.1:<port>/img:latest`
    uri: String,
    reg: Registry,
    _shutdown: oneshot::Sender<()>,
}

/// Publish the fixture image. `layer_media_types` overrides the two
/// layers' mediaTypes for the fail-loud test (`None` = the correct
/// gzip/zstd pair).
async fn publish_fixture(layer_media_types: Option<[&str; 2]>) -> Fixture {
    let (addr, reg, shutdown) = spawn_registry().await;
    let [gzip_mt, zstd_mt] = layer_media_types.unwrap_or([
        "application/vnd.oci.image.layer.v1.tar+gzip",
        "application/vnd.oci.image.layer.v1.tar+zstd",
    ]);

    let mut index_entries = Vec::new();
    for arch in ["arm64", "amd64"] {
        let (l0, l1) = platform_layers(arch);
        let (l0_digest, l0_size) = reg.add_blob(l0);
        let (l1_digest, l1_size) = reg.add_blob(l1);

        let config = serde_json::json!({
            "architecture": arch,
            "os": "linux",
            "config": {
                "Env": [format!("FIXTURE_ARCH={arch}"), "PATH=/usr/bin".to_string()],
                "WorkingDir": "/workspace"
            },
            "rootfs": { "type": "layers", "diff_ids": [] }
        });
        let (cfg_digest, cfg_size) = reg.add_blob(serde_json::to_vec(&config).unwrap());

        let manifest = serde_json::json!({
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "config": {
                "mediaType": "application/vnd.oci.image.config.v1+json",
                "digest": cfg_digest,
                "size": cfg_size
            },
            "layers": [
                { "mediaType": gzip_mt, "digest": l0_digest, "size": l0_size },
                { "mediaType": zstd_mt, "digest": l1_digest, "size": l1_size }
            ]
        });
        let (m_digest, m_size) = reg.add_manifest(
            None,
            "application/vnd.oci.image.manifest.v1+json",
            serde_json::to_vec(&manifest).unwrap(),
        );
        index_entries.push(serde_json::json!({
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "digest": m_digest,
            "size": m_size,
            "platform": { "os": "linux", "architecture": arch }
        }));
    }
    // buildx-style attestation entry: platform unknown/unknown — the
    // selector must skip it.
    index_entries.push(serde_json::json!({
        "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "digest": "sha256:0000000000000000000000000000000000000000000000000000000000000000",
        "size": 1,
        "platform": { "os": "unknown", "architecture": "unknown" }
    }));

    let index = serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.index.v1+json",
        "manifests": index_entries
    });
    reg.add_manifest(
        Some("latest"),
        "application/vnd.oci.image.index.v1+json",
        serde_json::to_vec(&index).unwrap(),
    );

    Fixture {
        uri: format!("127.0.0.1:{}/img:latest", addr.port()),
        reg,
        _shutdown: shutdown,
    }
}

fn oci_client() -> OciClient {
    OciClient::new(Arc::new(AnonymousResolver))
}

// ---------------------------------------------------------------
// Pull stage: platform selection + defaults extraction.
// ---------------------------------------------------------------

#[tokio::test]
async fn pull_selects_platform_from_index_and_extracts_defaults() {
    let fx = publish_fixture(None).await;
    let client = oci_client();

    for (platform, arch) in [
        (Platform::LinuxArm64, "arm64"),
        (Platform::LinuxAmd64, "amd64"),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let pulled = pull_image(&client, &fx.uri, platform, dir.path())
            .await
            .unwrap_or_else(|e| panic!("pull {arch}: {e}"));

        // Config defaults come from THIS platform's config blob.
        assert_eq!(
            pulled
                .oci_defaults
                .env
                .get("FIXTURE_ARCH")
                .map(String::as_str),
            Some(arch),
            "config blob must be the {arch} one"
        );
        assert_eq!(pulled.oci_defaults.workdir.as_deref(), Some("/workspace"));

        // Both layers landed, compression classified from mediaType.
        assert_eq!(pulled.layers.len(), 2);
        assert_eq!(pulled.layers[0].compression, LayerCompression::Gzip);
        assert_eq!(pulled.layers[1].compression, LayerCompression::Zstd);
        for layer in &pulled.layers {
            let bytes = std::fs::read(&layer.path).unwrap();
            assert_eq!(sha256_of(&bytes), layer.digest, "digest-faithful bytes");
        }
        assert!(pulled.compressed_bytes > 0);
    }
}

/// The ADR's "fail loud on unknown media types": the offending
/// mediaType is named in the error, and NO blob bytes are downloaded
/// (validation happens on the manifest, before any transfer).
#[tokio::test]
async fn unknown_layer_media_type_fails_loud_before_any_download() {
    let fx = publish_fixture(Some([
        "application/vnd.oci.image.layer.v1.tar+gzip",
        "application/vnd.fancy.layer.v9+brotli",
    ]))
    .await;
    let client = oci_client();

    let dir = tempfile::tempdir().unwrap();
    let err = pull_image(&client, &fx.uri, Platform::LinuxArm64, dir.path())
        .await
        .expect_err("bogus mediaType must fail the pull");
    assert!(
        err.to_string()
            .contains("application/vnd.fancy.layer.v9+brotli"),
        "error must name the mediaType: {err}"
    );
    assert_eq!(
        fx.reg.blob_gets.load(Ordering::Relaxed),
        0,
        "must fail before downloading any blob (config included)"
    );
}

// ---------------------------------------------------------------
// Full pipeline: determinism + scratch discipline (needs mke2fs).
// ---------------------------------------------------------------

/// ADR 0036 byte-determinism hinges on SOURCE_DATE_EPOCH, honored by
/// e2fsprogs >= 1.47.1 (the same >=1.47.1 e2fsprogs determinism
/// gate). Older/missing mke2fs → skip with a note, never flake.
fn deterministic_mke2fs_available() -> Option<String> {
    let on_path = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|dir| dir.join("mke2fs").is_file()))
        .unwrap_or(false);
    if !on_path {
        return None;
    }
    let out = std::process::Command::new("mke2fs")
        .arg("-V")
        .output()
        .ok()?;
    // `mke2fs -V` prints e.g. "mke2fs 1.47.2 (1-Jan-2025)" to stderr.
    let text = String::from_utf8_lossy(&out.stderr).into_owned();
    let ver = text.split_whitespace().find_map(|tok| {
        let mut it = tok.split('.');
        let a: u32 = it.next()?.parse().ok()?;
        let b: u32 = it.next()?.parse().ok()?;
        let c: u32 = it.next()?.parse().ok()?;
        Some((a, b, c))
    });
    matches!(ver, Some(v) if v >= (1, 47, 1)).then_some(text)
}

fn materializer() -> Materializer {
    Materializer::new(
        oci_client(),
        InitInjection {
            vsock_port: 1024,
            transport: Transport::Vsock,
            init_script: None,
        },
    )
}

/// The keystone: two FULL runs (pull → flatten → inject → pack →
/// chunk) over the same fixtures produce the SAME content-derived
/// manifest ref — what keeps enable-time snapshot reuse working — and
/// every run scrubs its scratch, success or not.
#[tokio::test]
async fn materialize_is_deterministic_and_scrubs_scratch() {
    let Some(ver) = deterministic_mke2fs_available() else {
        eprintln!(
            "mke2fs missing or < 1.47.1 (no SOURCE_DATE_EPOCH); skipping determinism test \
             (use the flake-pinned mke2fs: `nix develop`)"
        );
        return;
    };
    eprintln!("using {}", ver.trim());

    let fx = publish_fixture(None).await;
    let store_dir = tempfile::tempdir().unwrap();
    let chunk_store = engram_chunk_store::ChunkStore::new(Arc::new(
        engram_storage_local::LocalBlobStorage::new(store_dir.path().to_path_buf()),
    ));
    let scratch = tempfile::tempdir().unwrap();
    let m = materializer();

    // Phase 3b: the RPC's progress frames — assert the honest stage
    // sequence rides the optional sender.
    let (ptx, mut prx) = tokio::sync::mpsc::channel(32);
    let first = m
        .materialize(
            &fx.uri,
            Platform::LinuxArm64,
            scratch.path(),
            &chunk_store,
            0,
            Some(ptx),
        )
        .await
        .expect("first materialize");
    let mut frames = Vec::new();
    while let Ok(p) = prx.try_recv() {
        frames.push(p);
    }
    use engram_core::types::MaterializeStage as S;
    // One frame per stage TRANSITION in pipeline order, plus intra-stage
    // chunk-window progress frames (repeated stage Chunk carrying
    // chunks_done/chunks_total — the enable UI's progress bar), so
    // dedup consecutive stages before asserting the order.
    let mut stages: Vec<S> = frames.iter().map(|p| p.stage).collect();
    stages.dedup();
    assert_eq!(
        stages,
        vec![S::Pull, S::Flatten, S::Pack, S::Chunk],
        "stage transitions in pipeline order (intra-stage progress deduped)"
    );
    let last_chunk = frames
        .iter()
        .rfind(|p| p.stage == S::Chunk)
        .expect("at least one chunk frame");
    assert_eq!(
        last_chunk.chunks_done, last_chunk.chunks_total,
        "the final chunk-window frame reports completion"
    );
    assert!(
        last_chunk.chunks_total.is_some_and(|t| t > 0),
        "chunk-window counts ride the final chunk frame: {last_chunk:?}"
    );
    assert!(
        std::fs::read_dir(scratch.path()).unwrap().next().is_none(),
        "scratch must be scrubbed after a successful run"
    );
    assert!(first.ext4_size_bytes > 0);
    assert!(
        first.manifest_digest.starts_with("sha256:"),
        "the platform manifest digest must ride the result: {}",
        first.manifest_digest
    );
    assert_eq!(
        first
            .oci_defaults
            .env
            .get("FIXTURE_ARCH")
            .map(String::as_str),
        Some("arm64")
    );
    // The manifest is committed and resolvable.
    chunk_store
        .get_manifest(first.disk_manifest)
        .await
        .expect("manifest committed to the store");

    let second = m
        .materialize(
            &fx.uri,
            Platform::LinuxArm64,
            scratch.path(),
            &chunk_store,
            0,
            None,
        )
        .await
        .expect("second materialize");
    assert_eq!(
        first.disk_manifest, second.disk_manifest,
        "deterministic double-run must reproduce the SAME content-derived manifest ref \
         (fixed FS UUID + hash_seed + SOURCE_DATE_EPOCH + mtime clamp)"
    );
    assert_eq!(first.ext4_size_bytes, second.ext4_size_bytes);
}

/// Scrub-on-error: a failing materialize (unknown layer mediaType)
/// leaves nothing behind in the caller's scratch dir.
#[tokio::test]
async fn materialize_scrubs_scratch_on_error() {
    let fx = publish_fixture(Some([
        "application/vnd.oci.image.layer.v1.tar+gzip",
        "application/vnd.fancy.layer.v9+brotli",
    ]))
    .await;
    let store_dir = tempfile::tempdir().unwrap();
    let chunk_store = engram_chunk_store::ChunkStore::new(Arc::new(
        engram_storage_local::LocalBlobStorage::new(store_dir.path().to_path_buf()),
    ));
    let scratch = tempfile::tempdir().unwrap();

    let err = materializer()
        .materialize(
            &fx.uri,
            Platform::LinuxArm64,
            scratch.path(),
            &chunk_store,
            0,
            None,
        )
        .await
        .expect_err("must fail on the bogus mediaType");
    assert!(err.to_string().contains("vnd.fancy.layer.v9+brotli"));
    assert!(
        std::fs::read_dir(scratch.path()).unwrap().next().is_none(),
        "scratch must be scrubbed on the error path too"
    );
}

/// ADR 0093 addendum: `min_fs_size_bytes` floors the packed ext4 so
/// `suggested_disk_gib` grants real workspace. Sized to the property
/// (256 MiB, not GiB): the floor lands verbatim in `ext4_size_bytes`,
/// the padding is zero-elided in the manifest (chunk bytes stay far
/// below the fs size), and the padded image still checksum-verifies.
#[tokio::test]
async fn materialize_honors_the_min_fs_size_floor() {
    const FLOOR: u64 = 256 * 1024 * 1024;
    let fx = publish_fixture(None).await;
    let store_dir = tempfile::tempdir().unwrap();
    let chunk_store = engram_chunk_store::ChunkStore::new(Arc::new(
        engram_storage_local::LocalBlobStorage::new(store_dir.path().to_path_buf()),
    ));
    let scratch = tempfile::tempdir().unwrap();

    let out = materializer()
        .materialize(
            &fx.uri,
            Platform::LinuxArm64,
            scratch.path(),
            &chunk_store,
            FLOOR,
            None,
        )
        .await
        .expect("materialize with a floor");
    assert_eq!(out.ext4_size_bytes, FLOOR);

    let manifest = chunk_store.get_manifest(out.disk_manifest).await.unwrap();
    assert_eq!(manifest.total_bytes, FLOOR);
    let stored =
        manifest.chunks.len() as u64 * engram_chunk_store::manifest::DEFAULT_DISK_CHUNK_SIZE;
    assert!(
        stored < FLOOR / 2,
        "zero padding must be elided from the manifest (~{stored} stored of {FLOOR})"
    );
    // The padded image reassembles and verifies clean (zero-fill gaps
    // round-trip through the store).
    image_from_store(&chunk_store, out.disk_manifest).await;
}

// ---------------------------------------------------------------
// Pull/flatten pipeline (ADR 0088 addendum).
// ---------------------------------------------------------------

/// Reassemble the materialized image from the chunk store (manifest
/// gaps are zero-fill) and open it with the mkext4 verification
/// reader — the streaming pack produces no image file, so the store
/// IS the artifact (ADR 0093). Checksum-verifies before returning.
async fn image_from_store(
    chunk_store: &engram_chunk_store::ChunkStore,
    manifest_ref: engram_chunk_store::ManifestRef,
) -> Vec<u8> {
    let manifest = chunk_store.get_manifest(manifest_ref).await.unwrap();
    let mut image = vec![0u8; manifest.total_bytes as usize];
    for c in &manifest.chunks {
        let bytes = chunk_store.get_chunk(c.hash).await.unwrap();
        image[c.offset as usize..c.offset as usize + bytes.len()].copy_from_slice(&bytes);
    }
    let fs = mkext4::reader::Fs::open(&image[..]).unwrap();
    let issues = fs.verify().unwrap();
    assert!(issues.is_empty(), "image must verify clean: {issues:?}");
    image
}

fn read_path(image: &[u8], path: &str) -> Option<Vec<u8>> {
    let fs = mkext4::reader::Fs::open(image).unwrap();
    let ino = fs.resolve(path).ok()?;
    fs.read_file(ino).ok()
}

/// Publish an arm64-only image whose layers are the given raw tars
/// (gzipped on the wire).
async fn publish_gzip_layers(layer_tars: Vec<Vec<u8>>) -> Fixture {
    let (addr, reg, shutdown) = spawn_registry().await;
    let mut layer_descs = Vec::new();
    for tar in layer_tars {
        let (digest, size) = reg.add_blob(gzip(&tar));
        layer_descs.push(serde_json::json!({
            "mediaType": "application/vnd.oci.image.layer.v1.tar+gzip",
            "digest": digest,
            "size": size
        }));
    }
    let config = serde_json::json!({
        "architecture": "arm64",
        "os": "linux",
        "config": { "Env": [], "WorkingDir": "/" },
        "rootfs": { "type": "layers", "diff_ids": [] }
    });
    let (cfg_digest, cfg_size) = reg.add_blob(serde_json::to_vec(&config).unwrap());
    let manifest = serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "config": {
            "mediaType": "application/vnd.oci.image.config.v1+json",
            "digest": cfg_digest,
            "size": cfg_size
        },
        "layers": layer_descs
    });
    reg.add_manifest(
        Some("latest"),
        "application/vnd.oci.image.manifest.v1+json",
        serde_json::to_vec(&manifest).unwrap(),
    );
    Fixture {
        uri: format!("127.0.0.1:{}/img:latest", addr.port()),
        reg,
        _shutdown: shutdown,
    }
}

/// Three layers whose semantics only hold under IN-ORDER apply
/// (layer 2 whiteouts layer 1's file; layer 3 overwrites layer 2's):
/// the buffered download pipeline must deliver in manifest order even
/// though downloads run concurrently.
#[tokio::test]
async fn pipelined_layers_apply_in_order() {
    let mut l1 = tar::Builder::new(Vec::new());
    tar_file(&mut l1, "a", 0o644, b"v1");
    tar_file(&mut l1, "keep", 0o644, b"k");
    l1.finish().unwrap();
    let mut l2 = tar::Builder::new(Vec::new());
    tar_file(&mut l2, ".wh.a", 0o644, b"");
    tar_file(&mut l2, "b", 0o644, b"v2");
    l2.finish().unwrap();
    let mut l3 = tar::Builder::new(Vec::new());
    tar_file(&mut l3, "b", 0o644, b"v3-longer");
    l3.finish().unwrap();

    let fx = publish_gzip_layers(vec![
        l1.into_inner().unwrap(),
        l2.into_inner().unwrap(),
        l3.into_inner().unwrap(),
    ])
    .await;
    let store_dir = tempfile::tempdir().unwrap();
    let chunk_store = engram_chunk_store::ChunkStore::new(Arc::new(
        engram_storage_local::LocalBlobStorage::new(store_dir.path().to_path_buf()),
    ));
    let scratch = tempfile::tempdir().unwrap();
    let out = materializer()
        .materialize(
            &fx.uri,
            Platform::LinuxArm64,
            scratch.path(),
            &chunk_store,
            0,
            None,
        )
        .await
        .expect("pipelined materialize");

    let image = image_from_store(&chunk_store, out.disk_manifest).await;
    assert!(
        read_path(&image, "/a").is_none(),
        "layer-2 whiteout must remove layer-1's file"
    );
    assert_eq!(read_path(&image, "/keep").as_deref(), Some(&b"k"[..]));
    assert_eq!(
        read_path(&image, "/b").as_deref(),
        Some(&b"v3-longer"[..]),
        "layer 3 must overwrite layer 2 (in-order apply)"
    );
}

/// A mid-pull failure (layer blob vanishes) fails the materialize with
/// the PULL error (not the flattener's truncation echo) and scrubs
/// scratch.
#[tokio::test]
async fn mid_pull_failure_fails_and_scrubs() {
    let mut l1 = tar::Builder::new(Vec::new());
    tar_file(&mut l1, "ok", 0o644, b"fine");
    l1.finish().unwrap();
    let mut l2 = tar::Builder::new(Vec::new());
    tar_file(&mut l2, "later", 0o644, b"never lands");
    l2.finish().unwrap();

    let fx = publish_gzip_layers(vec![l1.into_inner().unwrap(), l2.into_inner().unwrap()]).await;
    // Vanish layer 2's blob AFTER the manifest is published: resolve
    // succeeds, the download 404s.
    {
        let mut blobs = fx.reg.blobs.lock();
        let gone: Vec<String> = blobs
            .iter()
            .filter(|(_, v)| {
                let mut gz = flate2::read::GzDecoder::new(std::io::Cursor::new(v.to_vec()));
                let mut out = Vec::new();
                std::io::Read::read_to_end(&mut gz, &mut out)
                    .map(|_| out.windows(5).any(|w| w == b"later"))
                    .unwrap_or(false)
            })
            .map(|(k, _)| k.clone())
            .collect();
        assert_eq!(gone.len(), 1, "exactly the second layer blob");
        for k in gone {
            blobs.remove(&k);
        }
    }

    let store_dir = tempfile::tempdir().unwrap();
    let chunk_store = engram_chunk_store::ChunkStore::new(Arc::new(
        engram_storage_local::LocalBlobStorage::new(store_dir.path().to_path_buf()),
    ));
    let scratch = tempfile::tempdir().unwrap();

    let err = materializer()
        .materialize(
            &fx.uri,
            Platform::LinuxArm64,
            scratch.path(),
            &chunk_store,
            0,
            None,
        )
        .await
        .expect_err("vanished layer blob must fail the materialize");
    assert!(
        err.to_string().starts_with("pull:"),
        "the pull error must win over the flatten truncation echo: {err}"
    );
    assert!(
        std::fs::read_dir(scratch.path()).unwrap().next().is_none(),
        "scratch must be scrubbed on the mid-pull error path"
    );
}

/// Small-image fast path: a single tiny layer round-trips through the
/// pipeline with no behavioral change (the no-regression guard for
/// demo-class images).
#[tokio::test]
async fn single_layer_small_image_fast_path() {
    let mut l1 = tar::Builder::new(Vec::new());
    tar_file(&mut l1, "hello", 0o644, b"world");
    l1.finish().unwrap();
    let fx = publish_gzip_layers(vec![l1.into_inner().unwrap()]).await;

    let store_dir = tempfile::tempdir().unwrap();
    let chunk_store = engram_chunk_store::ChunkStore::new(Arc::new(
        engram_storage_local::LocalBlobStorage::new(store_dir.path().to_path_buf()),
    ));
    let scratch = tempfile::tempdir().unwrap();

    let out = materializer()
        .materialize(
            &fx.uri,
            Platform::LinuxArm64,
            scratch.path(),
            &chunk_store,
            0,
            None,
        )
        .await
        .expect("single-layer materialize");
    let image = image_from_store(&chunk_store, out.disk_manifest).await;
    assert_eq!(read_path(&image, "/hello").as_deref(), Some(&b"world"[..]));
    // The init shim rides in every image (declared, not tree-written).
    assert!(read_path(&image, "/sbin/engram-init").is_some());
    assert!(out.manifest_digest.starts_with("sha256:"));
    assert!(
        std::fs::read_dir(scratch.path()).unwrap().next().is_none(),
        "scratch scrubbed"
    );
}
