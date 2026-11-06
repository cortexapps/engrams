#![cfg(target_os = "linux")]
//! The `engram-disk-source` binary against the kernel's ublk driver, as
//! root. `ENGRAM_TEST_UBLK=1` runs them (the driver must be loaded:
//! `modprobe ublk_drv`); unset, every test skips.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader};
use std::os::unix::fs::{FileExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use engram_chunk_store::{
    ChunkHash, ChunkRef, ChunkSize, ChunkStore, Manifest, ManifestKind, ManifestRef,
};
use engram_storage_local::LocalBlobStorage;
use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;

const CHUNK: u64 = 1 << 20;
/// Chunk seeds; `None` is a hole.
const LAYOUT: [Option<u8>; 6] = [Some(1), None, Some(2), Some(3), None, Some(4)];
const READ: usize = 64 * 1024;
const READY_TIMEOUT: Duration = Duration::from_secs(60);

macro_rules! ublk_or_skip {
    () => {
        if std::env::var_os("ENGRAM_TEST_UBLK").is_none() {
            eprintln!("SKIP: ENGRAM_TEST_UBLK is not set");
            return;
        }
    };
}

struct Disk {
    _dir: tempfile::TempDir,
    store_root: PathBuf,
    manifest: ManifestRef,
    expected: Vec<u8>,
}

fn chunk_bytes(seed: u8) -> Vec<u8> {
    (0..CHUNK as usize)
        .map(|i| ((i as u32).wrapping_mul(2_654_435_761) >> (seed % 24)) as u8 ^ seed)
        .collect()
}

/// A manifest of `LAYOUT` published to a local store.
fn publish() -> Disk {
    let dir = tempfile::tempdir().unwrap();
    let store_root = dir.path().to_path_buf();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let total = LAYOUT.len() as u64 * CHUNK;
    let store = ChunkStore::new(Arc::new(LocalBlobStorage::new(store_root.join("blobs"))));
    let mut expected = vec![0u8; total as usize];
    let mut chunks = Vec::new();
    for (index, seed) in LAYOUT.iter().enumerate() {
        let Some(seed) = seed else { continue };
        let offset = index as u64 * CHUNK;
        let bytes = chunk_bytes(*seed);
        let hash = rt.block_on(store.put_chunk(&bytes)).unwrap();
        expected[offset as usize..(offset + CHUNK) as usize].copy_from_slice(&bytes);
        chunks.push(ChunkRef { offset, hash });
    }
    let mut manifest = Manifest::empty(ManifestKind::Disk, total);
    manifest.chunk_size = ChunkSize::bytes(CHUNK);
    manifest.chunks = chunks;
    let manifest_ref = ManifestRef::new();
    rt.block_on(store.put_manifest(manifest_ref, &manifest))
        .unwrap();
    Disk {
        _dir: dir,
        store_root,
        manifest: manifest_ref,
        expected,
    }
}

/// A running binary. Dropped while it runs (a failed assertion), it is
/// stopped so the device does not outlive the test.
struct Source {
    child: Option<Child>,
    path: PathBuf,
    dev_id: u32,
}

/// Start the binary and wait for its ready line.
fn start(disk: &Disk, recover: Option<u32>) -> Source {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_engram-disk-source"));
    cmd.args([
        "serve",
        "--manifest",
        &disk.manifest.to_string(),
        "--queues",
        "2",
        "--depth",
        "16",
    ])
    .env("ENGRAM_BLOB_BACKEND", "local")
    .env("ENGRAM_LOCAL_PATH", &disk.store_root)
    .stdout(Stdio::piped())
    .stderr(Stdio::inherit());
    if let Some(id) = recover {
        cmd.args(["--recover", &id.to_string()]);
    }
    let mut child = cmd.spawn().expect("spawn engram-disk-source");
    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        let _ = BufReader::new(stdout).read_line(&mut line);
        let _ = tx.send(line);
    });
    let line = rx
        .recv_timeout(READY_TIMEOUT)
        .expect("engram-disk-source printed no ready line");
    let (path, id) = line
        .trim()
        .split_once(' ')
        .unwrap_or_else(|| panic!("ready line {line:?} is not \"<path> <id>\""));
    Source {
        child: Some(child),
        path: PathBuf::from(path),
        dev_id: id.parse().unwrap(),
    }
}

impl Source {
    /// SIGTERM: a normal stop, which deletes the device.
    fn stop(mut self) {
        let mut child = self.child.take().unwrap();
        kill(Pid::from_raw(child.id() as i32), Signal::SIGTERM).unwrap();
        let status = child.wait().unwrap();
        assert!(status.success(), "a stopped source exits cleanly: {status}");
        let sysfs = PathBuf::from(format!("/sys/block/ublkb{}", self.dev_id));
        assert!(!sysfs.exists(), "a normal stop deletes the device");
    }

    /// SIGKILL: the device stays for recovery.
    fn crash(mut self) -> u32 {
        let mut child = self.child.take().unwrap();
        child.kill().unwrap();
        child.wait().unwrap();
        self.dev_id
    }
}

impl Drop for Source {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = kill(Pid::from_raw(child.id() as i32), Signal::SIGTERM);
            let _ = child.wait();
        }
    }
}

/// A buffer at an address aligned for direct I/O.
struct Aligned {
    buf: Vec<u8>,
    start: usize,
    len: usize,
}

impl Aligned {
    fn new(len: usize) -> Self {
        let buf = vec![0u8; len + 4096];
        let start = buf.as_ptr().align_offset(4096);
        Self { buf, start, len }
    }
    fn as_mut(&mut self) -> &mut [u8] {
        &mut self.buf[self.start..self.start + self.len]
    }
}

fn open_direct(path: &Path) -> File {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECT)
        .open(path)
        .unwrap()
}

#[test]
#[ignore]
fn the_device_reads_as_the_manifest_bytes() {
    ublk_or_skip!();
    let disk = publish();
    let source = start(&disk, None);
    let size = std::fs::read_to_string(format!("/sys/block/ublkb{}/size", source.dev_id)).unwrap();
    assert_eq!(
        size.trim().parse::<u64>().unwrap() * 512,
        disk.expected.len() as u64
    );

    let device = open_direct(&source.path);
    let mut buf = Aligned::new(READ);
    for offset in (0..disk.expected.len()).step_by(READ) {
        device.read_exact_at(buf.as_mut(), offset as u64).unwrap();
        assert!(
            buf.as_mut() == &disk.expected[offset..offset + READ],
            "bytes at {offset} differ from the manifest"
        );
    }
    drop(device);
    source.stop();
}

#[test]
#[ignore]
fn writes_are_refused() {
    ublk_or_skip!();
    let disk = publish();
    let source = start(&disk, None);
    let ro = std::fs::read_to_string(format!("/sys/block/ublkb{}/ro", source.dev_id)).unwrap();
    assert_eq!(ro.trim(), "1", "the kernel marks the device read-only");
    // Root may open a read-only device for writing; the write itself fails.
    let refused = match OpenOptions::new().write(true).open(&source.path) {
        Err(e) => e,
        Ok(device) => device
            .write_at(&[0u8; 4096], 0)
            .expect_err("a write to a read-only device fails"),
    };
    assert!(
        matches!(
            refused.raw_os_error(),
            Some(libc::EROFS) | Some(libc::EACCES) | Some(libc::EPERM)
        ),
        "unexpected error {refused}"
    );
    source.stop();
}

#[test]
#[ignore]
fn reads_stall_across_a_crash_and_complete_after_recovery() {
    ublk_or_skip!();
    let disk = Arc::new(publish());
    let source = start(&disk, None);
    let path = source.path.clone();

    let reading = Arc::new(AtomicBool::new(true));
    let reader = {
        let disk = Arc::clone(&disk);
        let reading = Arc::clone(&reading);
        std::thread::spawn(move || {
            let device = open_direct(&path);
            let mut buf = Aligned::new(READ);
            let mut spans = Vec::new();
            let mut offset = 0usize;
            while reading.load(Ordering::SeqCst) {
                let began = Instant::now();
                // A crash must stall this read, never fail it.
                device.read_exact_at(buf.as_mut(), offset as u64).unwrap();
                assert!(
                    buf.as_mut() == &disk.expected[offset..offset + READ],
                    "bytes at {offset} differ from the manifest"
                );
                spans.push((began, Instant::now()));
                offset = (offset + READ) % disk.expected.len();
            }
            spans
        })
    };

    std::thread::sleep(Duration::from_millis(200));
    let crashed_at = Instant::now();
    let dev_id = source.crash();
    // The kernel holds the device's I/O while no process serves it.
    std::thread::sleep(Duration::from_millis(500));
    let recovered = start(&disk, Some(dev_id));
    let recovered_at = Instant::now();
    assert_eq!(recovered.dev_id, dev_id);
    assert_eq!(recovered.path, source_path(dev_id));
    std::thread::sleep(Duration::from_millis(200));
    reading.store(false, Ordering::SeqCst);
    let spans = reader.join().expect("no read failed");

    assert!(
        spans.iter().any(
            |(began, ended)| *began < crashed_at + Duration::from_millis(100)
                && *ended > recovered_at - Duration::from_millis(100)
        ),
        "a read issued around the crash waits for the recovered process"
    );
    assert!(
        spans.iter().any(|(began, _)| *began > recovered_at),
        "reads continue after recovery"
    );
    recovered.stop();
}

#[test]
#[ignore]
fn a_stop_ends_a_read_that_cannot_be_served() {
    ublk_or_skip!();
    let disk = publish();
    // Chunk 3 is gone from the store, so a read of it tries again forever.
    let lost = ChunkHash::of(&chunk_bytes(3));
    std::fs::remove_file(disk.store_root.join("blobs").join(lost.storage_key())).unwrap();
    let source = start(&disk, None);
    let path = source.path.clone();
    let reader = std::thread::spawn(move || {
        let mut buf = Aligned::new(READ);
        open_direct(&path).read_exact_at(buf.as_mut(), 3 * CHUNK)
    });
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        !reader.is_finished(),
        "the read waits while the store cannot serve it"
    );
    // The kernel stops a device only once its requests complete; the stop
    // must end the waiting read, or it never finishes.
    source.stop();
    let err = reader
        .join()
        .unwrap()
        .expect_err("a read ended by a stop fails");
    assert_eq!(
        err.raw_os_error(),
        Some(libc::EIO),
        "unexpected error {err}"
    );
}

fn source_path(dev_id: u32) -> PathBuf {
    PathBuf::from(format!("/dev/ublkb{dev_id}"))
}
