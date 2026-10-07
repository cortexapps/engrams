//! engrams#1378: durable device→sandbox OWNER RECORDS for the NBD data plane.
//!
//! The kernel NBD binding is the one resource that deliberately outlives the
//! host-agent process (the survivor design), but device→sandbox attribution
//! used to live only in memory — a teardown that died between the FC kill and
//! the NBD disconnect (a destroy racing a pod roll, the 2026-08-25
//! `rehydrate-unknown-device` firing) left a kernel-connected device NO
//! successor generation could account for, and the resulting quarantine had
//! no settlement owner.
//!
//! An owner record is one tiny file, `<dir>/<devname>` (e.g. `nbd4`), whose
//! content is the owning sandbox UUID for root, or a JSON owner and role
//! for swap. Lifecycle:
//!
//! - **Written at the id-known point** of every attach (the
//!   `install_flush_scheduler` call sites — the exact moment the pending
//!   dirty file is renamed onto the sandbox id) and at every survivor
//!   rehydrate. Atomic (tmp + rename), so a torn write never yields a
//!   half-record.
//! - **Never removed on teardown.** A record is only MEANINGFUL for a device
//!   the kernel currently has CONNECTED; a record for a disconnected device
//!   is inert and is lazily removed by [`NbdOwnerDir::remove_stale`] at the
//!   next startup classification. This makes every teardown crash point safe:
//!   disconnect-then-crash leaves an inert record, crash-before-disconnect
//!   leaves the (record ∧ binding) pair the successor needs.
//! - **Overwritten on slot reuse** at the new sandbox's id-known point. The
//!   window between a re-claimed device's CONNECT and its id-known write is
//!   covered by the kernel owner pid: a device this generation CONNECTed
//!   classifies `Serving` (self pid) regardless of any stale record, and a
//!   crash inside that window leaves a failed-create leftover for which a
//!   stale-record disposition (coordinator-ordered disconnect) is the correct
//!   outcome anyway.
//!
//! The startup classification barrier reads these records to attribute
//! kernel-connected devices (`StartupSlot::attributed`); an attributed
//! dead-owner device with no rehydrate record becomes
//! `ResidueAwaitingTombstone` — reported to the coordinator and settled by
//! its tombstone — instead of an operator-paging `QuarantinedUnknown`.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};

use super::DiskRole;
use engram_core::SandboxId;

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DiskOwner {
    pub sandbox_id: SandboxId,
    #[serde(default)]
    pub role: DiskRole,
}

/// The owner-record directory (prod: `<work_dir>/nbd-owners`).
pub struct NbdOwnerDir {
    dir: PathBuf,
}

impl NbdOwnerDir {
    /// Open (creating if absent) the owner-record directory.
    pub fn open(dir: PathBuf) -> io::Result<Self> {
        std::fs::create_dir_all(&dir)?;
        Ok(Self { dir })
    }

    fn record_path(&self, device: &Path) -> Option<PathBuf> {
        let name = device.file_name()?;
        Some(self.dir.join(name))
    }

    /// Atomically record the owner. Root retains the legacy UUID format.
    pub fn record(&self, device: &Path, sandbox_id: SandboxId, role: DiskRole) -> io::Result<()> {
        let path = self.record_path(device).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "device has no file name")
        })?;
        let tmp = path.with_extension("tmp");
        let bytes = match role {
            DiskRole::Root => sandbox_id.to_string().into_bytes(),
            DiskRole::Swap => serde_json::to_vec(&DiskOwner { sandbox_id, role })?,
        };
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(&tmp, &path)?;
        // Preserve the legacy best-effort directory durability.
        if let Ok(dir) = std::fs::File::open(&self.dir) {
            let _ = dir.sync_all();
        }
        Ok(())
    }

    /// Run once at startup, before any owner writer can start.
    pub fn sweep_stale_tmp(&self) -> io::Result<()> {
        for entry in std::fs::read_dir(&self.dir)? {
            let path = entry?.path();
            if path.extension().is_some_and(|ext| ext == "tmp") {
                std::fs::remove_file(path)?;
            }
        }
        Ok(())
    }

    /// Read owner records without modifying the directory, including torn files.
    pub fn load(&self) -> HashMap<PathBuf, DiskOwner> {
        let mut out = HashMap::new();
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return out;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if name.ends_with(".tmp") {
                continue;
            }
            let parsed = std::fs::read_to_string(&path).ok().and_then(|s| {
                serde_json::from_str::<DiskOwner>(&s).ok().or_else(|| {
                    s.trim()
                        .parse::<SandboxId>()
                        .ok()
                        .map(|sandbox_id| DiskOwner {
                            sandbox_id,
                            role: DiskRole::Root,
                        })
                })
            });
            match parsed {
                Some(id) => {
                    out.insert(PathBuf::from("/dev").join(name), id);
                }
                None => {
                    tracing::warn!(
                        path = %path.display(),
                        "unreadable NBD owner record; skipping",
                    );
                }
            }
        }
        out
    }

    /// Remove records for devices NOT in `connected` — inert leftovers of
    /// completed teardowns, cleaned lazily at startup classification.
    pub fn remove_stale(&self, connected: &std::collections::HashSet<PathBuf>) {
        for (device, _) in self.load() {
            if !connected.contains(&device) {
                if let Some(path) = self.record_path(&device) {
                    let _ = std::fs::remove_file(path);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn root_bytes_and_reader_writer_interleaving() {
        let dir = tempfile::tempdir().unwrap();
        let owners = NbdOwnerDir::open(dir.path().to_path_buf()).unwrap();
        let id = SandboxId::new();
        owners
            .record(Path::new("/dev/nbd0"), id, DiskRole::Root)
            .unwrap();
        assert_eq!(
            std::fs::read(dir.path().join("nbd0")).unwrap(),
            id.to_string().as_bytes()
        );
        let tmp = dir.path().join("nbd1.tmp");
        std::fs::write(&tmp, id.to_string()).unwrap();
        owners.load();
        std::fs::rename(tmp, dir.path().join("nbd1")).unwrap();
        assert_eq!(owners.load()[Path::new("/dev/nbd1")].sandbox_id, id);
    }

    #[test]
    fn legacy_root_and_torn_role_records() {
        let tmp = tempfile::tempdir().unwrap();
        let owners = NbdOwnerDir::open(tmp.path().to_path_buf()).unwrap();
        let id = SandboxId::new();
        std::fs::write(tmp.path().join("nbd0"), id.to_string()).unwrap();
        assert_eq!(owners.load()[Path::new("/dev/nbd0")].role, DiskRole::Root);
        owners
            .record(Path::new("/dev/nbd1"), id, DiskRole::Swap)
            .unwrap();
        let path = tmp.path().join("nbd1");
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(owners.load()[Path::new("/dev/nbd1")].role, DiskRole::Swap);
        for cut in 0..bytes.len() {
            std::fs::write(&path, &bytes[..cut]).unwrap();
            let records = owners.load();
            assert!(!records.contains_key(Path::new("/dev/nbd1")));
            assert_eq!(records[Path::new("/dev/nbd0")].role, DiskRole::Root);
        }
    }

    #[test]
    fn record_load_roundtrip_and_overwrite() {
        let dir = tempfile::tempdir().expect("tempdir");
        let owners = NbdOwnerDir::open(dir.path().join("nbd-owners")).expect("open");
        let a = SandboxId::new();
        let b = SandboxId::new();
        owners
            .record(Path::new("/dev/nbd4"), a, DiskRole::Root)
            .unwrap();
        owners
            .record(Path::new("/dev/nbd7"), b, DiskRole::Root)
            .unwrap();
        let loaded = owners.load();
        assert_eq!(
            loaded.get(Path::new("/dev/nbd4")),
            Some(&DiskOwner {
                sandbox_id: a,
                role: DiskRole::Root
            })
        );
        assert_eq!(
            loaded.get(Path::new("/dev/nbd7")),
            Some(&DiskOwner {
                sandbox_id: b,
                role: DiskRole::Root
            })
        );
        // Slot reuse overwrites.
        owners
            .record(Path::new("/dev/nbd4"), b, DiskRole::Root)
            .unwrap();
        assert_eq!(
            owners.load().get(Path::new("/dev/nbd4")),
            Some(&DiskOwner {
                sandbox_id: b,
                role: DiskRole::Root
            })
        );
    }

    #[test]
    fn stale_records_are_removed_and_torn_records_tolerated() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("nbd-owners");
        let owners = NbdOwnerDir::open(root.clone()).expect("open");
        let a = SandboxId::new();
        owners
            .record(Path::new("/dev/nbd4"), a, DiskRole::Root)
            .unwrap();
        owners
            .record(Path::new("/dev/nbd9"), SandboxId::new(), DiskRole::Root)
            .unwrap();
        // A torn/garbage record (crash-state discipline: externally
        // constructed) is skipped and retained on load.
        std::fs::write(root.join("nbd12"), b"not-a-uuid").expect("garbage");
        // A leftover tmp from a torn rename is swept.
        std::fs::write(root.join("nbd13.tmp"), b"whatever").expect("tmp");
        let loaded = owners.load();
        assert_eq!(loaded.len(), 2);
        assert!(root.join("nbd12").exists());
        assert!(root.join("nbd13.tmp").exists());
        owners.sweep_stale_tmp().unwrap();
        assert!(!root.join("nbd13.tmp").exists());
        // Only nbd4 is still kernel-connected: nbd9's record is inert and
        // lazily cleaned.
        let connected: HashSet<PathBuf> = [PathBuf::from("/dev/nbd4")].into_iter().collect();
        owners.remove_stale(&connected);
        let after = owners.load();
        assert_eq!(after.len(), 1);
        assert_eq!(
            after.get(Path::new("/dev/nbd4")),
            Some(&DiskOwner {
                sandbox_id: a,
                role: DiskRole::Root
            })
        );
    }
}
