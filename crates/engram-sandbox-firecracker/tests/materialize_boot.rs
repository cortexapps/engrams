//! ADR 0080 §C phase 3b end-to-end: `materialize_and_boot` — a tiny
//! synthetic STANDARD docker image (mock loopback registry, ~1 MB of
//! layers) goes through the REAL materializer pipeline (pull →
//! whiteout-aware flatten → stage-1 init inject → mke2fs pack → chunk
//! into a chunk store), the chunked ext4 is reassembled from the store
//! (the host's materialize-to-file path), and a real FC microVM boots
//! it — the injected shim mounts the agentd bundle slot, execs agentd
//! from tmpfs, and the test execs `/bin/true` (exit 0) plus reads a
//! layer-2 file to prove the flatten's output is what the guest sees.
//!
//! Sized to the property, not to realism: the guest userland is a
//! single static busybox + symlinks (the smallest thing that can run
//! the `/bin/sh` init shim), layers are KB-to-1MB, one boot, two execs.
//!
//! Run via:
//!
//! ```sh
//! eval "$(bash crates/engram-sandbox-firecracker/scripts/fetch-fc-test-artifacts.sh)"
//! cargo test -p engram-sandbox-firecracker --test materialize_boot -- --ignored --nocapture
//! ```

#![cfg(target_os = "linux")]

mod common;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Path as AxPath, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use bytes::Bytes;
use engram_core::traits::sandbox::SandboxBackend;
use engram_core::types::sandbox::{
    AuxRoDrive, CpuLimit, DiskLimit, ExecRequest, MemoryLimit, SandboxSpec,
};
use engram_oci::{AnonymousResolver, OciClient};
use engram_rootfs_materializer::{InitInjection, Materializer, Platform, Transport};
use engram_sandbox_firecracker::{FirecrackerBackend, FirecrackerConfig, ENGRAM_AGENTD_PORT};
use parking_lot::Mutex;
use sha2::Digest as _;

use common::{drain, fc_preflight, require_bin};

// ---- mock docker registry (pull verbs only) -----------------------------

#[derive(Clone, Default)]
struct Registry {
    blobs: Arc<Mutex<HashMap<String, Bytes>>>,
    manifests: Arc<Mutex<HashMap<String, (String, Bytes)>>>,
}

impl Registry {
    fn add_blob(&self, bytes: Vec<u8>) -> (String, u64) {
        let digest = format!("sha256:{:x}", sha2::Sha256::digest(&bytes));
        let size = bytes.len() as u64;
        self.blobs.lock().insert(digest.clone(), Bytes::from(bytes));
        (digest, size)
    }

    fn add_manifest(&self, tag: &str, bytes: Vec<u8>) {
        self.manifests.lock().insert(
            tag.to_string(),
            (
                "application/vnd.oci.image.manifest.v1+json".to_string(),
                Bytes::from(bytes),
            ),
        );
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
    Response::builder()
        .status(StatusCode::OK)
        .header(axum::http::header::CONTENT_LENGTH, body.len())
        .header(axum::http::header::CONTENT_TYPE, "application/octet-stream")
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
    let digest = format!("sha256:{:x}", sha2::Sha256::digest(&body));
    Response::builder()
        .status(StatusCode::OK)
        .header(axum::http::header::CONTENT_TYPE, content_type)
        .header("Docker-Content-Digest", digest)
        .header(axum::http::header::CONTENT_LENGTH, body.len())
        .body(axum::body::Body::from(body))
        .unwrap()
}

async fn spawn_registry() -> (SocketAddr, Registry) {
    let reg = Registry::default();
    let app = Router::new()
        .route("/v2/", get(v2_root))
        .route("/v2/:repo/blobs/:digest", get(get_blob))
        .route("/v2/:repo/manifests/:reference", get(get_manifest))
        .with_state(reg.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (addr, reg)
}

// ---- fixture layers ------------------------------------------------------

enum Entry<'a> {
    Dir {
        path: &'a str,
    },
    File {
        path: &'a str,
        body: &'a [u8],
        mode: u32,
    },
    Symlink {
        path: &'a str,
        target: &'a str,
    },
}

fn gzip_layer(entries: &[Entry<'_>]) -> Vec<u8> {
    let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    let mut tar = tar::Builder::new(gz);
    for e in entries {
        match e {
            Entry::Dir { path } => {
                let mut h = tar::Header::new_gnu();
                h.set_entry_type(tar::EntryType::Directory);
                h.set_size(0);
                h.set_mode(0o755);
                h.set_uid(0);
                h.set_gid(0);
                h.set_mtime(0);
                tar.append_data(&mut h, format!("{path}/"), std::io::empty())
                    .unwrap();
            }
            Entry::File { path, body, mode } => {
                let mut h = tar::Header::new_gnu();
                h.set_size(body.len() as u64);
                h.set_mode(*mode);
                h.set_uid(0);
                h.set_gid(0);
                h.set_mtime(0);
                tar.append_data(&mut h, path, *body).unwrap();
            }
            Entry::Symlink { path, target } => {
                let mut h = tar::Header::new_gnu();
                h.set_entry_type(tar::EntryType::Symlink);
                h.set_size(0);
                h.set_mode(0o777);
                h.set_uid(0);
                h.set_gid(0);
                h.set_mtime(0);
                tar.append_link(&mut h, path, target).unwrap();
            }
        }
    }
    tar.into_inner().unwrap().finish().unwrap()
}

/// Static busybox from the runner — the guest's whole userland. The
/// FC CI lane apt-installs `busybox-static`; dev boxes usually have
/// one of the standard paths (or set `BUSYBOX_STATIC`).
fn find_busybox() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("BUSYBOX_STATIC") {
        let p = PathBuf::from(p);
        return p.is_file().then_some(p);
    }
    ["/bin/busybox", "/usr/bin/busybox"]
        .iter()
        .map(Path::new)
        .find(|p| p.is_file())
        .map(Path::to_path_buf)
}

/// Exec `command` in the guest, polling until agentd answers (covers
/// the cold-boot window). Returns stdout; asserts exit 0.
async fn exec_ok(
    backend: &FirecrackerBackend,
    id: engram_core::SandboxId,
    command: Vec<String>,
) -> String {
    let req = ExecRequest {
        command: command.clone(),
        stdin: None,
        env: HashMap::new(),
        workdir: None,
        timeout: Some(Duration::from_secs(10)),
    };
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let stream = loop {
        match backend.exec_stream(id, req.clone()).await {
            Ok(s) => break s,
            Err(e) if tokio::time::Instant::now() < deadline => {
                let _ = e;
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
            Err(e) => panic!("agentd never answered exec {command:?}: {e:?}"),
        }
    };
    let (stdout, stderr, exit) = drain(stream.events).await;
    let stdout = String::from_utf8_lossy(&stdout).into_owned();
    assert_eq!(
        exit,
        Some(0),
        "guest cmd {command:?} failed: stdout={stdout} stderr={}",
        String::from_utf8_lossy(&stderr),
    );
    stdout
}

#[tokio::test]
#[ignore = "requires Linux + KVM + firecracker + mke2fs + mksquashfs + static busybox"]
async fn materialize_and_boot() {
    let env = match fc_preflight() {
        Some(e) => e,
        None => return,
    };
    if !require_bin("mke2fs") || !require_bin("mksquashfs") {
        return;
    }
    let Some(busybox) = find_busybox() else {
        eprintln!("SKIP: no static busybox (apt install busybox-static, or set BUSYBOX_STATIC)");
        return;
    };
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let agent = Path::new(&manifest_dir)
        .join("../../target/x86_64-unknown-linux-musl/release/engram-agentd");
    if !agent.exists() {
        eprintln!(
            "SKIP: musl engram-agentd not built at {} — \
             cargo build -p engram-agentd --target x86_64-unknown-linux-musl --release",
            agent.display(),
        );
        return;
    }

    // ---- 1. Publish the synthetic docker image ----
    // Layer 1: the busybox userland — the binary plus the applet
    // symlinks the injected stage-1 shim (and agentd's exec) resolve.
    // Layer 2: a marker file, proving multi-layer flatten output is
    // what the booted guest actually reads.
    let busybox_bytes = std::fs::read(&busybox).expect("read busybox");
    // Applet symlinks for every external command the injected shim
    // runs (echo/printf/read/[ are ash builtins) plus the exec targets.
    let applet_links = [
        "bin/sh",
        "bin/mount",
        "bin/umount",
        "bin/mkdir",
        "bin/rmdir",
        "bin/cat",
        "bin/cp",
        "bin/chmod",
        "bin/cut",
        "bin/true",
        "bin/ls",
    ];
    // The FHS skeleton every real docker base image carries (the shim
    // mounts proc/sys/dev/run over these; /root hosts the seeded
    // .bashrc; /tmp gets chmod 1777).
    let mut entries: Vec<Entry<'_>> = [
        "bin", "dev", "etc", "opt", "proc", "root", "run", "sys", "tmp",
    ]
    .iter()
    .map(|path| Entry::Dir { path })
    .collect();
    entries.push(Entry::File {
        path: "bin/busybox",
        body: &busybox_bytes,
        mode: 0o755,
    });
    for path in &applet_links {
        entries.push(Entry::Symlink {
            path,
            target: "busybox",
        });
    }
    let layer1 = gzip_layer(&entries);
    let layer2 = gzip_layer(&[Entry::File {
        path: "etc/materialize-marker",
        body: b"materialized-by-adr-0080",
        mode: 0o644,
    }]);

    let (addr, reg) = spawn_registry().await;
    let (cfg_digest, cfg_size) =
        reg.add_blob(br#"{"config":{"Env":["MAT_BOOT=1"],"WorkingDir":"/"}}"#.to_vec());
    let (l1_digest, l1_size) = reg.add_blob(layer1);
    let (l2_digest, l2_size) = reg.add_blob(layer2);
    let manifest_json = serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "config": {
            "mediaType": "application/vnd.oci.image.config.v1+json",
            "digest": cfg_digest,
            "size": cfg_size,
        },
        "layers": [
            { "mediaType": "application/vnd.oci.image.layer.v1.tar+gzip", "digest": l1_digest, "size": l1_size },
            { "mediaType": "application/vnd.oci.image.layer.v1.tar+gzip", "digest": l2_digest, "size": l2_size },
        ],
    });
    reg.add_manifest("boot-1", serde_json::to_vec(&manifest_json).unwrap());
    let uri = format!("127.0.0.1:{}/mat-boot:boot-1", addr.port());

    // ---- 2. Materialize: the REAL pipeline, real mke2fs ----
    let store_dir = tempfile::tempdir().expect("store dir");
    let chunk_store = engram_chunk_store::ChunkStore::new(Arc::new(
        engram_storage_local::LocalBlobStorage::new(store_dir.path().to_path_buf()),
    ));
    let scratch = tempfile::tempdir().expect("scratch");
    let materializer = Materializer::new(
        OciClient::new(Arc::new(AnonymousResolver)),
        InitInjection {
            vsock_port: ENGRAM_AGENTD_PORT,
            transport: Transport::Vsock,
            init_script: None,
        },
    );
    let out = materializer
        .materialize(
            &uri,
            Platform::LinuxAmd64,
            scratch.path(),
            &chunk_store,
            None,
        )
        .await
        .expect("materialize");
    assert!(out.ext4_size_bytes > 0);
    assert_eq!(
        out.oci_defaults.env.get("MAT_BOOT").map(String::as_str),
        Some("1"),
        "Dockerfile ENV must survive into oci_defaults"
    );

    // ---- 3. Reassemble the ext4 FROM THE CHUNK STORE ----
    // (the host's materialize-to-file path — proves the chunked bytes,
    // not the scratch file the guard scrubbed, are what boots.)
    let disk_manifest = chunk_store
        .get_manifest(out.disk_manifest)
        .await
        .expect("materialized manifest resolvable");
    let work = tempfile::tempdir().expect("work dir");
    let rootfs = work.path().join("rootfs.ext4");
    chunk_store
        .materialize_to_file(&disk_manifest, &rootfs)
        .await
        .expect("reassemble ext4 from chunks");

    // ---- 4. Stage the agentd bundle + boot ----
    let bundle_dir = work.path().join("bundles");
    let _bundle = common::stage_agentd_bundle(&bundle_dir, &agent);

    let mut cfg = FirecrackerConfig::with_kernel(env.kernel);
    cfg.net_pool = None; // unprivileged test — see lifecycle.rs
    cfg.bundle_dir = bundle_dir;
    cfg.default_boot_args = "console=ttyS0 reboot=k panic=1 pci=off init=/sbin/engram-init".into();
    let backend = FirecrackerBackend::new(work.path(), cfg);
    std::env::set_var("ENGRAM_FC_KEEP_JAIL_ON_FAILURE", "1");

    let spec = SandboxSpec {
        image: "materialize-boot-test".into(),
        rootfs_source: Some(rootfs),
        image_uri: None,
        rootfs_manifest: None,
        cpu: CpuLimit { vcpus: 1 },
        memory: MemoryLimit { max_mib: 256 },
        disk: DiskLimit { max_gib: 1 },
        ttl: None,
        env: HashMap::new(),
        workdir: None,
        network: Default::default(),
        // Symbolic agentd slot — resolved against the staged bundle
        // stamp, mounted + exec'd by the shim the materializer injected.
        aux_ro_drives: vec![AuxRoDrive::reserved_slot(AuxRoDrive::AGENTD_SLOT_INDEX)],
    };
    let id = backend.create(spec).await.expect("create");

    // The deliverable's property: the materialized image boots to a
    // serving agentd and can exec.
    exec_ok(&backend, id, vec!["/bin/true".into()]).await;
    // And the flatten's layered content is what the guest sees.
    let marker = exec_ok(
        &backend,
        id,
        vec![
            "/bin/sh".into(),
            "-c".into(),
            "cat /etc/materialize-marker".into(),
        ],
    )
    .await;
    assert_eq!(marker.trim(), "materialized-by-adr-0080");

    backend.destroy(id).await.expect("destroy");
}
