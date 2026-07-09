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
    apply_layer, emit_tar, pull_image, recommended_size, recursive_size, Ext4Packer, InitInjection,
    LayerCompression, Materializer, Mke2fsPacker, Platform, Transport, TreeMetadata,
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

const SCRATCHPAD_MKE2FS: &str = "/private/tmp/claude-501/-Users-ganeshdatta-Documents-engrams/8c393984-8b5a-4ad7-9428-47048ade71fd/scratchpad/mke2fs-libarchive/bin/mke2fs";

fn find_on_path(bin: &str) -> Option<std::path::PathBuf> {
    std::env::var_os("PATH")
        .into_iter()
        .flat_map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
        .map(|dir| dir.join(bin))
        .find(|p| p.is_file())
}

fn e2fsprogs_pair() -> Option<(std::path::PathBuf, std::path::PathBuf)> {
    let scratchpad = std::path::PathBuf::from(SCRATCHPAD_MKE2FS);
    let mke2fs = if scratchpad.is_file() {
        scratchpad
    } else if let Some(p) = std::env::var_os("ENGRAM_MKE2FS").map(std::path::PathBuf::from) {
        p
    } else {
        find_on_path("mke2fs")?
    };

    let debugfs = if let Some(p) = std::env::var_os("ENGRAM_DEBUGFS").map(std::path::PathBuf::from)
    {
        p
    } else if let Some(sibling) = mke2fs.parent().map(|d| d.join("debugfs")) {
        if sibling.is_file() {
            sibling
        } else {
            find_on_path("debugfs")?
        }
    } else {
        find_on_path("debugfs")?
    };

    (mke2fs.is_file() && debugfs.is_file()).then_some((mke2fs, debugfs))
}

/// ADR 0036 byte-determinism hinges on SOURCE_DATE_EPOCH, honored by
/// e2fsprogs >= 1.47.1, and ADR 0084 additionally requires libarchive
/// tar input support. Older/missing/non-libarchive mke2fs → skip with
/// a note, never flake.
fn deterministic_mke2fs_available() -> Option<(std::path::PathBuf, String)> {
    let (mke2fs, _debugfs) = e2fsprogs_pair()?;
    let out = std::process::Command::new(&mke2fs)
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
    if !matches!(ver, Some(v) if v >= (1, 47, 1)) {
        return None;
    }
    mke2fs_accepts_tar(&mke2fs).ok()?;
    Some((mke2fs, text))
}

fn materializer(mke2fs: impl Into<std::path::PathBuf>) -> Materializer {
    Materializer::with_packer(
        oci_client(),
        InitInjection {
            vsock_port: 1024,
            transport: Transport::Vsock,
            init_script: None,
        },
        Arc::new(Mke2fsPacker::with_binary(mke2fs)),
    )
}

fn mke2fs_accepts_tar(mke2fs: &std::path::Path) -> Result<(), String> {
    let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
    let tar_path = dir.path().join("smoke.tar");
    {
        let file = std::fs::File::create(&tar_path).map_err(|e| e.to_string())?;
        let mut tar = tar::Builder::new(file);
        let mut h = tar::Header::new_gnu();
        h.set_mode(0o644);
        h.set_uid(0);
        h.set_gid(0);
        h.set_size(2);
        h.set_mtime(946_684_800);
        h.set_entry_type(tar::EntryType::Regular);
        tar.append_data(&mut h, "ok", &b"ok"[..])
            .map_err(|e| e.to_string())?;
        tar.finish().map_err(|e| e.to_string())?;
    }
    let image = dir.path().join("smoke.ext4");
    let f = std::fs::File::create(&image).map_err(|e| e.to_string())?;
    f.set_len(16 * 1024 * 1024).map_err(|e| e.to_string())?;
    drop(f);

    let out = std::process::Command::new(mke2fs)
        .arg("-t")
        .arg("ext4")
        .arg("-F")
        .arg("-q")
        .arg("-d")
        .arg(&tar_path)
        .arg(&image)
        .output()
        .map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).into_owned())
    }
}

fn debugfs_cmd(debugfs: &std::path::Path, image: &std::path::Path, cmd: &str) -> String {
    let out = std::process::Command::new(debugfs)
        .arg("-R")
        .arg(cmd)
        .arg(image)
        .output()
        .expect("run debugfs");
    assert!(
        out.status.success(),
        "debugfs {cmd:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn debugfs_inode(stat: &str) -> u64 {
    stat.split_whitespace()
        .collect::<Vec<_>>()
        .windows(2)
        .find_map(|w| (w[0] == "Inode:").then(|| w[1].parse().ok()).flatten())
        .unwrap_or_else(|| panic!("debugfs stat missing inode: {stat}"))
}

fn metadata_layer() -> Vec<u8> {
    let mut tar = tar::Builder::new(Vec::new());
    let mut file = tar::Header::new_gnu();
    file.set_mode(0o4755);
    file.set_uid(0);
    file.set_gid(0);
    file.set_size(10);
    file.set_mtime(946_684_800);
    file.set_entry_type(tar::EntryType::Regular);
    let cap = [
        0x01, 0x00, 0x00, 0x02, // revision 2 + effective flag
        0x00, 0x00, 0x20, 0x00, // permitted: cap_sys_admin
        0x00, 0x00, 0x00, 0x00, // inheritable
        0x00, 0x00, 0x00, 0x00, // permitted high
        0x00, 0x00, 0x00, 0x00, // inheritable high
    ];
    tar.append_pax_extensions([
        ("SCHILY.xattr.security.capability", &cap[..]),
        ("SCHILY.xattr.user.test", &b"dropped-by-mke2fs"[..]),
    ])
    .unwrap();
    tar.append_data(&mut file, "usr/bin/mount", &b"fake mount"[..])
        .unwrap();

    let mut hardlink = tar::Header::new_gnu();
    hardlink.set_mode(0o4755);
    hardlink.set_uid(0);
    hardlink.set_gid(0);
    hardlink.set_size(0);
    hardlink.set_mtime(946_684_800);
    hardlink.set_entry_type(tar::EntryType::Link);
    tar.append_link(&mut hardlink, "usr/bin/mount.link", "usr/bin/mount")
        .unwrap();

    let mut symlink = tar::Header::new_gnu();
    symlink.set_mode(0o777);
    symlink.set_uid(0);
    symlink.set_gid(0);
    symlink.set_size(0);
    symlink.set_mtime(946_684_800);
    symlink.set_entry_type(tar::EntryType::Symlink);
    tar.append_link(&mut symlink, "etc/mtab", "../proc/self/mounts")
        .unwrap();

    let mut fifo = tar::Header::new_gnu();
    fifo.set_mode(0o644);
    fifo.set_uid(0);
    fifo.set_gid(0);
    fifo.set_size(0);
    fifo.set_mtime(946_684_800);
    fifo.set_entry_type(tar::EntryType::Fifo);
    tar.append_data(&mut fifo, "run/queue.pipe", std::io::empty())
        .unwrap();

    tar.finish().unwrap();
    tar.into_inner().unwrap()
}

#[tokio::test]
async fn real_mke2fs_tar_input_preserves_materialized_metadata() {
    let Some((mke2fs, debugfs)) = e2fsprogs_pair() else {
        eprintln!("SKIP: mke2fs/debugfs pair not available");
        return;
    };
    if let Err(e) = mke2fs_accepts_tar(&mke2fs) {
        eprintln!(
            "SKIP: mke2fs does not support tar input (likely built without libarchive): {}",
            e.trim()
        );
        return;
    }

    let root = tempfile::tempdir().unwrap();
    let mut meta = TreeMetadata::default();
    apply_layer(root.path(), &mut meta, metadata_layer().as_slice()).unwrap();

    let image_dir = tempfile::tempdir().unwrap();
    let size = recommended_size(recursive_size(root.path()).await.unwrap());
    let tar_path = image_dir.path().join("rootfs.tar");
    emit_tar(root.path(), &meta, &tar_path).unwrap();
    let image = image_dir.path().join("rootfs.ext4");
    Mke2fsPacker::with_binary(&mke2fs)
        .pack(&tar_path, &image, size)
        .await
        .unwrap();

    let mount_stat = debugfs_cmd(&debugfs, &image, "stat /usr/bin/mount");
    assert!(mount_stat.contains("Type: regular"), "{mount_stat}");
    assert!(mount_stat.contains("Mode:  04755"), "{mount_stat}");
    assert!(mount_stat.contains("User:     0"), "{mount_stat}");
    assert!(mount_stat.contains("Group:     0"), "{mount_stat}");

    let link_stat = debugfs_cmd(&debugfs, &image, "stat /usr/bin/mount.link");
    assert_eq!(
        debugfs_inode(&mount_stat),
        debugfs_inode(&link_stat),
        "hardlink entries must share an ext4 inode"
    );

    let symlink_stat = debugfs_cmd(&debugfs, &image, "stat /etc/mtab");
    assert!(symlink_stat.contains("Type: symlink"), "{symlink_stat}");
    assert!(
        symlink_stat.contains("../proc/self/mounts"),
        "{symlink_stat}"
    );

    let fifo_stat = debugfs_cmd(&debugfs, &image, "stat /run/queue.pipe");
    assert!(
        fifo_stat.to_ascii_lowercase().contains("type: fifo"),
        "{fifo_stat}"
    );

    let xattrs = debugfs_cmd(&debugfs, &image, "ea_list /usr/bin/mount");
    assert!(
        xattrs.contains("security.capability"),
        "security.capability should round-trip through mke2fs/libarchive: {xattrs}"
    );
    assert!(
        !xattrs.contains("user.test"),
        "mke2fs/libarchive 1.47.2 drops user.* xattrs; test pins that behavior: {xattrs}"
    );
}

/// The keystone: two FULL runs (pull → flatten → inject → pack →
/// chunk) over the same fixtures produce the SAME content-derived
/// manifest ref — what keeps enable-time snapshot reuse working — and
/// every run scrubs its scratch, success or not.
#[tokio::test]
async fn materialize_is_deterministic_and_scrubs_scratch() {
    let Some((mke2fs, ver)) = deterministic_mke2fs_available() else {
        eprintln!(
            "mke2fs missing, < 1.47.1, or lacks tar input support; skipping determinism test \
             (use the libarchive-enabled flake-pinned mke2fs)"
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
    let m = materializer(mke2fs);

    // Phase 3b: the RPC's progress frames — assert the honest stage
    // sequence rides the optional sender.
    let (ptx, mut prx) = tokio::sync::mpsc::channel(32);
    let first = m
        .materialize(
            &fx.uri,
            Platform::LinuxArm64,
            scratch.path(),
            &chunk_store,
            Some(ptx),
        )
        .await
        .expect("first materialize");
    let mut stages = Vec::new();
    while let Ok(p) = prx.try_recv() {
        stages.push(p.stage);
    }
    use engram_core::types::MaterializeStage as S;
    assert_eq!(
        stages,
        vec![S::Pull, S::Flatten, S::Pack, S::Chunk],
        "one frame per stage transition, in pipeline order"
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

    let err = materializer("mke2fs")
        .materialize(
            &fx.uri,
            Platform::LinuxArm64,
            scratch.path(),
            &chunk_store,
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
