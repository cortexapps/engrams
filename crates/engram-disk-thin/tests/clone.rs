#![cfg(target_os = "linux")]
//! A hydrating device over the pool, as root, with a loop device as the
//! source. `ENGRAM_TEST_THIN_POOL` names the pool as in `thin_pool.rs`;
//! unset, the test skips.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use engram_disk_thin::ThinPoolDisk;
use engram_host_core::disk::{ChunkIndex, Hydration, SessionDisk, VolumeName};

/// Two pool blocks per chunk: a skip discard spans regions, and the
/// kernel copies a chunk as more than one region.
const CHUNK: u64 = 2 * 65536;
const CHUNKS: u64 = 8;
const REGIONS: u64 = CHUNKS * 2;

macro_rules! pool_or_skip {
    () => {
        match std::env::var("ENGRAM_TEST_THIN_POOL") {
            Ok(pool) => ThinPoolDisk::open(&pool, CHUNK, Path::new("/dev/engram-test"))
                .await
                .expect("open pool"),
            Err(_) => {
                eprintln!("SKIP: ENGRAM_TEST_THIN_POOL is not set");
                return;
            }
        }
    };
}

fn name(role: &str) -> VolumeName {
    VolumeName::new(format!("t-clone-{role}-{}", std::process::id())).unwrap()
}

fn fill(byte: u8) -> Vec<u8> {
    vec![byte; CHUNK as usize]
}

/// A loop device over a file, detached on drop.
struct Loop(PathBuf);

impl Loop {
    async fn over(file: &Path) -> Self {
        let output = tokio::process::Command::new("losetup")
            .args(["--find", "--show"])
            .arg(file)
            .output()
            .await
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        Self(PathBuf::from(
            String::from_utf8(output.stdout).unwrap().trim(),
        ))
    }
}

impl Drop for Loop {
    fn drop(&mut self) {
        let _ = std::process::Command::new("losetup")
            .arg("-d")
            .arg(&self.0)
            .status();
    }
}

/// The source holds chunks 0, 1 and 3; the destination's origin holds
/// chunks 0 and 1 with chunk 0 equal to the source's. Skipped: chunk 0
/// (held) and the zero chunks. The kernel copies 1 and 3; the guest
/// writes 5 through the device. After the settle the destination reads
/// as the source plus the write, and chunk 0 still shares its blocks
/// with the origin.
#[tokio::test]
#[ignore]
async fn a_hydrating_device_copies_only_the_delta_and_settles_onto_its_destination() {
    let pool = pool_or_skip!();
    let base = pool.create(&name("base"), CHUNKS * CHUNK).await.unwrap();
    base.write_chunk(ChunkIndex(0), fill(1)).await.unwrap();
    base.write_chunk(ChunkIndex(1), fill(2)).await.unwrap();
    let dest = pool.snapshot(&base, &name("l")).await.unwrap();

    let dir = tempfile::tempdir().unwrap();
    let source_file = dir.path().join("source");
    let mut source = vec![0u8; (CHUNKS * CHUNK) as usize];
    for (chunk, byte) in [(0, 1u8), (1, 3), (3, 4)] {
        let at = (chunk * CHUNK) as usize;
        source[at..at + CHUNK as usize].fill(byte);
    }
    std::fs::write(&source_file, &source).unwrap();
    let source = Loop::over(&source_file).await;

    let skip: Vec<ChunkIndex> = [0, 2, 4, 5, 6, 7].map(ChunkIndex).to_vec();
    let device = pool
        .hydrate(&dest, &source.0, &name("io"), &skip)
        .await
        .unwrap();
    let listed = pool.list().await.unwrap();
    let metadata = VolumeName::new(format!("{}-md", name("io"))).unwrap();
    assert!(
        listed.contains(&name("io")) && listed.contains(&metadata),
        "the device and its metadata thin are volumes of the pool: {listed:?}"
    );
    assert_eq!(
        pool.open(&name("io")).await.unwrap().size_bytes,
        device.size_bytes
    );

    assert_eq!(device.read_chunk(ChunkIndex(0)).await.unwrap(), fill(1));
    assert_eq!(device.read_chunk(ChunkIndex(1)).await.unwrap(), fill(3));
    assert_eq!(device.read_chunk(ChunkIndex(2)).await.unwrap(), fill(0));
    assert_eq!(device.read_chunk(ChunkIndex(3)).await.unwrap(), fill(4));
    device.write_chunk(ChunkIndex(5), fill(5)).await.unwrap();

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match pool.hydration(&device.name).await.unwrap() {
            Hydration::Hydrating {
                hydrated,
                total,
                source: from,
            } if hydrated == total => {
                assert_eq!((total, from), (REGIONS, source.0.clone()));
                break;
            }
            Hydration::Hydrating { .. } if Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            state => panic!("hydration did not finish: {state:?}"),
        }
    }
    // A snapshot of the hydrating device is a snapshot of its
    // destination, under the device's suspend; it diffs against the base.
    let mid = pool.snapshot(&device, &name("mid")).await.unwrap();
    assert_eq!(
        pool.dirty_blocks(&base, &mid, CHUNK).await.unwrap(),
        vec![1, 3, 5]
    );
    assert_eq!(mid.read_chunk(ChunkIndex(5)).await.unwrap(), fill(5));
    let mid_name = mid.name.clone();
    drop(mid);
    pool.delete(&mid_name).await.unwrap();

    pool.settle(&device.name, &dest).await.unwrap();
    assert_eq!(
        pool.hydration(&device.name).await.unwrap(),
        Hydration::Settled
    );
    let after = pool.snapshot(&device, &name("after")).await.unwrap();
    assert_eq!(after.read_chunk(ChunkIndex(3)).await.unwrap(), fill(4));
    let after_name = after.name.clone();
    drop(after);
    pool.delete(&after_name).await.unwrap();
    assert!(
        !pool.list().await.unwrap().contains(&metadata),
        "the settle deleted the metadata thin"
    );

    assert_eq!(device.read_chunk(ChunkIndex(1)).await.unwrap(), fill(3));
    assert_eq!(device.read_chunk(ChunkIndex(5)).await.unwrap(), fill(5));
    for (chunk, byte) in [(0, 1u8), (1, 3), (2, 0), (3, 4), (5, 5)] {
        assert_eq!(
            dest.read_chunk(ChunkIndex(chunk)).await.unwrap(),
            fill(byte),
            "chunk {chunk}"
        );
    }
    assert_eq!(
        pool.dirty_blocks(&base, &dest, CHUNK).await.unwrap(),
        vec![1, 3, 5],
        "the skipped chunk 0 still shares its blocks with the origin"
    );

    // A volume holds its device open; a delete needs it closed.
    let names = (device.name.clone(), dest.name.clone(), base.name.clone());
    drop((device, dest, base));
    let (device, dest, base) = names;
    pool.delete(&device).await.unwrap();
    pool.delete(&dest).await.unwrap();
    pool.delete(&base).await.unwrap();
    assert!(!pool
        .list()
        .await
        .unwrap()
        .iter()
        .any(|v| v.as_str().starts_with("t-clone-")));
}

/// A settle before the copy is done is refused and changes nothing.
#[tokio::test]
#[ignore]
async fn settle_refuses_a_device_that_is_still_hydrating() {
    let pool = pool_or_skip!();
    let dest = pool.create(&name("early-l"), CHUNKS * CHUNK).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let source_file = dir.path().join("source");
    std::fs::write(&source_file, vec![7u8; (CHUNKS * CHUNK) as usize]).unwrap();
    let source = Loop::over(&source_file).await;
    // A suspended destination takes no write, so the copy cannot finish.
    dmsetup(&["suspend", &pool.device_name(&dest.name)]).await;
    let device = pool
        .hydrate(&dest, &source.0, &name("early-io"), &[])
        .await
        .unwrap();
    let err = pool.settle(&device.name, &dest).await.unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput, "{err}");
    assert!(matches!(
        pool.hydration(&device.name).await.unwrap(),
        Hydration::Hydrating { .. }
    ));
    dmsetup(&["resume", &pool.device_name(&dest.name)]).await;
    // A delete while the kernel is still copying leaks nothing.
    let names = (device.name.clone(), dest.name.clone());
    drop((device, dest));
    let (device, dest) = names;
    pool.delete(&device).await.unwrap();
    pool.delete(&dest).await.unwrap();
    assert!(!pool
        .list()
        .await
        .unwrap()
        .iter()
        .any(|v| v.as_str().starts_with("t-clone-early")));
}

async fn dmsetup(args: &[&str]) {
    let status = tokio::process::Command::new("dmsetup")
        .args(args)
        .status()
        .await
        .unwrap();
    assert!(status.success(), "dmsetup {args:?}");
}
