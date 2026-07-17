//! Host-durable per-session fencing-epoch high-water (ADR 0079).
//!
//! One JSON file per session under the host-agent's `epochs_dir` (a
//! work_dir child that survives pod rolls, like `bindings_dir`). The
//! gRPC server gates every session-scoped lifecycle RPC (destroy /
//! snapshot family / pause / resume / restore / start_agent) on this
//! store — `check_and_advance` rejects an epoch below the stored
//! high-water, so a fenced-out op executor's late RPC can never act on
//! a session a successor already re-claimed.
//!
//! Deliberately per-SESSION, not per-sandbox (i.e. NOT `sandbox.json`,
//! which is FC-owned and dies with the sandbox): the fence must survive
//! sandbox replacement — evict/resume swaps the VM identity while the
//! session's epoch lineage continues.
//!
//! Atomic write: writer-unique temp file + rename in the same directory
//! (the same primitive as `bindings.rs`); records are tiny and written
//! only on lifecycle RPCs, so synchronous `std::fs` is fine. Node death
//! loses the directory — acceptable: the epoch is a *fence*, not the
//! authority (`sessions.current_epoch` in PG is), and a fresh host
//! starts at high-water 0, which any real epoch exceeds.

use std::path::PathBuf;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use engram_core::traits::{Clock, SystemClock};
use engram_core::SessionId;
use serde::{Deserialize, Serialize};

/// Bump when the on-disk shape changes incompatibly. Readers reject
/// unknown versions (fail loud, never guess).
const EPOCH_SCHEMA_VERSION: u32 = 1;

/// The durable high-water record, as persisted on the host.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct EpochRecord {
    pub schema_version: u32,
    pub session_id: SessionId,
    /// Highest fencing epoch ever accepted for this session on this host.
    pub epoch: u64,
    pub updated_at: DateTime<Utc>,
}

/// Why a [`SessionEpochStore::check_and_advance`] was refused.
#[derive(Debug)]
pub enum EpochError {
    /// `incoming == 0` — never a legal claimed-op epoch
    /// (`sessions.current_epoch` starts at 0 and is bumped BEFORE being
    /// stamped, so the first real epoch is 1). The gRPC gate currently
    /// short-circuits 0 before reaching the store (the interim
    /// "verb not yet migrated" allowance — TODO(#543)); the store itself
    /// holds the final semantics.
    Zero,
    /// The on-disk high-water is strictly newer — the caller is a
    /// fenced-out predecessor. The record is left untouched.
    Stale {
        stored: u64,
        incoming: u64,
    },
    Io(std::io::Error),
}

impl std::fmt::Display for EpochError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EpochError::Zero => write!(f, "fencing epoch 0 is never valid"),
            EpochError::Stale { stored, incoming } => write!(
                f,
                "stale fencing epoch: stored high-water {stored} > incoming {incoming}"
            ),
            EpochError::Io(e) => write!(f, "session epoch io: {e}"),
        }
    }
}

impl std::error::Error for EpochError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            EpochError::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for EpochError {
    fn from(e: std::io::Error) -> Self {
        EpochError::Io(e)
    }
}

/// Directory-backed store. Cheap to clone.
#[derive(Clone, Debug)]
pub struct SessionEpochStore {
    dir: PathBuf,
    /// ADR 0098 D1: wall clock is an injected world input. P1 wires the
    /// production clock; sim injection rides the flow-extraction PRs.
    clock: Arc<dyn Clock>,
}

impl SessionEpochStore {
    /// Open (creating the directory if needed). The directory must be
    /// on storage that survives a host-agent restart for the fencing
    /// guarantee to hold across rolls — the host-agent wires
    /// `work_dir/epochs`.
    pub fn open(dir: impl Into<PathBuf>) -> std::io::Result<Self> {
        let dir = dir.into();
        std::fs::create_dir_all(&dir)?;
        Ok(Self {
            dir,
            clock: Arc::new(SystemClock::new()),
        })
    }

    fn path_for(&self, session_id: SessionId) -> PathBuf {
        self.dir.join(format!("{session_id}.json"))
    }

    /// Gate an incoming session-scoped RPC: reject `incoming == 0`
    /// (`Zero`) and `incoming < stored` (`Stale`); otherwise persist
    /// `max(stored, incoming)` and accept. Equal-epoch calls accept
    /// without a rewrite — one op makes many RPCs under one epoch.
    pub fn check_and_advance(
        &self,
        session_id: SessionId,
        incoming: u64,
    ) -> Result<(), EpochError> {
        if incoming == 0 {
            return Err(EpochError::Zero);
        }
        let stored = self.read(session_id)?.map_or(0, |r| r.epoch);
        if incoming < stored {
            return Err(EpochError::Stale { stored, incoming });
        }
        if incoming > stored {
            self.write_atomic(&EpochRecord {
                schema_version: EPOCH_SCHEMA_VERSION,
                session_id,
                epoch: incoming,
                updated_at: self.clock.now_utc(),
            })?;
        }
        Ok(())
    }

    /// The current record for `session_id`, if any. Unknown schema
    /// versions and unparseable files surface as errors — a corrupt
    /// record must fail the RPC loudly, not silently pass the fence.
    pub fn read(&self, session_id: SessionId) -> std::io::Result<Option<EpochRecord>> {
        let path = self.path_for(session_id);
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        let record: EpochRecord = serde_json::from_slice(&bytes).map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("session epoch record {}: {e}", path.display()),
            )
        })?;
        if record.schema_version != EPOCH_SCHEMA_VERSION {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "session epoch record {}: schema {} (want {EPOCH_SCHEMA_VERSION})",
                    path.display(),
                    record.schema_version
                ),
            ));
        }
        Ok(Some(record))
    }

    /// Remove the record. Called when the session is torn down for good
    /// (destroy) so the directory doesn't accumulate one file per
    /// session forever. Idempotent.
    pub fn remove(&self, session_id: SessionId) -> std::io::Result<()> {
        match std::fs::remove_file(self.path_for(session_id)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }

    fn write_atomic(&self, record: &EpochRecord) -> std::io::Result<()> {
        let final_path = self.path_for(record.session_id);
        // Writer-unique temp name (PR #437's lesson: fixed temp names
        // race across writers; include pid + the epoch).
        let tmp = self.dir.join(format!(
            ".{}.{}.{}.tmp",
            record.session_id,
            std::process::id(),
            record.epoch,
        ));
        std::fs::write(&tmp, serde_json::to_vec_pretty(record)?)?;
        std::fs::rename(&tmp, &final_path)?;
        Ok(())
    }
}

/// Ephemeral store for tests and in-process glue that never exercises
/// the fence. Backed by a temp dir so the production code path runs
/// unchanged.
pub fn ephemeral() -> SessionEpochStore {
    let dir = std::env::temp_dir().join(format!(
        "engram-session-epochs-{}",
        crate::time_source::unique_path_token()
    ));
    SessionEpochStore::open(dir).expect("ephemeral session epoch store")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> SessionEpochStore {
        let dir = tempfile::tempdir().expect("tempdir");
        let s = SessionEpochStore::open(dir.path().join("epochs")).expect("open");
        // Leak the tempdir guard so the dir outlives the test body's
        // store usage; the OS reaps temp dirs.
        std::mem::forget(dir);
        s
    }

    #[test]
    fn accepts_monotonic_epochs_and_repeats() {
        let s = store();
        let sid = SessionId::new();
        s.check_and_advance(sid, 1).expect("first epoch");
        s.check_and_advance(sid, 1)
            .expect("same epoch re-check (one op, many RPCs)");
        s.check_and_advance(sid, 3).expect("skip forward");
        assert_eq!(s.read(sid).unwrap().unwrap().epoch, 3);
    }

    #[test]
    fn rejects_stale_epoch_and_keeps_high_water() {
        let s = store();
        let sid = SessionId::new();
        s.check_and_advance(sid, 5).expect("advance to 5");
        let err = s
            .check_and_advance(sid, 4)
            .expect_err("stale must be refused");
        assert!(matches!(
            err,
            EpochError::Stale {
                stored: 5,
                incoming: 4
            }
        ));
        // High-water untouched by the rejected call.
        assert_eq!(s.read(sid).unwrap().unwrap().epoch, 5);
    }

    #[test]
    fn rejects_zero_epoch() {
        let s = store();
        let sid = SessionId::new();
        assert!(matches!(s.check_and_advance(sid, 0), Err(EpochError::Zero)));
        // Zero never creates a record.
        assert!(s.read(sid).expect("read").is_none());
    }

    #[test]
    fn high_water_persists_across_reopen() {
        // Simulated host-agent restart: a second store over the SAME
        // directory must still fence the stale predecessor.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("epochs");
        let first = SessionEpochStore::open(&path).expect("open first");
        let sid = SessionId::new();
        first.check_and_advance(sid, 7).expect("advance");
        drop(first);

        let reopened = SessionEpochStore::open(&path).expect("re-open");
        assert!(matches!(
            reopened.check_and_advance(sid, 6),
            Err(EpochError::Stale {
                stored: 7,
                incoming: 6
            })
        ));
        reopened
            .check_and_advance(sid, 8)
            .expect("newer epoch accepted");
        assert_eq!(reopened.read(sid).unwrap().unwrap().epoch, 8);
    }

    #[test]
    fn remove_is_idempotent() {
        let s = store();
        let sid = SessionId::new();
        s.check_and_advance(sid, 2).expect("advance");
        s.remove(sid).expect("remove");
        assert!(s.read(sid).expect("read").is_none());
        s.remove(sid).expect("second remove is a no-op");
    }

    #[test]
    fn corrupt_record_reads_as_error_not_silent_none() {
        let s = store();
        let sid = SessionId::new();
        std::fs::write(s.path_for(sid), b"{not json").expect("write garbage");
        assert!(s.read(sid).is_err());
        // And the gate fails loudly rather than treating it as epoch 0.
        assert!(matches!(
            s.check_and_advance(sid, 1),
            Err(EpochError::Io(_))
        ));
    }
}
