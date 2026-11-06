#![cfg(target_os = "linux")]
//! Cloned drives against a real dm-thin pool, as root.
//! `ENGRAM_TEST_THIN_POOL` names the pool; unset, every test skips. The
//! source is a read-only loop device over a file, behind dm-delay where
//! a test needs the background copy to take time.

use std::io::ErrorKind;
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use engram_disk_thin::ThinPoolDisk;
use engram_host_core::disk::{
    CloneState, Hydration, SessionDisk, Volume, VolumeName, CLONE_REGION_BYTES,
};

const CHUNK: u64 = 65536;
const REGION: u64 = CLONE_REGION_BYTES;
const REGIONS: u64 = 64;
const SIZE: u64 = REGION * REGIONS;
const BITMAP: u64 = 4 << 20;
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
    let bitmap = rig.volume("bitmap", BITMAP).await;
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
    let bitmap = rig.volume("bitmap", BITMAP).await;
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
    let bitmap = rig.volume("bitmap", BITMAP).await;
    let big = rig.volume("big", 2 * SIZE).await;
    let other = rig.volume("other", BITMAP).await;
    let dest2 = rig.volume("dest2", SIZE).await;
    let small = rig.volume("small", 512).await;
    let taken = rig.volume("taken", BITMAP).await;

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
    let bitmap = rig.volume("bitmap", BITMAP).await;
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
        let bitmap = rig.volume("bitmap", BITMAP).await;
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
        let bitmap = rig.volume("bitmap", BITMAP).await;
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
