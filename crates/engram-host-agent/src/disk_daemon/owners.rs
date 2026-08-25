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
//! content is the owning sandbox UUID. Lifecycle:
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

use engram_core::SandboxId;

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

    /// Durably record `device` → `sandbox_id`. Atomic via tmp + rename;
    /// best-effort (an error is logged, never propagated — attribution is
    /// recovery insurance, and the attach must not fail on it).
    pub fn record(&self, device: &Path, sandbox_id: SandboxId) {
        let Some(path) = self.record_path(device) else {
            return;
        };
        let tmp = path.with_extension("tmp");
        let write = || -> io::Result<()> {
            std::fs::write(&tmp, sandbox_id.to_string().as_bytes())?;
            std::fs::rename(&tmp, &path)?;
            // Best-effort directory durability; the record is insurance, so a
            // lost fsync degrades to the pre-#1378 (unattributed) behavior.
            if let Ok(d) = std::fs::File::open(&self.dir) {
                let _ = d.sync_all();
            }
            Ok(())
        };
        if let Err(error) = write() {
            tracing::warn!(
                device = %device.display(),
                %sandbox_id,
                %error,
                "NBD owner record write failed; device stays unattributed for \
                 the next generation",
            );
        }
    }

    /// Load every readable owner record as `/dev/<name>` → sandbox. An
    /// unparsable record is removed (a torn legacy state, tolerated per the
    /// crash-state discipline) and skipped.
    pub fn load(&self) -> HashMap<PathBuf, SandboxId> {
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
                let _ = std::fs::remove_file(&path);
                continue;
            }
            let parsed = std::fs::read_to_string(&path)
                .ok()
                .and_then(|s| s.trim().parse::<SandboxId>().ok());
            match parsed {
                Some(id) => {
                    out.insert(PathBuf::from("/dev").join(name), id);
                }
                None => {
                    tracing::warn!(
                        path = %path.display(),
                        "unreadable NBD owner record; removing",
                    );
                    let _ = std::fs::remove_file(&path);
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
    fn record_load_roundtrip_and_overwrite() {
        let dir = tempfile::tempdir().expect("tempdir");
        let owners = NbdOwnerDir::open(dir.path().join("nbd-owners")).expect("open");
        let a = SandboxId::new();
        let b = SandboxId::new();
        owners.record(Path::new("/dev/nbd4"), a);
        owners.record(Path::new("/dev/nbd7"), b);
        let loaded = owners.load();
        assert_eq!(loaded.get(Path::new("/dev/nbd4")), Some(&a));
        assert_eq!(loaded.get(Path::new("/dev/nbd7")), Some(&b));
        // Slot reuse overwrites.
        owners.record(Path::new("/dev/nbd4"), b);
        assert_eq!(owners.load().get(Path::new("/dev/nbd4")), Some(&b));
    }

    #[test]
    fn stale_records_are_removed_and_torn_records_tolerated() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("nbd-owners");
        let owners = NbdOwnerDir::open(root.clone()).expect("open");
        let a = SandboxId::new();
        owners.record(Path::new("/dev/nbd4"), a);
        owners.record(Path::new("/dev/nbd9"), SandboxId::new());
        // A torn/garbage record (crash-state discipline: externally
        // constructed) is skipped and removed on load.
        std::fs::write(root.join("nbd12"), b"not-a-uuid").expect("garbage");
        // A leftover tmp from a torn rename is swept.
        std::fs::write(root.join("nbd13.tmp"), b"whatever").expect("tmp");
        let loaded = owners.load();
        assert_eq!(loaded.len(), 2);
        assert!(!root.join("nbd12").exists());
        assert!(!root.join("nbd13.tmp").exists());
        // Only nbd4 is still kernel-connected: nbd9's record is inert and
        // lazily cleaned.
        let connected: HashSet<PathBuf> = [PathBuf::from("/dev/nbd4")].into_iter().collect();
        owners.remove_stale(&connected);
        let after = owners.load();
        assert_eq!(after.len(), 1);
        assert_eq!(after.get(Path::new("/dev/nbd4")), Some(&a));
    }
}
