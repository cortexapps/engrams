//! The ublk source under a real kernel, as root, with `ublk_drv` loaded.
//! `ENGRAM_TEST_UBLK` set runs the tests (the CI root lane); unset, they
//! skip.
// tests drive a live system; wall clock/OS entropy here is input, not a decision source (ADR 0098 D1)
#![allow(clippy::disallowed_methods)]
#![cfg(target_os = "linux")]

use std::fs::File;
use std::os::unix::fs::{FileExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use engram_chunk_store::{ChunkRef, ChunkSize, ChunkStore, Manifest, ManifestKind};
use engram_fill::FillEngine;
use engram_host_agent::blk_source::BlkSource;
use engram_storage_local::LocalBlobStorage;
use libublk::helpers::IoBuf;

const CHUNK: u64 = 256 << 10;
const REGION: u64 = 64 << 10;
const CHUNKS: u64 = 4;
/// Chunks 0, 1 and 3 hold bytes; chunk 2 is a hole.
const HELD: [(u64, u8); 3] = [(0, 0x11), (1, 0x22), (3, 0x33)];
const DELTA: [u64; 3] = [0, 1, 3];

fn gate() -> bool {
    if std::env::var_os("ENGRAM_TEST_UBLK").is_none() {
        eprintln!("SKIP: ENGRAM_TEST_UBLK is not set");
        return false;
    }
    true
}

fn image() -> Vec<u8> {
    let mut image = vec![0u8; (CHUNKS * CHUNK) as usize];
    for (chunk, byte) in HELD {
        let at = (chunk * CHUNK) as usize;
        image[at..at + CHUNK as usize].fill(byte);
    }
    image
}

/// A store holding the image's chunks under `dir`, and its manifest.
async fn engine_and_manifest(dir: &Path) -> (FillEngine, Manifest) {
    let store = ChunkStore::new(Arc::new(LocalBlobStorage::new(dir.join("blobs"))));
    let mut chunks = Vec::new();
    for (chunk, byte) in HELD {
        let hash = store.put_chunk(&vec![byte; CHUNK as usize]).await.unwrap();
        chunks.push(ChunkRef {
            offset: chunk * CHUNK,
            hash,
        });
    }
    let manifest = Manifest {
        chunk_size: ChunkSize::bytes(CHUNK),
        chunks,
        ..Manifest::empty(ManifestKind::Disk, CHUNKS * CHUNK)
    };
    (FillEngine::new(store, Default::default()), manifest)
}

/// The device opened for direct I/O, so every read reaches the source
/// with the offset and length the test gives it.
fn open_direct(path: &Path) -> File {
    File::options()
        .read(true)
        .custom_flags(libc::O_DIRECT)
        .open(path)
        .unwrap()
}

fn read_at(device: &File, offset: u64, len: usize) -> Vec<u8> {
    let mut buf = IoBuf::<u8>::new(len);
    device.read_exact_at(buf.as_mut_slice(), offset).unwrap();
    buf.as_slice().to_vec()
}

#[tokio::test]
#[ignore = "requires root and the ublk_drv module"]
async fn a_source_serves_the_manifest_and_records_the_chunks_read_cold() {
    if !gate() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let (engine, manifest) = engine_and_manifest(dir.path()).await;
    let source = BlkSource::serve(engine, &manifest, DELTA.to_vec(), REGION)
        .await
        .unwrap();
    let path = source.path().to_path_buf();
    let device = open_direct(&path);
    let image = image();

    // A guest read far from where the copy starts: served, and recorded.
    let at = 3 * CHUNK + 8192;
    assert_eq!(
        read_at(&device, at, 4096),
        image[at as usize..at as usize + 4096]
    );
    assert_eq!(source.touched(), vec![3]);

    // The copy's stream: the whole device, region by region, ascending.
    for at in (0..CHUNKS * CHUNK).step_by(REGION as usize) {
        assert_eq!(
            read_at(&device, at, REGION as usize),
            image[at as usize..(at + REGION) as usize],
            "region at {at}"
        );
    }
    assert_eq!(source.touched(), vec![3], "the stream is not a guest read");

    drop(device);
    source.stop().await.unwrap();
    assert!(!path.exists(), "{} was deleted", path.display());
}

/// The server half of the recovery test: serves a source from the
/// directory `ENGRAM_UBLK_CHILD_DIR` names until it is killed.
#[tokio::test]
#[ignore = "a helper the recovery test runs as a child process"]
async fn child_serves_until_killed() {
    let Some(dir) = std::env::var_os("ENGRAM_UBLK_CHILD_DIR") else {
        return;
    };
    let dir = PathBuf::from(dir);
    let store = ChunkStore::new(Arc::new(LocalBlobStorage::new(dir.join("blobs"))));
    let manifest: Manifest =
        serde_json::from_slice(&std::fs::read(dir.join("manifest.json")).unwrap()).unwrap();
    let source = BlkSource::serve(
        FillEngine::new(store, Default::default()),
        &manifest,
        DELTA.to_vec(),
        REGION,
    )
    .await
    .unwrap();
    std::fs::write(dir.join("dev_id"), source.dev_id().to_string()).unwrap();
    std::future::pending::<()>().await;
}

/// A host-agent killed with a read queued at its source: the device
/// stays, the read waits, and the next host-agent's server answers it.
#[tokio::test]
#[ignore = "requires root and the ublk_drv module"]
async fn a_source_killed_with_a_queued_read_is_recovered_by_a_new_server() {
    if !gate() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let (engine, manifest) = engine_and_manifest(dir.path()).await;
    std::fs::write(
        dir.path().join("manifest.json"),
        serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "child_serves_until_killed",
            "--exact",
            "--ignored",
            "--nocapture",
        ])
        .env("ENGRAM_UBLK_CHILD_DIR", dir.path())
        .spawn()
        .unwrap();
    let dev_id_file = dir.path().join("dev_id");
    let deadline = Instant::now() + Duration::from_secs(20);
    while !dev_id_file.exists() {
        assert!(
            Instant::now() < deadline,
            "the child did not serve a device"
        );
        assert!(child.try_wait().unwrap().is_none(), "the child exited");
        std::thread::sleep(Duration::from_millis(50));
    }
    let dev_id: u32 = std::fs::read_to_string(&dev_id_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let path = PathBuf::from(format!("/dev/ublkb{dev_id}"));
    let image = image();
    assert_eq!(
        read_at(&open_direct(&path), 0, REGION as usize),
        image[..REGION as usize]
    );

    // Freeze the server, queue a read it cannot answer, then kill it.
    let pid = nix::unistd::Pid::from_raw(child.id() as i32);
    nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGSTOP).unwrap();
    let at = 3 * CHUNK + REGION;
    let queued = std::thread::spawn({
        let path = path.clone();
        move || read_at(&open_direct(&path), at, REGION as usize)
    });
    std::thread::sleep(Duration::from_millis(300));
    assert!(!queued.is_finished(), "the read waits on the frozen server");
    nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGKILL).unwrap();
    child.wait().unwrap();

    let recovered = BlkSource::recover(dev_id, engine, &manifest, DELTA.to_vec(), REGION)
        .await
        .unwrap();
    let bytes = tokio::time::timeout(
        Duration::from_secs(10),
        tokio::task::spawn_blocking(move || queued.join().unwrap()),
    )
    .await
    .expect("the queued read completes after recovery")
    .unwrap();
    assert_eq!(bytes, image[at as usize..(at + REGION) as usize]);

    recovered.stop().await.unwrap();
    assert!(!path.exists(), "{} was deleted", path.display());
}
