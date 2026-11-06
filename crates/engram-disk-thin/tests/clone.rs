#![cfg(target_os = "linux")]
//! Cloned drives against a real dm-thin pool, as root.
//! `ENGRAM_TEST_THIN_POOL` names the pool; unset, every test skips. The
//! source is a read-only loop device over a file, behind dm-delay where
//! a test needs the background copy to take time.

use std::collections::HashMap;
use std::io::ErrorKind;
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use engram_chunk_store::{ChunkStore, ManifestRef};
use engram_core::types::ids::SandboxId;
use engram_core::types::snapshot::DiskRole;
use engram_disk_thin::ThinPoolDisk;
use engram_host_core::disk::{
    clone_bitmap_bytes, CloneState, Hydration, SessionDisk, Volume, VolumeName, CLONE_REGION_BYTES,
};
use engram_host_core::disk_sources::Served;
use engram_host_core::image_volumes;
use engram_host_core::session_volumes::{live_name, Cutover, Source, Waiting};
use engram_host_core::{checkpoint, stage, DiskSources, FileDisk, FileSources, SessionVolumes};
use engram_storage_local::LocalBlobStorage;

const CHUNK: u64 = 65536;
const REGION: u64 = CLONE_REGION_BYTES;
const REGIONS: u64 = 64;
const SIZE: u64 = REGION * REGIONS;
const HYDRATION_DEADLINE: Duration = Duration::from_secs(60);
const NODES: &str = "/dev/engram-test";

/// A pool, a source device of `SIZE` bytes, and the names one test uses.
/// Dropping it removes every device and node the test left, so a failed
/// test leaves none behind; a thin it leaves is deleted by the next
/// `ThinPoolDisk::open`.
struct Rig {
    disk: ThinPoolDisk,
    test: &'static str,
    source: PathBuf,
    bytes: Vec<u8>,
    loop_dev: String,
    _dir: tempfile::TempDir,
}

impl Rig {
    /// `None`, with a `SKIP:` line, when no pool is named.
    async fn new(test: &'static str, read_delay_ms: Option<u32>) -> Option<Self> {
        let Ok(pool) = std::env::var("ENGRAM_TEST_THIN_POOL") else {
            eprintln!("SKIP: ENGRAM_TEST_THIN_POOL is not set");
            return None;
        };
        let disk = ThinPoolDisk::open(&pool, CHUNK, Path::new(NODES))
            .await
            .expect("open pool");
        let dir = tempfile::tempdir().unwrap();
        let bytes = pattern(SIZE as usize, 0x9E37_79B9);
        let file = dir.path().join("source.img");
        std::fs::write(&file, &bytes).unwrap();
        let loop_dev = run(
            "losetup",
            &["--read-only", "--show", "-f", file.to_str().unwrap()],
        );
        let mut rig = Self {
            source: PathBuf::from(&loop_dev),
            disk,
            test,
            bytes,
            loop_dev,
            _dir: dir,
        };
        if let Some(ms) = read_delay_ms {
            let delay = rig.device(&rig.name("delay"));
            let table = format!("0 {} delay {} 0 {ms}", SIZE / 512, rig.loop_dev);
            run(
                "dmsetup",
                &["create", "--noudevsync", &delay, "--table", &table],
            );
            run("dmsetup", &["mknodes", &delay]);
            rig.source = PathBuf::from("/dev/mapper").join(&delay);
        }
        Some(rig)
    }

    fn name(&self, role: &str) -> VolumeName {
        VolumeName::new(format!("t-{}-{role}-{}", self.test, std::process::id())).unwrap()
    }

    fn device(&self, name: &VolumeName) -> String {
        self.disk.device_name(name)
    }

    /// Every device-mapper device this test made that still exists.
    fn leftovers(&self) -> Vec<String> {
        let suffix = format!("-{}", std::process::id());
        let prefix = format!("{}.t-{}-", self.disk.pool(), self.test);
        run("dmsetup", &["ls"])
            .lines()
            .filter_map(|line| line.split_whitespace().next())
            .filter(|name| name.starts_with(&prefix) && name.ends_with(&suffix))
            .map(str::to_string)
            .collect()
    }

    /// [`Self::leftovers`] the disk made: all but the source's delay.
    fn disk_leftovers(&self) -> Vec<String> {
        let delay = self.device(&self.name("delay"));
        self.leftovers()
            .into_iter()
            .filter(|device| *device != delay)
            .collect()
    }

    async fn volume(&self, role: &str, size: u64) -> Volume {
        self.disk.create(&self.name(role), size).await.unwrap()
    }

    async fn clone_drive(&self, dest: &Volume, bitmap: &Volume, hydration: Hydration) -> Volume {
        self.disk
            .clone_drive(&self.name("drive"), dest, bitmap, &self.source, hydration)
            .await
            .unwrap()
    }

    async fn state(&self, role: &str) -> CloneState {
        self.disk.clone_state(&self.name(role)).await.unwrap()
    }

    /// Poll until the drive has copied `at_least` regions.
    async fn hydrated(&self, at_least: u64) -> u64 {
        let deadline = tokio::time::Instant::now() + HYDRATION_DEADLINE;
        loop {
            match self.state("drive").await {
                CloneState::Copying {
                    hydrated_regions, ..
                } if hydrated_regions >= at_least => return hydrated_regions,
                CloneState::Copying { .. } => {}
                state => panic!("the drive is {state:?}"),
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "fewer than {at_least} regions copied in {HYDRATION_DEADLINE:?}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        // A drive or view holds its thins, and every device holds the
        // source, so remove in passes until nothing is left. A device a
        // failed test left suspended is resumed first, so no I/O waits on
        // it.
        for _ in 0..4 {
            for device in self.leftovers() {
                for args in [vec!["resume"], vec!["remove", "--noudevsync"]] {
                    let _ = Command::new("dmsetup").args(args).arg(&device).output();
                }
                let _ = std::fs::remove_file(Path::new(NODES).join(&device));
            }
        }
        let _ = Command::new("losetup")
            .args(["-d", &self.loop_dev])
            .output();
    }
}

fn run(program: &str, args: &[&str]) -> String {
    let output = Command::new(program).args(args).output().unwrap();
    assert!(
        output.status.success(),
        "{program} {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

/// `len` bytes of xorshift output: no region repeats another or is zero.
fn pattern(len: usize, mut state: u32) -> Vec<u8> {
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            state as u8
        })
        .collect()
}

fn off(threshold_regions: u32, batch_regions: u32) -> Hydration {
    Hydration {
        enabled: false,
        threshold_regions,
        batch_regions,
    }
}

async fn write(volume: &Volume, expected: &mut [u8], offset: u64, bytes: Vec<u8>) {
    expected[offset as usize..offset as usize + bytes.len()].copy_from_slice(&bytes);
    volume.write_range(offset, bytes).await.unwrap();
}

async fn whole(volume: &Volume) -> Vec<u8> {
    volume.read_range(0, volume.size_bytes).await.unwrap()
}

/// `major:minor` of a volume's device node.
fn devno(volume: &Volume) -> String {
    let rdev = std::fs::metadata(&volume.path).unwrap().rdev();
    format!("{}:{}", libc::major(rdev), libc::minor(rdev))
}

/// The first region at which two images differ, for a short message.
fn first_difference(got: &[u8], want: &[u8]) -> Option<u64> {
    got.iter()
        .zip(want)
        .position(|(g, w)| g != w)
        .map(|byte| byte as u64 / REGION)
}

#[tokio::test]
#[ignore]
async fn a_clone_reads_the_source_and_cuts_over_with_a_handle_open() {
    let Some(rig) = Rig::new("cutover", None).await else {
        return;
    };
    let dest = rig.volume("dest", SIZE).await;
    let bitmap = rig.volume("bitmap", clone_bitmap_bytes(SIZE)).await;
    let drive = rig.clone_drive(&dest, &bitmap, off(4, 4)).await;
    assert_eq!(
        drive.path,
        Path::new(NODES).join(rig.device(&rig.name("drive")))
    );
    assert!(!rig.disk.list().await.unwrap().contains(&rig.name("drive")));

    // Reads of uncopied regions come from the source and copy nothing.
    let mut expected = rig.bytes.clone();
    assert_eq!(first_difference(&whole(&drive).await, &expected), None);
    assert_eq!(
        rig.state("drive").await,
        CloneState::Copying {
            hydrated_regions: 0,
            total_regions: REGIONS,
            hydration_enabled: false
        }
    );

    // A partial write copies its region first; a whole-region write does
    // not need to. Both land.
    write(&drive, &mut expected, 3 * REGION + 4096, vec![0xA5; 4096]).await;
    write(
        &drive,
        &mut expected,
        5 * REGION,
        pattern(REGION as usize, 7),
    )
    .await;
    assert_eq!(first_difference(&whole(&drive).await, &expected), None);
    assert_eq!(rig.hydrated(2).await, 2);
    assert_eq!(
        rig.disk
            .cut_over(&rig.name("drive"))
            .await
            .unwrap_err()
            .kind(),
        ErrorKind::ResourceBusy
    );

    rig.disk.enable_hydration(&rig.name("drive")).await.unwrap();
    rig.hydrated(REGIONS).await;
    assert!(matches!(
        rig.state("drive").await,
        CloneState::Copying {
            hydration_enabled: true,
            ..
        }
    ));

    let held = std::fs::File::open(&drive.path).unwrap();
    let rdev = held.metadata().unwrap().rdev();
    rig.disk.cut_over(&rig.name("drive")).await.unwrap();
    rig.disk.cut_over(&rig.name("drive")).await.unwrap();
    assert_eq!(rig.state("drive").await, CloneState::CutOver);
    let table = run("dmsetup", &["table", &rig.device(&rig.name("drive"))]);
    assert_eq!(table.split_whitespace().nth(2), Some("linear"), "{table}");
    assert_eq!(std::fs::metadata(&drive.path).unwrap().rdev(), rdev);

    // The bitmap is free, and both handles opened before the cutover work.
    drop(bitmap);
    rig.disk.delete(&rig.name("bitmap")).await.unwrap();
    write(&drive, &mut expected, 9 * REGION, vec![0x5A; 8192]).await;
    drive.sync().await.unwrap();
    let mut through_held = vec![0u8; 8192];
    held.read_exact_at(&mut through_held, 9 * REGION).unwrap();
    assert_eq!(through_held, vec![0x5A; 8192]);
    assert_eq!(first_difference(&whole(&drive).await, &expected), None);
    assert_eq!(first_difference(&whole(&dest).await, &expected), None);
    let reopened = rig.disk.open_clone(&rig.name("drive")).await.unwrap();
    assert_eq!(reopened.path, drive.path);

    drop((held, drive, reopened));
    rig.disk.remove_clone(&rig.name("drive")).await.unwrap();
    assert_eq!(first_difference(&whole(&dest).await, &expected), None);
    drop(dest);
    rig.disk.delete(&rig.name("dest")).await.unwrap();
    assert_eq!(rig.disk_leftovers(), Vec::<String>::new());
}

/// The checkpoint view: snapshots of the destination and the bitmap,
/// taken while the copy runs and after guest writes, composed again with
/// the source read exactly what the guest saw. The drive keeps copying
/// and cuts over unharmed.
#[tokio::test]
#[ignore]
async fn a_view_over_snapshots_reads_what_the_guest_saw_mid_hydration() {
    let Some(rig) = Rig::new("view", Some(30)).await else {
        return;
    };
    let dest = rig.volume("dest", SIZE).await;
    let bitmap = rig.volume("bitmap", clone_bitmap_bytes(SIZE)).await;
    let drive = rig.clone_drive(&dest, &bitmap, off(1, 1)).await;
    let mut expected = rig.bytes.clone();
    write(&drive, &mut expected, 3 * REGION + 512, vec![0x11; 4096]).await;
    write(
        &drive,
        &mut expected,
        5 * REGION,
        pattern(REGION as usize, 5),
    )
    .await;
    drive.sync().await.unwrap();
    rig.disk.enable_hydration(&rig.name("drive")).await.unwrap();
    rig.hydrated(8).await;

    let (dest_snap, bitmap_snap) = rig
        .disk
        .snapshot_clone(
            &rig.name("drive"),
            &dest,
            &bitmap,
            &rig.name("dest-snap"),
            &rig.name("bitmap-snap"),
        )
        .await
        .unwrap();
    let seen = expected.clone();
    // Writes after the snapshot, to a copied and an uncopied region.
    write(&drive, &mut expected, REGION, vec![0x22; REGION as usize]).await;
    write(&drive, &mut expected, 60 * REGION + 4096, vec![0x33; 4096]).await;
    drive.sync().await.unwrap();

    let view = rig
        .disk
        .clone_view(&rig.name("view"), &dest_snap, &bitmap_snap, &rig.source)
        .await
        .unwrap();
    let CloneState::Copying {
        hydrated_regions: at_snapshot,
        hydration_enabled: false,
        ..
    } = rig.state("view").await
    else {
        panic!("a view is a clone with hydration off");
    };
    eprintln!("{at_snapshot} of {REGIONS} regions copied at the snapshot");
    assert!(
        (8..REGIONS).contains(&at_snapshot),
        "{at_snapshot} regions copied at the snapshot"
    );
    // The destination snapshot alone holds zeros where nothing was
    // copied; the view reads the source there.
    assert_eq!(
        dest_snap.read_range(63 * REGION, REGION).await.unwrap(),
        vec![0; REGION as usize]
    );
    assert_eq!(first_difference(&whole(&view).await, &seen), None);
    assert!(matches!(
        rig.state("view").await,
        CloneState::Copying { hydrated_regions, .. } if hydrated_regions == at_snapshot
    ));
    assert!(view.write_range(0, vec![0xFF; 4096]).await.is_err());
    let reopened = rig.disk.open_clone(&rig.name("view")).await.unwrap();
    assert!(reopened.write_range(0, vec![0xFF; 4096]).await.is_err());
    let refused = [
        rig.disk.cut_over(&rig.name("view")).await,
        rig.disk.enable_hydration(&rig.name("view")).await,
        rig.disk.rename(&rig.name("view"), &rig.name("moved")).await,
        rig.disk
            .snapshot_clone(
                &rig.name("view"),
                &dest_snap,
                &bitmap_snap,
                &rig.name("s1"),
                &rig.name("s2"),
            )
            .await
            .map(drop),
    ];
    for result in refused {
        assert_eq!(result.unwrap_err().kind(), ErrorKind::InvalidInput);
    }
    drop(reopened);

    // The drive copies on and cuts over with the guest's every write.
    rig.hydrated(REGIONS).await;
    rig.disk.cut_over(&rig.name("drive")).await.unwrap();
    drop(bitmap);
    rig.disk.delete(&rig.name("bitmap")).await.unwrap();
    assert_eq!(first_difference(&whole(&drive).await, &expected), None);
    assert_eq!(first_difference(&whole(&dest).await, &expected), None);
    // The view depends on its snapshots only.
    assert_eq!(first_difference(&whole(&view).await, &seen), None);

    drop((view, drive, dest, dest_snap, bitmap_snap));
    rig.disk.remove_clone(&rig.name("view")).await.unwrap();
    rig.disk.remove_clone(&rig.name("drive")).await.unwrap();
    for role in ["dest-snap", "bitmap-snap", "dest"] {
        rig.disk.delete(&rig.name(role)).await.unwrap();
    }
    assert_eq!(rig.disk_leftovers(), Vec::<String>::new());
}

#[tokio::test]
#[ignore]
async fn a_drive_refuses_what_its_state_does_not_allow() {
    let Some(rig) = Rig::new("refuse", None).await else {
        return;
    };
    let dest = rig.volume("dest", SIZE).await;
    let bitmap = rig.volume("bitmap", clone_bitmap_bytes(SIZE)).await;
    let big = rig.volume("big", 2 * SIZE).await;
    let other = rig.volume("other", clone_bitmap_bytes(SIZE)).await;
    let dest2 = rig.volume("dest2", SIZE).await;
    let small = rig.volume("small", 512).await;
    let taken = rig.volume("taken", clone_bitmap_bytes(SIZE)).await;

    // The source is smaller than the destination: the kernel refuses the
    // table and no device is left.
    let err = rig
        .disk
        .clone_drive(&rig.name("drive"), &big, &bitmap, &rig.source, off(1, 1))
        .await
        .unwrap_err();
    assert!(
        !rig.leftovers().iter().any(|d| d.contains("-drive-")),
        "{err}"
    );

    let err = rig
        .disk
        .clone_drive(&rig.name("drive"), &dest, &small, &rig.source, off(1, 1))
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::InvalidInput, "{err}");

    let drive = rig.clone_drive(&dest, &bitmap, off(1, 1)).await;
    let err = rig
        .disk
        .clone_drive(&rig.name("drive"), &dest, &bitmap, &rig.source, off(1, 1))
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::AlreadyExists);
    // A second drive over the first one's destination or bitmap.
    for (d, b) in [(&dest, &other), (&dest2, &bitmap)] {
        let err = rig
            .disk
            .clone_drive(&rig.name("second"), d, b, &rig.source, off(1, 1))
            .await
            .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::ResourceBusy, "{err}");
    }
    assert_eq!(
        rig.disk.delete(&rig.name("dest")).await.unwrap_err().kind(),
        ErrorKind::ResourceBusy
    );
    assert_eq!(
        rig.disk
            .rename(&rig.name("drive"), &rig.name("moved"))
            .await
            .unwrap_err()
            .kind(),
        ErrorKind::InvalidInput
    );

    // A snapshot name that is taken: the call fails, deletes nothing it
    // did not make, and leaves the drive running.
    let err = rig
        .disk
        .snapshot_clone(
            &rig.name("drive"),
            &dest,
            &bitmap,
            &rig.name("taken"),
            &rig.name("bitmap-snap"),
        )
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::AlreadyExists, "{err}");
    rig.disk.open(&rig.name("taken")).await.unwrap();
    assert_eq!(
        rig.disk
            .open(&rig.name("bitmap-snap"))
            .await
            .unwrap_err()
            .kind(),
        ErrorKind::NotFound
    );
    let mut expected = rig.bytes.clone();
    write(&drive, &mut expected, 4096, vec![0x77; 4096]).await;
    assert_eq!(first_difference(&whole(&drive).await, &expected), None);
    let err = rig
        .disk
        .snapshot_clone(
            &rig.name("drive"),
            &dest,
            &other,
            &rig.name("dest-snap"),
            &rig.name("bitmap-snap"),
        )
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::InvalidInput);
    assert_eq!(
        rig.disk
            .remove_clone(&rig.name("dest"))
            .await
            .unwrap_err()
            .kind(),
        ErrorKind::InvalidInput
    );
    assert_eq!(
        rig.disk
            .remove_clone(&rig.name("drive"))
            .await
            .unwrap_err()
            .kind(),
        ErrorKind::ResourceBusy
    );

    drop(drive);
    rig.disk.enable_hydration(&rig.name("drive")).await.unwrap();
    rig.hydrated(REGIONS).await;
    rig.disk.cut_over(&rig.name("drive")).await.unwrap();
    assert_eq!(
        rig.disk
            .enable_hydration(&rig.name("drive"))
            .await
            .unwrap_err()
            .kind(),
        ErrorKind::InvalidInput
    );
    // The cutover frees the bitmap, which still holds the drive's header.
    let err = rig
        .disk
        .clone_drive(&rig.name("second"), &dest2, &bitmap, &rig.source, off(1, 1))
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::InvalidInput, "{err}");
    rig.disk.remove_clone(&rig.name("drive")).await.unwrap();
    assert_eq!(
        rig.disk
            .clone_state(&rig.name("drive"))
            .await
            .unwrap_err()
            .kind(),
        ErrorKind::NotFound
    );

    drop((dest, bitmap, big, other, dest2, small, taken));
    for role in ["dest", "bitmap", "big", "other", "dest2", "small", "taken"] {
        rig.disk.delete(&rig.name(role)).await.unwrap();
    }
    assert_eq!(rig.disk_leftovers(), Vec::<String>::new());
}

/// A process death between a suspend and its resume leaves the drive and
/// its destination suspended. Opening the pool again resumes both: the
/// guest's write that waited completes, and the copy runs to the cutover.
#[tokio::test]
#[ignore]
async fn a_drive_left_suspended_resumes_when_the_pool_is_opened_again() {
    let Some(rig) = Rig::new("resume", Some(30)).await else {
        return;
    };
    let dest = rig.volume("dest", SIZE).await;
    let bitmap = rig.volume("bitmap", clone_bitmap_bytes(SIZE)).await;
    let drive = rig.clone_drive(&dest, &bitmap, off(1, 1)).await;
    rig.disk.enable_hydration(&rig.name("drive")).await.unwrap();
    rig.hydrated(2).await;

    for role in ["drive", "dest"] {
        run("dmsetup", &["suspend", &rig.device(&rig.name(role))]);
    }
    let mut expected = rig.bytes.clone();
    let write = pattern(8192, 3);
    expected[(7 * REGION) as usize..(7 * REGION) as usize + 8192].copy_from_slice(&write);
    let path = drive.path.clone();
    let writer = std::thread::spawn(move || {
        let file = std::fs::File::options().write(true).open(path).unwrap();
        file.write_all_at(&write, 7 * REGION).unwrap();
        file.sync_all().unwrap();
    });
    std::thread::sleep(Duration::from_millis(200));
    assert!(!writer.is_finished(), "a write to a suspended drive waits");

    let pool = std::env::var("ENGRAM_TEST_THIN_POOL").unwrap();
    let disk = ThinPoolDisk::open(&pool, CHUNK, Path::new(NODES))
        .await
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !writer.is_finished() {
        assert!(
            std::time::Instant::now() < deadline,
            "the write still waits after the pool was opened again"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    writer.join().unwrap();
    rig.hydrated(REGIONS).await;
    disk.cut_over(&rig.name("drive")).await.unwrap();
    assert_eq!(first_difference(&whole(&drive).await, &expected), None);

    drop((drive, bitmap));
    rig.disk.delete(&rig.name("bitmap")).await.unwrap();
    rig.disk.remove_clone(&rig.name("drive")).await.unwrap();
    assert_eq!(first_difference(&whole(&dest).await, &expected), None);
    drop(dest);
    rig.disk.delete(&rig.name("dest")).await.unwrap();
    assert_eq!(rig.disk_leftovers(), Vec::<String>::new());
}

/// A `clone_drive` cut after the device was made leaves it with no live
/// table, and with its table loaded inactive the kernel has already
/// written the bitmap's header. The restore removes the drive, makes the
/// bitmap again, and converges.
#[tokio::test]
#[ignore]
async fn a_half_made_drive_is_removed_and_made_again() {
    let Some(rig) = Rig::new("half", None).await else {
        return;
    };
    for loaded in [false, true] {
        let dest = rig.volume("dest", SIZE).await;
        let bitmap = rig.volume("bitmap", clone_bitmap_bytes(SIZE)).await;
        let device = rig.device(&rig.name("drive"));
        run("dmsetup", &["create", "--noudevsync", "--notable", &device]);
        if loaded {
            let table = format!(
                "0 {} clone {} {} {} 128 1 no_hydration",
                SIZE / 512,
                devno(&bitmap),
                devno(&dest),
                rig.source.display()
            );
            run("dmsetup", &["load", &device, "--table", &table]);
        }
        assert_eq!(rig.state("drive").await, CloneState::Incomplete);
        assert_eq!(
            rig.disk
                .open_clone(&rig.name("drive"))
                .await
                .unwrap_err()
                .kind(),
            ErrorKind::InvalidData
        );

        rig.disk.remove_clone(&rig.name("drive")).await.unwrap();
        if loaded {
            let err = rig
                .disk
                .clone_drive(&rig.name("drive"), &dest, &bitmap, &rig.source, off(1, 1))
                .await
                .unwrap_err();
            assert_eq!(err.kind(), ErrorKind::InvalidInput, "{err}");
        }
        drop(bitmap);
        rig.disk.delete(&rig.name("bitmap")).await.unwrap();
        let bitmap = rig.volume("bitmap", clone_bitmap_bytes(SIZE)).await;
        let drive = rig.clone_drive(&dest, &bitmap, off(4, 4)).await;
        let mut expected = rig.bytes.clone();
        write(&drive, &mut expected, REGION + 512, vec![0xC3; 4096]).await;
        rig.disk.enable_hydration(&rig.name("drive")).await.unwrap();
        rig.hydrated(REGIONS).await;
        rig.disk.cut_over(&rig.name("drive")).await.unwrap();
        assert_eq!(first_difference(&whole(&drive).await, &expected), None);

        drop((drive, bitmap));
        rig.disk.remove_clone(&rig.name("drive")).await.unwrap();
        rig.disk.delete(&rig.name("bitmap")).await.unwrap();
        assert_eq!(first_difference(&whole(&dest).await, &expected), None);
        drop(dest);
        rig.disk.delete(&rig.name("dest")).await.unwrap();
    }
    assert_eq!(rig.disk_leftovers(), Vec::<String>::new());
}

/// [`FileSources`] behind a read-only loop device, as the block device a
/// drive needs, and behind dm-delay when reads must be slow.
struct LoopSources {
    files: FileSources,
    delay_ms: Option<u32>,
    devices: Mutex<HashMap<(SandboxId, DiskRole), LoopSource>>,
}

struct LoopSource {
    loop_dev: String,
    /// The dm-delay device over the loop device.
    delay: Option<String>,
}

impl LoopSource {
    fn path(&self) -> PathBuf {
        match &self.delay {
            Some(delay) => Path::new("/dev/mapper").join(delay),
            None => PathBuf::from(&self.loop_dev),
        }
    }
}

impl LoopSources {
    /// The devices, also after a test panicked while it held them.
    fn devices(&self) -> std::sync::MutexGuard<'_, HashMap<(SandboxId, DiskRole), LoopSource>> {
        self.devices.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn detach_all(&self) {
        let sources: Vec<LoopSource> = self.devices().drain().map(|(_, s)| s).collect();
        for source in sources {
            if let Some(delay) = &source.delay {
                let _ = remove_dm(delay);
            }
            let _ = Command::new("losetup")
                .args(["-d", &source.loop_dev])
                .output();
        }
    }
}

/// Remove a device-mapper device, waiting a bounded time for a holder
/// that goes by itself, such as a udev probe of a new device.
fn remove_dm(name: &str) -> std::io::Result<()> {
    let mut stderr = String::new();
    for _ in 0..10 {
        let output = Command::new("dmsetup")
            .args(["remove", "--retry", "--noudevsync", name])
            .output()?;
        if output.status.success() {
            return Ok(());
        }
        stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        std::thread::sleep(Duration::from_millis(200));
    }
    Err(std::io::Error::new(
        ErrorKind::ResourceBusy,
        format!("dmsetup remove {name}: {stderr}"),
    ))
}

#[async_trait]
impl DiskSources for LoopSources {
    fn can_start(&self) -> bool {
        true
    }
    async fn start(
        &self,
        sandbox: SandboxId,
        role: DiskRole,
        manifest: ManifestRef,
        base: Option<(&Volume, ManifestRef)>,
    ) -> std::io::Result<PathBuf> {
        let file = self.files.start(sandbox, role, manifest, base).await?;
        let loop_dev = run(
            "losetup",
            &["--read-only", "--show", "-f", file.to_str().unwrap()],
        );
        let delay = self.delay_ms.map(|ms| {
            let name = format!("engram-test-delay-{}-{role}", sandbox.as_uuid().simple());
            let sectors = std::fs::metadata(&file).unwrap().len() / 512;
            let table = format!("0 {sectors} delay {loop_dev} 0 {ms}");
            run(
                "dmsetup",
                &["create", "--noudevsync", &name, "--table", &table],
            );
            run("dmsetup", &["mknodes", &name]);
            name
        });
        let source = LoopSource { loop_dev, delay };
        let path = source.path();
        self.devices().insert((sandbox, role), source);
        Ok(path)
    }

    async fn ensure_alive(&self, sandbox: SandboxId, role: DiskRole) -> std::io::Result<()> {
        self.files.ensure_alive(sandbox, role).await
    }

    async fn served(&self, sandbox: SandboxId, role: DiskRole) -> std::io::Result<Served> {
        let served = self.files.served(sandbox, role).await?;
        let device = self.devices().get(&(sandbox, role)).map(LoopSource::path);
        Ok(Served {
            device: device.ok_or(ErrorKind::NotFound)?,
            ..served
        })
    }

    async fn stop(&self, sandbox: SandboxId, role: DiskRole) -> std::io::Result<()> {
        let source = self.devices().remove(&(sandbox, role));
        if let Some(source) = source {
            if let Some(delay) = &source.delay {
                if let Err(e) = remove_dm(delay) {
                    self.devices().insert((sandbox, role), source);
                    return Err(e);
                }
            }
            let detached = Command::new("losetup")
                .args(["-d", &source.loop_dev])
                .output()?;
            if !detached.status.success() {
                return Err(std::io::Error::other(format!(
                    "losetup -d {}: {}",
                    source.loop_dev,
                    String::from_utf8_lossy(&detached.stderr)
                )));
            }
        }
        self.files.stop(sandbox, role).await
    }

    async fn list(&self) -> std::io::Result<Vec<(SandboxId, DiskRole)>> {
        self.files.list().await
    }
}

/// Every device-mapper device whose name holds `id`: a sandbox's with
/// its source's delay, or an image's.
fn devices_naming(id: impl std::fmt::Display) -> Vec<String> {
    let id = id.to_string();
    run("dmsetup", &["ls"])
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .filter(|name| name.contains(&id))
        .map(str::to_string)
        .collect()
}

/// Session volumes over the pool with loop-device sources, an image
/// manifest of `SIZE` pattern bytes, and one sandbox. Dropping it removes
/// what a failed test left of the sandbox and the image: their devices,
/// in passes because a drive holds its thins, then the loop devices.
struct SessionRig {
    disk: ThinPoolDisk,
    store: ChunkStore,
    files: FileDisk,
    sources: Arc<LoopSources>,
    volumes: SessionVolumes,
    bytes: Vec<u8>,
    image: ManifestRef,
    sandbox: SandboxId,
    _dir: tempfile::TempDir,
}

impl SessionRig {
    /// `None`, with a `SKIP:` line, when no pool is named.
    async fn new(read_delay_ms: Option<u32>) -> Option<Self> {
        let Ok(pool) = std::env::var("ENGRAM_TEST_THIN_POOL") else {
            eprintln!("SKIP: ENGRAM_TEST_THIN_POOL is not set");
            return None;
        };
        let disk = ThinPoolDisk::open(&pool, CHUNK, Path::new(NODES))
            .await
            .expect("open pool");
        let dir = tempfile::tempdir().unwrap();
        let store = ChunkStore::new(Arc::new(LocalBlobStorage::new(dir.path().join("blobs"))));
        let files = FileDisk::new(dir.path().join("files"), CHUNK).unwrap();
        let bytes = pattern(SIZE as usize, 0x2545_F491);
        let image = publish(&files, &store, "image", &bytes).await;
        let sources = Arc::new(LoopSources {
            files: FileSources::new(dir.path().join("sources"), store.clone()).unwrap(),
            delay_ms: read_delay_ms,
            devices: Mutex::new(HashMap::new()),
        });
        let volumes = SessionVolumes::new(Arc::new(disk.clone()), store.clone())
            .with_sources(sources.clone());
        Some(Self {
            disk,
            store,
            files,
            sources,
            volumes,
            bytes,
            image,
            sandbox: SandboxId::new(),
            _dir: dir,
        })
    }

    /// The sandbox's root, provisioned lazily from the image.
    async fn lazy_root(&self) -> Volume {
        self.root(Source::LazyFork {
            manifest: self.image,
            base: None,
        })
        .await
    }

    /// The sandbox's root, provisioned from `source`.
    async fn root(&self, source: Source) -> Volume {
        let provisioned = self
            .volumes
            .provision(self.sandbox, source, None)
            .await
            .unwrap();
        Volume::open(
            VolumeName::new("probe").unwrap(),
            &provisioned.root,
            SIZE,
            CHUNK,
        )
        .unwrap()
    }

    async fn root_state(&self) -> CloneState {
        self.disk
            .clone_state(&live_name(self.sandbox, DiskRole::Root))
            .await
            .unwrap()
    }

    /// Release the sandbox and check that nothing of it is left.
    async fn release_clean(&self) {
        self.volumes.release(self.sandbox).await.unwrap();
        assert_eq!(self.sources.list().await.unwrap(), Vec::new());
        assert_eq!(self.sources.devices().len(), 0);
        assert_eq!(
            devices_naming(self.sandbox.as_uuid().simple()),
            Vec::<String>::new()
        );
    }
}

impl Drop for SessionRig {
    fn drop(&mut self) {
        let ids = [
            self.sandbox.as_uuid().simple().to_string(),
            self.image.manifest_id.simple().to_string(),
        ];
        for _ in 0..4 {
            for device in ids.iter().flat_map(devices_naming) {
                let _ = Command::new("dmsetup")
                    .args(["remove", "--retry", "--noudevsync", &device])
                    .output();
                let _ = std::fs::remove_file(Path::new(NODES).join(&device));
            }
        }
        self.sources.detach_all();
    }
}

/// Publish `bytes` as a disk manifest, through a file volume `name`.
async fn publish(files: &FileDisk, store: &ChunkStore, name: &str, bytes: &[u8]) -> ManifestRef {
    let volume = files
        .create(&VolumeName::new(name).unwrap(), bytes.len() as u64)
        .await
        .unwrap();
    volume.write_range(0, bytes.to_vec()).await.unwrap();
    let snap = VolumeName::new(format!("{name}-snap")).unwrap();
    let (_, manifest) = checkpoint(files, store, &volume, &snap, None)
        .await
        .unwrap();
    let published = ManifestRef::new();
    store.put_manifest(published, &manifest).await.unwrap();
    published
}

/// The manifest's bytes, staged into a file volume `name`.
async fn staged_bytes(files: &FileDisk, store: &ChunkStore, name: &str, m: ManifestRef) -> Vec<u8> {
    let manifest = store.get_manifest(m).await.unwrap();
    let staged = stage(
        files,
        store,
        &VolumeName::new(name).unwrap(),
        &manifest,
        None,
    )
    .await
    .unwrap();
    whole(&staged).await
}

/// A lazy provision boots the role over a drive that reads the manifest
/// from its source; a release removes the drive, stops the source and
/// deletes every volume of the sandbox.
#[tokio::test]
#[ignore]
async fn a_lazy_session_role_reads_its_manifest_and_releases_clean() {
    let Some(rig) = SessionRig::new(None).await else {
        return;
    };
    let drive = rig.lazy_root().await;
    assert_eq!(first_difference(&whole(&drive).await, &rig.bytes), None);
    let state = rig.root_state().await;
    assert!(
        matches!(
            state,
            CloneState::Copying {
                hydration_enabled: true,
                ..
            }
        ),
        "{state:?}"
    );
    drop(drive);
    rig.release_clean().await;
}

/// A checkpoint while the drive copies reads through a view over the
/// snapshot pair and the source: staged again, the published manifest is
/// exactly what the drive read at the take, guest writes included.
#[tokio::test]
#[ignore]
async fn a_checkpoint_mid_hydration_stages_into_what_the_drive_read() {
    let Some(rig) = SessionRig::new(Some(30)).await else {
        return;
    };
    let drive = rig.lazy_root().await;
    let mut expected = rig.bytes.clone();
    write(&drive, &mut expected, 60 * REGION + 4096, vec![0x44; 4096]).await;
    write(
        &drive,
        &mut expected,
        5 * REGION,
        pattern(REGION as usize, 9),
    )
    .await;
    let CloneState::Copying {
        hydrated_regions, ..
    } = rig.root_state().await
    else {
        panic!("the drive is not copying");
    };
    eprintln!("{hydrated_regions} of {REGIONS} regions copied before the take");
    assert!(hydrated_regions < REGIONS);

    let published = rig
        .volumes
        .checkpoint(rig.sandbox, DiskRole::Root)
        .await
        .unwrap();

    let staged = staged_bytes(&rig.files, &rig.store, "staged", published).await;
    assert_eq!(first_difference(&staged, &expected), None);
    assert!(matches!(rig.root_state().await, CloneState::Copying { .. }));
    drop(drive);
    rig.release_clean().await;
}

/// Hydrated to the end, the role cuts over through the session volumes:
/// a handle opened before reads the manifest and the guest's write, and
/// the bitmap, the source and its loop device are gone. The first
/// checkpoint after the cutover stages into what the drive reads.
#[tokio::test]
#[ignore]
async fn a_hydrated_session_role_cuts_over_and_stops_its_source() {
    let Some(rig) = SessionRig::new(None).await else {
        return;
    };
    let drive = rig.lazy_root().await;
    let mut expected = rig.bytes.clone();
    write(&drive, &mut expected, 3 * REGION + 4096, vec![0x5A; 4096]).await;
    let deadline = tokio::time::Instant::now() + HYDRATION_DEADLINE;
    loop {
        match rig
            .volumes
            .cut_over(rig.sandbox, DiskRole::Root)
            .await
            .unwrap()
        {
            Cutover::Done => break,
            Cutover::Waiting(Waiting::Copying(_)) => {}
            waiting => panic!("{waiting:?}"),
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "not hydrated in {HYDRATION_DEADLINE:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    assert_eq!(rig.root_state().await, CloneState::CutOver);
    assert_eq!(first_difference(&whole(&drive).await, &expected), None);
    assert_eq!(rig.sources.list().await.unwrap(), Vec::new());
    assert_eq!(rig.sources.devices().len(), 0);
    let bitmap = format!("{}-root-bitmap", rig.sandbox.as_uuid().simple());
    let devices = devices_naming(rig.sandbox.as_uuid().simple());
    assert!(!devices.iter().any(|d| d.ends_with(&bitmap)), "{devices:?}");
    assert_eq!(
        rig.volumes
            .hydration(rig.sandbox, DiskRole::Root)
            .await
            .unwrap(),
        None
    );

    let published = rig
        .volumes
        .checkpoint(rig.sandbox, DiskRole::Root)
        .await
        .unwrap();
    let staged = staged_bytes(&rig.files, &rig.store, "staged", published).await;
    assert_eq!(first_difference(&staged, &expected), None);
    drop(drive);
    let report = rig.volumes.purge(rig.sandbox).await.unwrap();
    assert!(report.failed.is_empty(), "{:?}", report.failed);
    assert_eq!(
        devices_naming(rig.sandbox.as_uuid().simple()),
        Vec::<String>::new()
    );
}

/// Over a staged base the destination is a snapshot of the image
/// volume. A chunk the destination never wrote is the source's, also
/// where the base differs; a chunk the guest wrote with the base's own
/// bytes is the guest's, although its content equals the origin's.
#[tokio::test]
#[ignore]
async fn a_checkpoint_over_a_staged_base_stages_into_what_the_drive_read() {
    let Some(rig) = SessionRig::new(Some(50)).await else {
        return;
    };
    let image_volume = image_volumes::volume_name(rig.image);
    let base = rig.store.get_manifest(rig.image).await.unwrap();
    drop(
        stage(&rig.disk, &rig.store, &image_volume, &base, None)
            .await
            .unwrap(),
    );
    let mut wanted = rig.bytes.clone();
    let late = (40 * REGION) as usize;
    wanted[late..].copy_from_slice(&pattern(SIZE as usize - late, 0x1357_9BDF));
    let manifest = publish(&rig.files, &rig.store, "continued", &wanted).await;

    let drive = rig
        .root(Source::LazyContinue {
            manifest,
            base: Some(rig.image),
        })
        .await;
    let mut expected = wanted.clone();
    let region = (50 * REGION) as usize..(51 * REGION) as usize;
    write(
        &drive,
        &mut expected,
        50 * REGION,
        rig.bytes[region].to_vec(),
    )
    .await;
    let CloneState::Copying {
        hydrated_regions, ..
    } = rig.root_state().await
    else {
        panic!("the drive is not copying");
    };
    eprintln!("{hydrated_regions} of {REGIONS} regions copied before the take");
    assert!(hydrated_regions < 40);

    let published = rig
        .volumes
        .checkpoint(rig.sandbox, DiskRole::Root)
        .await
        .unwrap();

    let staged = staged_bytes(&rig.files, &rig.store, "staged", published).await;
    assert_eq!(first_difference(&staged, &expected), None);
    drop(drive);
    rig.release_clean().await;
    rig.disk.delete(&image_volume).await.unwrap();
    assert_eq!(
        devices_naming(rig.image.manifest_id.simple()),
        Vec::<String>::new()
    );
}
