//! A lazy resume over the dm-thin pool with a ublk source, as root: the
//! device the guest would open answers a cold read with the manifest's
//! bytes while the kernel copies, the volume settles onto its
//! destination, and a process that dies mid-copy leaves a volume the
//! next process must serve again before it can delete it. Needs
//! `ENGRAM_TEST_THIN_POOL` and `ENGRAM_TEST_UBLK` (the CI root lane);
//! unset, the tests skip.
// tests drive a live system; wall clock/OS entropy here is input, not a decision source (ADR 0098 D1)
#![allow(clippy::disallowed_methods)]
#![cfg(target_os = "linux")]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use engram_chunk_store::{ChunkRef, ChunkSize, ChunkStore, Manifest, ManifestKind};
use engram_core::types::manifest::ManifestRef;
use engram_core::types::snapshot::DiskRole;
use engram_core::SandboxId;
use engram_disk_thin::ThinPoolDisk;
use engram_fill::FillEngine;
use engram_host_agent::blk_source::UblkSources;
use engram_host_core::disk::ChunkIndex;
use engram_host_core::session_volumes::{live_name, Source, StagePath};
use engram_host_core::{Hydration, SessionVolumes};
use engram_storage_local::LocalBlobStorage;

/// Two pool blocks per chunk, as in the disk-thin clone test.
const CHUNK: u64 = 2 * 65536;
/// Chunk `i` holds byte `PATTERN[i]`; zero is a hole.
const PATTERN: [u8; 8] = [1, 2, 0, 4, 0, 0, 7, 8];
const SETTLE_DEADLINE: Duration = Duration::from_secs(30);

fn gate() -> Option<String> {
    let pool = std::env::var("ENGRAM_TEST_THIN_POOL").ok();
    if pool.is_none() || std::env::var_os("ENGRAM_TEST_UBLK").is_none() {
        eprintln!("SKIP: ENGRAM_TEST_THIN_POOL or ENGRAM_TEST_UBLK is not set");
        return None;
    }
    pool
}

fn fill(byte: u8) -> Vec<u8> {
    vec![byte; CHUNK as usize]
}

fn store(dir: &Path) -> ChunkStore {
    ChunkStore::new(Arc::new(LocalBlobStorage::new(dir.join("blobs"))))
}

/// Session volumes over the pool, with the host's ublk sources.
async fn volumes(pool: &str, store: ChunkStore) -> SessionVolumes {
    let disk = ThinPoolDisk::open(pool, CHUNK, Path::new("/dev/engram-test"))
        .await
        .expect("open pool");
    SessionVolumes::new(Arc::new(disk), FillEngine::new(store, Default::default()))
        .with_sources(Arc::new(UblkSources))
}

/// `PATTERN` as a manifest in `store`.
async fn manifest_in(store: &ChunkStore) -> ManifestRef {
    let mut chunks = Vec::new();
    for (i, byte) in PATTERN.iter().enumerate() {
        if *byte != 0 {
            chunks.push(ChunkRef {
                offset: i as u64 * CHUNK,
                hash: store.put_chunk(&fill(*byte)).await.unwrap(),
            });
        }
    }
    let manifest = Manifest {
        chunk_size: ChunkSize::bytes(CHUNK),
        chunks,
        ..Manifest::empty(ManifestKind::Disk, PATTERN.len() as u64 * CHUNK)
    };
    let manifest_ref = ManifestRef::new();
    store.put_manifest(manifest_ref, &manifest).await.unwrap();
    manifest_ref
}

async fn settled(volumes: &SessionVolumes, sandbox: SandboxId) {
    let deadline = Instant::now() + SETTLE_DEADLINE;
    loop {
        match volumes.settle(sandbox, DiskRole::Root).await.unwrap() {
            Hydration::Settled => return,
            Hydration::Hydrating {
                hydrated, total, ..
            } => {
                assert!(
                    Instant::now() < deadline,
                    "the volume did not settle: {hydrated} of {total} regions"
                );
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
    }
}

async fn assert_disk(volumes: &SessionVolumes, sandbox: SandboxId, expected: &[u8]) {
    let live = volumes.live(sandbox, DiskRole::Root).await.unwrap();
    for (i, byte) in expected.iter().enumerate() {
        assert_eq!(
            live.read_chunk(ChunkIndex(i as u64)).await.unwrap(),
            fill(*byte),
            "chunk {i}"
        );
    }
}

async fn dmsetup(args: &[&str]) {
    let status = tokio::process::Command::new("dmsetup")
        .args(args)
        .status()
        .await
        .unwrap();
    assert!(status.success(), "dmsetup {args:?}");
}

/// The device name of the sandbox's live root volume in the pool.
fn live_device(pool: &str, sandbox: SandboxId) -> String {
    format!("{pool}.{}", live_name(sandbox, DiskRole::Root))
}

#[tokio::test]
#[ignore = "requires root, a dm-thin pool and the ublk_drv module"]
async fn a_lazy_resume_reads_cold_chunks_through_the_source_and_settles() {
    let Some(pool) = gate() else { return };
    let dir = tempfile::tempdir().unwrap();
    let store = store(dir.path());
    let manifest = manifest_in(&store).await;
    let volumes = volumes(&pool, store).await;
    let sandbox = SandboxId::new();

    let provisioned = volumes
        .provision(
            sandbox,
            Source::Continue {
                manifest,
                base: None,
            },
            None,
        )
        .await
        .unwrap();
    assert_eq!(provisioned.root_stage, StagePath::Lazy);
    assert_eq!(volumes.sources().len(), 1, "one source serves the root");

    // The last chunk is the last the ascending copy reaches: read now, it
    // comes through the source. A write to a hole lands on the device.
    let live = volumes.live(sandbox, DiskRole::Root).await.unwrap();
    assert_eq!(live.read_chunk(ChunkIndex(7)).await.unwrap(), fill(8));
    live.write_chunk(ChunkIndex(2), fill(9)).await.unwrap();
    drop(live);

    settled(&volumes, sandbox).await;
    assert!(volumes.hydrating(sandbox).await.unwrap().is_empty());
    assert!(volumes.sources().is_empty(), "the source stopped");
    assert_disk(&volumes, sandbox, &[1, 2, 9, 4, 0, 0, 7, 8]).await;

    volumes.release(sandbox).await.unwrap();
    let prefix = format!("sb-{sandbox}-");
    assert!(!volumes
        .disk()
        .list()
        .await
        .unwrap()
        .iter()
        .any(|n| n.as_str().starts_with(&prefix)));
}

/// The first half of the recovery test, in a child process: provisions
/// the sandbox lazily, suspends its live device so the copy halts, and
/// waits to be killed.
#[tokio::test]
#[ignore = "a helper the recovery test runs as a child process"]
async fn child_provisions_and_waits() {
    let Some(dir) = std::env::var_os("ENGRAM_LAZY_CHILD_DIR") else {
        return;
    };
    let dir = PathBuf::from(dir);
    let pool = std::env::var("ENGRAM_TEST_THIN_POOL").unwrap();
    let store = store(&dir);
    let manifest: ManifestRef =
        serde_json::from_slice(&std::fs::read(dir.join("manifest.json")).unwrap()).unwrap();
    let volumes = volumes(&pool, store).await;
    let sandbox = SandboxId::new();
    let provisioned = volumes
        .provision(
            sandbox,
            Source::Continue {
                manifest,
                base: None,
            },
            None,
        )
        .await
        .unwrap();
    assert_eq!(provisioned.root_stage, StagePath::Lazy);
    dmsetup(&["suspend", &live_device(&pool, sandbox)]).await;
    std::fs::write(dir.join("sandbox"), sandbox.to_string()).unwrap();
    std::future::pending::<()>().await;
}

/// A host-agent killed mid-copy: the next host-agent cannot delete the
/// hydrating volume until it serves the source again, then settles it.
#[tokio::test]
#[ignore = "requires root, a dm-thin pool and the ublk_drv module"]
async fn a_killed_process_leaves_a_volume_the_next_one_serves_again() {
    let Some(pool) = gate() else { return };
    let dir = tempfile::tempdir().unwrap();
    let store = store(dir.path());
    let manifest = manifest_in(&store).await;
    std::fs::write(
        dir.path().join("manifest.json"),
        serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "child_provisions_and_waits",
            "--exact",
            "--ignored",
            "--nocapture",
        ])
        .env("ENGRAM_LAZY_CHILD_DIR", dir.path())
        .spawn()
        .unwrap();
    let sandbox_file = dir.path().join("sandbox");
    let deadline = Instant::now() + Duration::from_secs(30);
    while !sandbox_file.exists() {
        assert!(Instant::now() < deadline, "the child did not provision");
        assert!(child.try_wait().unwrap().is_none(), "the child exited");
        std::thread::sleep(Duration::from_millis(50));
    }
    let sandbox: SandboxId = std::fs::read_to_string(&sandbox_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(child.id() as i32),
        nix::sys::signal::Signal::SIGKILL,
    )
    .unwrap();
    child.wait().unwrap();

    let volumes = volumes(&pool, store).await;
    let refused = volumes.release(sandbox).await.unwrap_err();
    assert_eq!(
        refused.kind(),
        std::io::ErrorKind::ResourceBusy,
        "{refused}"
    );
    assert_eq!(
        volumes.reattach_all().await.unwrap(),
        vec![(sandbox, DiskRole::Root)]
    );
    assert!(volumes.reattach_all().await.unwrap().is_empty());
    dmsetup(&["resume", &live_device(&pool, sandbox)]).await;

    settled(&volumes, sandbox).await;
    assert_disk(&volumes, sandbox, &PATTERN).await;
    assert!(volumes.sources().is_empty());
    volumes.release(sandbox).await.unwrap();
}
