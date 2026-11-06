//! `ProcessSources` against the kernel's ublk driver, as root: a source
//! killed while a read waits on its device is recovered by `ensure_alive`,
//! the read completes with the manifest's bytes, and a stop leaves no
//! process and no device; the startup sweep deletes a device that no
//! record names and no process serves.
//!
//! `ENGRAM_TEST_UBLK=1` and `ENGRAM_DISK_SOURCE_BIN` (a built
//! `engram-disk-source`) run it; unset, it skips. The driver must be loaded.
#![cfg(target_os = "linux")]

use std::fs::OpenOptions;
use std::os::unix::fs::{FileExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use engram_chunk_store::{ChunkRef, ChunkSize, ChunkStore, Manifest, ManifestKind, ManifestRef};
use engram_core::types::ids::SandboxId;
use engram_core::types::snapshot::DiskRole;
use engram_host_agent::disk_source::{KernelProbe, Probe, ProcessSources, ProcessSourcesConfig};
use engram_host_core::DiskSources;
use engram_storage_local::LocalBlobStorage;
use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;

const CHUNK: u64 = 1 << 20;
const CHUNKS: u64 = 4;
const READ: usize = 64 * 1024;

fn chunk_bytes(seed: u8) -> Vec<u8> {
    (0..CHUNK as usize)
        .map(|i| ((i as u32).wrapping_mul(2_654_435_761) >> (seed % 24)) as u8 ^ seed)
        .collect()
}

/// Publish a disk of `CHUNKS` distinct chunks to a local store.
async fn publish(store_root: &Path) -> (ManifestRef, Vec<u8>) {
    let store = ChunkStore::new(Arc::new(LocalBlobStorage::new(store_root.join("blobs"))));
    let mut expected = Vec::new();
    let mut chunks = Vec::new();
    for index in 0..CHUNKS {
        let bytes = chunk_bytes(index as u8 + 1);
        let hash = store.put_chunk(&bytes).await.unwrap();
        chunks.push(ChunkRef {
            offset: index * CHUNK,
            hash,
        });
        expected.extend_from_slice(&bytes);
    }
    let mut manifest = Manifest::empty(ManifestKind::Disk, CHUNKS * CHUNK);
    manifest.chunk_size = ChunkSize::bytes(CHUNK);
    manifest.chunks = chunks;
    let manifest_ref = ManifestRef::new();
    store.put_manifest(manifest_ref, &manifest).await.unwrap();
    (manifest_ref, expected)
}

/// One direct read, so the page cache does not serve it.
fn read_direct(path: &Path, offset: u64) -> std::io::Result<Vec<u8>> {
    let device = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECT)
        .open(path)?;
    let mut buf = vec![0u8; READ + 4096];
    let start = buf.as_ptr().align_offset(4096);
    device.read_exact_at(&mut buf[start..start + READ], offset)?;
    Ok(buf[start..start + READ].to_vec())
}

/// Stops the source if the test fails first: a device left without a
/// process holds the blocked read, and the test process cannot exit.
struct StopOnDrop<'a> {
    rt: &'a tokio::runtime::Runtime,
    sources: &'a ProcessSources,
    sandbox: SandboxId,
}

impl Drop for StopOnDrop<'_> {
    fn drop(&mut self) {
        if let Err(e) = self
            .rt
            .block_on(self.sources.stop(self.sandbox, DiskRole::Root))
        {
            eprintln!("stop the source: {e}");
        }
    }
}

struct Rig {
    work: tempfile::TempDir,
    rt: tokio::runtime::Runtime,
    cfg: ProcessSourcesConfig,
    manifest: ManifestRef,
    expected: Vec<u8>,
}

/// A published disk and a source config, or `None` when the lane's
/// variables are not set.
fn rig() -> Option<Rig> {
    if std::env::var_os("ENGRAM_TEST_UBLK").is_none() {
        eprintln!("SKIP: ENGRAM_TEST_UBLK is not set");
        return None;
    }
    let Some(bin) = std::env::var_os("ENGRAM_DISK_SOURCE_BIN").map(PathBuf::from) else {
        eprintln!("SKIP: ENGRAM_DISK_SOURCE_BIN is not set");
        return None;
    };
    let work = tempfile::tempdir().unwrap();
    // The source reads the blob store from the environment it inherits.
    // nextest runs each test in a process of its own, before any other
    // thread starts.
    std::env::set_var("ENGRAM_BLOB_BACKEND", "local");
    std::env::set_var("ENGRAM_LOCAL_PATH", work.path());
    let rt = tokio::runtime::Runtime::new().unwrap();
    let (manifest, expected) = rt.block_on(publish(work.path()));
    let mut cfg = ProcessSourcesConfig::new(bin, work.path());
    cfg.queues = 1;
    cfg.depth = 8;
    Some(Rig {
        work,
        rt,
        cfg,
        manifest,
        expected,
    })
}

/// A host-agent that died before it recorded a source leaves a device
/// that no record names. When its server has died too, the startup sweep
/// deletes it.
#[test]
#[ignore = "requires root and the ublk driver"]
fn the_sweep_deletes_a_device_no_record_names() {
    let Some(r) = rig() else { return };
    let sources = ProcessSources::new(r.cfg.clone());
    let sandbox = SandboxId::new();
    r.rt.block_on(sources.start(sandbox, DiskRole::Root, r.manifest, None))
        .unwrap();
    let rec = r.rt.block_on(sources.records()).pop().unwrap();
    let device = rec.device.unwrap();
    kill(Pid::from_raw(rec.pid as i32), Signal::SIGKILL).unwrap();
    let probe = KernelProbe;
    while probe.start_time(rec.pid) == Some(rec.start_time) {
        std::thread::sleep(Duration::from_millis(10));
    }
    std::fs::remove_file(
        r.work
            .path()
            .join("disk-sources")
            .join(format!("{sandbox}-root.json")),
    )
    .unwrap();

    // The kernel quiesces the device once it sees the server's queues go.
    for _ in 0..100 {
        r.rt.block_on(sources.sweep_unrecorded());
        if !probe.device_exists(device.id) {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        !probe.device_exists(device.id),
        "the sweep deleted the device"
    );
}

#[test]
#[ignore = "requires root and the ublk driver"]
fn a_killed_source_is_recovered_under_a_waiting_read_and_a_stop_leaves_nothing() {
    let Some(Rig {
        work: _work,
        rt,
        cfg,
        manifest,
        expected,
    }) = rig()
    else {
        return;
    };
    let sources = ProcessSources::new(cfg);
    let sandbox = SandboxId::new();
    let path = rt
        .block_on(sources.start(sandbox, DiskRole::Root, manifest, None))
        .unwrap();
    let _stop = StopOnDrop {
        rt: &rt,
        sources: &sources,
        sandbox,
    };
    assert_eq!(read_direct(&path, 0).unwrap(), expected[..READ]);

    let first = rt.block_on(sources.records()).pop().unwrap();
    let device = first.device.clone().unwrap();
    kill(Pid::from_raw(first.pid as i32), Signal::SIGKILL).unwrap();
    let probe = KernelProbe;
    while probe.start_time(first.pid) == Some(first.start_time) {
        std::thread::sleep(Duration::from_millis(10));
    }
    let offset = 2 * CHUNK + 4096;
    let reader = {
        let path = path.clone();
        std::thread::spawn(move || read_direct(&path, offset))
    };
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        !reader.is_finished(),
        "the read waits while no process serves"
    );

    rt.block_on(sources.ensure_alive(sandbox, DiskRole::Root))
        .unwrap();
    let bytes = reader
        .join()
        .unwrap()
        .expect("the read completes after the recovery");
    assert_eq!(bytes, expected[offset as usize..offset as usize + READ]);
    let second = rt.block_on(sources.records()).pop().unwrap();
    assert_ne!(second.pid, first.pid);
    assert_eq!(second.device, Some(device.clone()));

    rt.block_on(sources.stop(sandbox, DiskRole::Root)).unwrap();
    assert_eq!(probe.start_time(second.pid), None, "the process exited");
    assert!(!probe.device_exists(device.id), "the device is deleted");
    assert!(!PathBuf::from(format!("/sys/block/ublkb{}", device.id)).exists());
    assert!(rt.block_on(sources.records()).is_empty());
}
