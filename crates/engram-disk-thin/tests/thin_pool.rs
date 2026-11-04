#![cfg(target_os = "linux")]
//! Against a real dm-thin pool, as root. `ENGRAM_TEST_THIN_POOL` names
//! the pool (`deploy/helm/engram-host-fleet/files/thin-pool.sh` builds
//! one); unset, every test skips.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use engram_chunk_store::ChunkStore;
use engram_disk_thin::ThinPoolDisk;
use engram_host_core::disk::{Capacity, ChunkIndex, SessionDisk, Volume, VolumeName};
use engram_host_core::{checkpoint, stage};
use engram_storage_local::LocalBlobStorage;

const CHUNK: u64 = 65536;
const CHUNKS: u64 = 8;

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

/// Names are unique per test so a failed run leaves nothing a later run
/// trips over.
fn name(test: &str, role: &str) -> VolumeName {
    VolumeName::new(format!("t-{test}-{role}-{}", std::process::id())).unwrap()
}

fn fill(byte: u8) -> Vec<u8> {
    vec![byte; CHUNK as usize]
}

async fn thin_id(volume: &Volume) -> u32 {
    let output = tokio::process::Command::new("dmsetup")
        .args(["table", volume.name.as_str()])
        .output()
        .await
        .unwrap();
    let text = String::from_utf8(output.stdout).unwrap();
    text.split_whitespace().nth(4).unwrap().parse().unwrap()
}

/// The trait's default full compare over the same pool: the oracle the
/// `thin_delta` override is held to.
struct Oracle<'a>(&'a ThinPoolDisk);

#[async_trait]
impl SessionDisk for Oracle<'_> {
    async fn create(&self, name: &VolumeName, size_bytes: u64) -> std::io::Result<Volume> {
        self.0.create(name, size_bytes).await
    }
    async fn snapshot(&self, origin: &Volume, name: &VolumeName) -> std::io::Result<Volume> {
        self.0.snapshot(origin, name).await
    }
    async fn open(&self, name: &VolumeName) -> std::io::Result<Volume> {
        self.0.open(name).await
    }
    async fn delete(&self, name: &VolumeName) -> std::io::Result<()> {
        self.0.delete(name).await
    }
    async fn capacity(&self) -> std::io::Result<Capacity> {
        self.0.capacity().await
    }
}

#[tokio::test]
#[ignore]
async fn a_volume_survives_a_dropped_handle_and_opens_by_name() {
    let disk = pool_or_skip!();
    let live = name("reopen", "live");
    let v = disk.create(&live, CHUNK * CHUNKS).await.unwrap();
    assert_eq!(v.read_chunk(ChunkIndex(5)).await.unwrap(), fill(0));
    v.write_chunk(ChunkIndex(5), fill(7)).await.unwrap();
    v.sync().await.unwrap();
    drop(v);

    let again = disk.open(&live).await.unwrap();
    assert_eq!(again.size_bytes, CHUNK * CHUNKS);
    assert_eq!(again.read_chunk(ChunkIndex(5)).await.unwrap(), fill(7));
    drop(again);
    disk.delete(&live).await.unwrap();
    assert_eq!(
        disk.open(&live).await.unwrap_err().kind(),
        std::io::ErrorKind::NotFound
    );
}

#[tokio::test]
#[ignore]
async fn delete_fails_while_a_handle_is_open() {
    let disk = pool_or_skip!();
    let live = name("busy", "live");
    let v = disk.create(&live, CHUNK * CHUNKS).await.unwrap();
    assert!(disk.delete(&live).await.is_err());
    drop(v);
    disk.delete(&live).await.unwrap();
}

#[tokio::test]
#[ignore]
async fn thin_delta_reports_exactly_what_the_full_compare_reports() {
    let disk = pool_or_skip!();
    let (live, s1, s2) = (
        name("delta", "live"),
        name("delta", "s1"),
        name("delta", "s2"),
    );
    let v = disk.create(&live, CHUNK * CHUNKS).await.unwrap();
    for i in [2, 4, 5] {
        v.write_chunk(ChunkIndex(i), fill(i as u8)).await.unwrap();
    }
    v.sync().await.unwrap();
    let before = disk.snapshot(&v, &s1).await.unwrap();

    v.write_chunk(ChunkIndex(4), fill(44)).await.unwrap();
    v.write_chunk(ChunkIndex(7), fill(77)).await.unwrap();
    v.zero_chunk(ChunkIndex(2)).await.unwrap();
    v.sync().await.unwrap();
    let after = disk.snapshot(&v, &s2).await.unwrap();

    let fast = disk.dirty_chunks(&before, &after).await.unwrap();
    let oracle = Oracle(&disk).dirty_chunks(&before, &after).await.unwrap();
    assert_eq!(fast, oracle);
    assert_eq!(fast, vec![ChunkIndex(2), ChunkIndex(4), ChunkIndex(7)]);
    assert_eq!(after.read_chunk(ChunkIndex(2)).await.unwrap(), fill(0));

    drop((v, before, after));
    for n in [&s2, &s1, &live] {
        disk.delete(n).await.unwrap();
    }
}

#[tokio::test]
#[ignore]
async fn a_checkpoint_over_the_pool_reproduces_through_stage() {
    let disk = pool_or_skip!();
    let dir = tempfile::tempdir().unwrap();
    let store = ChunkStore::new(Arc::new(LocalBlobStorage::new(dir.path().join("blobs"))));
    let (live, c1, staged) = (
        name("flow", "live"),
        name("flow", "c1"),
        name("flow", "staged"),
    );

    let v = disk.create(&live, CHUNK * CHUNKS).await.unwrap();
    v.write_chunk(ChunkIndex(1), fill(1)).await.unwrap();
    v.write_chunk(ChunkIndex(6), fill(6)).await.unwrap();
    let (snap, manifest) = checkpoint(&disk, &store, &v, &c1, None).await.unwrap();
    assert_eq!(
        manifest.chunks.iter().map(|c| c.offset).collect::<Vec<_>>(),
        vec![CHUNK, 6 * CHUNK]
    );

    let copy = stage(&disk, &store, &staged, &manifest, None)
        .await
        .unwrap();
    for i in 0..CHUNKS {
        assert_eq!(
            copy.read_chunk(ChunkIndex(i)).await.unwrap(),
            v.read_chunk(ChunkIndex(i)).await.unwrap(),
            "chunk {i}"
        );
    }

    drop((v, snap, copy));
    for n in [&staged, &c1, &live] {
        disk.delete(n).await.unwrap();
    }
}

#[tokio::test]
#[ignore]
async fn capacity_falls_by_the_blocks_written() {
    let disk = pool_or_skip!();
    let live = name("capacity", "live");
    let v = disk.create(&live, CHUNK * CHUNKS).await.unwrap();
    let before = disk.capacity().await.unwrap();
    for i in [0, 3, 7] {
        v.write_chunk(ChunkIndex(i), fill(9)).await.unwrap();
    }
    v.sync().await.unwrap();
    let after = disk.capacity().await.unwrap();
    assert_eq!(before.data_free_bytes - after.data_free_bytes, 3 * CHUNK);
    assert_eq!(before.data_total_bytes, after.data_total_bytes);
    drop(v);
    disk.delete(&live).await.unwrap();
}

#[tokio::test]
#[ignore]
async fn open_deletes_a_thin_whose_device_is_gone_and_create_skips_a_taken_id() {
    let disk = pool_or_skip!();
    let orphan = name("reap", "orphan");
    let v = disk.create(&orphan, CHUNK).await.unwrap();
    let orphan_id = thin_id(&v).await;
    drop(v);
    // A crash between `dmsetup remove` and the pool's `delete` message.
    let removed = tokio::process::Command::new("dmsetup")
        .args(["remove", orphan.as_str()])
        .status()
        .await
        .unwrap();
    assert!(removed.success());
    assert!(disk.thin_ids().await.unwrap().contains(&orphan_id));

    let disk = ThinPoolDisk::open(disk.pool(), CHUNK, Path::new("/dev/engram-test"))
        .await
        .unwrap();
    assert!(!disk.thin_ids().await.unwrap().contains(&orphan_id));

    // Another writer takes the id the counter will pick next.
    let taken = disk
        .thin_ids()
        .await
        .unwrap()
        .into_iter()
        .max()
        .unwrap_or(0)
        + 1;
    let created = tokio::process::Command::new("dmsetup")
        .args(["message", disk.pool(), "0", &format!("create_thin {taken}")])
        .status()
        .await
        .unwrap();
    assert!(created.success());
    let live = name("reap", "live");
    let v = disk.create(&live, CHUNK).await.unwrap();
    assert_eq!(thin_id(&v).await, taken + 1);

    drop(v);
    disk.delete(&live).await.unwrap();
    let deleted = tokio::process::Command::new("dmsetup")
        .args(["message", disk.pool(), "0", &format!("delete {taken}")])
        .status()
        .await
        .unwrap();
    assert!(deleted.success());
}
