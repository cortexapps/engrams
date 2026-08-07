//! Host-durable session→sandbox binding records (ADR 0073).
//!
//! One JSON file per session under the host-agent's `bindings_dir`
//! (a work_dir sibling that survives pod rolls). The hub validates
//! every harness attach against these records — never an in-memory
//! map — which is what closes the #447 host-roll orphan gap by
//! construction: the file survives the roll, so the survivor
//! harness's first re-dial validates with zero coordinator
//! involvement and zero rebuild pass.
//!
//! Monotonicity is the fencing property: [`BindingStore::bind`]
//! refuses to lower `binding_epoch`, so no matter how a resume race
//! interleaves its bind RPCs, the record converges to the newest
//! generation and every stale harness is rejected `Superseded`.
//!
//! Atomic write: temp file + rename in the same directory (the same
//! primitive as `sandbox_manifest.rs`); records are tiny and written
//! off the hot path (bind/unbind only), so synchronous `std::fs` is
//! fine. Node death loses the directory — that is the PG re-bind
//! path's job, not ours (ADR 0073 §Consequences).

use std::path::PathBuf;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use engram_core::traits::{Clock, SystemClock};
use engram_core::{SandboxId, SessionId};
use serde::{Deserialize, Serialize};

/// Bump when the on-disk shape changes incompatibly. Readers reject
/// unknown versions (fail loud, never guess).
const BINDING_SCHEMA_VERSION: u32 = 1;

/// The durable attach token, as persisted on the host.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct BindingRecord {
    pub schema_version: u32,
    pub session_id: SessionId,
    pub sandbox_id: SandboxId,
    pub binding_epoch: u64,
    pub bound_at: DateTime<Utc>,
}

/// Why a [`BindingStore::bind`] write was refused.
#[derive(Debug)]
pub enum BindError {
    /// The on-disk record carries a strictly newer epoch — the caller
    /// is acting on a stale view of the session. The record is left
    /// untouched.
    Stale {
        existing: u64,
        presented: u64,
    },
    Io(std::io::Error),
}

impl std::fmt::Display for BindError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BindError::Stale {
                existing,
                presented,
            } => write!(
                f,
                "stale bind: on-disk epoch {existing} > presented {presented}"
            ),
            BindError::Io(e) => write!(f, "binding io: {e}"),
        }
    }
}

impl std::error::Error for BindError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            BindError::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for BindError {
    fn from(e: std::io::Error) -> Self {
        BindError::Io(e)
    }
}

/// Directory-backed store. Cheap to clone.
#[derive(Clone, Debug)]
pub struct BindingStore {
    dir: PathBuf,
    /// ADR 0098 D1: wall clock is an injected world input. P1 wires the
    /// production clock; sim injection rides the flow-extraction PRs.
    clock: Arc<dyn Clock>,
}

impl BindingStore {
    /// Open (creating the directory if needed). The directory must be
    /// on storage that survives a host-agent restart for the ADR 0073
    /// guarantees to hold — the host-agent wires its work_dir.
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

    /// ADR 0111: the applied egress policy lives beside the binding
    /// record (`<session_id>.egress`), not inside it — `bind` callers
    /// do not carry a policy, and the policy writer must never race
    /// the record's monotonic-epoch logic. Extension is NOT `.json`
    /// so [`BindingStore::list`] never parses these files.
    fn policy_path_for(&self, session_id: SessionId) -> PathBuf {
        self.dir.join(format!("{session_id}.egress"))
    }

    /// Record (or refresh) the binding for `session_id`. Monotonic in
    /// `binding_epoch`: a lower epoch is refused `Stale`. An
    /// EQUAL-epoch write may re-point the sandbox — that is the live
    /// move shape (ADR 0045 C1: the harness process survives a
    /// teleport, so its generation is unchanged while the VM identity
    /// changes; the record follows the VM). Same-epoch writers are
    /// serialized upstream by the session lease.
    pub fn bind(
        &self,
        session_id: SessionId,
        sandbox_id: SandboxId,
        binding_epoch: u64,
    ) -> Result<BindingRecord, BindError> {
        if let Some(existing) = self.read(session_id)? {
            if existing.binding_epoch > binding_epoch {
                return Err(BindError::Stale {
                    existing: existing.binding_epoch,
                    presented: binding_epoch,
                });
            }
            if existing.binding_epoch == binding_epoch && existing.sandbox_id != sandbox_id {
                tracing::info!(
                    session_id = %session_id,
                    from = %existing.sandbox_id,
                    to = %sandbox_id,
                    binding_epoch,
                    "binding re-pointed at same epoch (live-move shape)",
                );
            }
        }
        let record = BindingRecord {
            schema_version: BINDING_SCHEMA_VERSION,
            session_id,
            sandbox_id,
            binding_epoch,
            bound_at: self.clock.now_utc(),
        };
        self.write_atomic(&record)?;
        Ok(record)
    }

    /// The current record for `session_id`, if any. Unknown schema
    /// versions and unparseable files surface as errors — a corrupt
    /// record must fail the attach loudly (`UnknownBinding` at the
    /// hub), not silently pass validation.
    pub fn read(&self, session_id: SessionId) -> std::io::Result<Option<BindingRecord>> {
        let path = self.path_for(session_id);
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        let record: BindingRecord = serde_json::from_slice(&bytes).map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("binding record {}: {e}", path.display()),
            )
        })?;
        if record.schema_version != BINDING_SCHEMA_VERSION {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "binding record {}: schema {} (want {BINDING_SCHEMA_VERSION})",
                    path.display(),
                    record.schema_version
                ),
            ));
        }
        Ok(Some(record))
    }

    /// Remove the record, returning what it was. Called on
    /// unbind/destroy so a late dial from the torn-down generation
    /// gets `UnknownBinding` (transient) rather than routing anywhere.
    /// The persisted egress policy shares the record's lifecycle and
    /// is removed with it (ADR 0111).
    pub fn unbind(&self, session_id: SessionId) -> std::io::Result<Option<BindingRecord>> {
        let prior = self.read(session_id).unwrap_or(None);
        if let Err(e) = std::fs::remove_file(self.policy_path_for(session_id)) {
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(%session_id, error = %e, "remove persisted egress policy failed");
            }
        }
        match std::fs::remove_file(self.path_for(session_id)) {
            Ok(()) => Ok(prior),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// ADR 0111: persist the applied egress policy so a restarted
    /// host-agent rebuilds its proxy registry from a local read. Full
    /// replace per apply — the same semantics as the in-memory
    /// `Registry::register`. Written BEFORE the `start_agent` ack, so
    /// an acked policy is on the node by the time any caller can
    /// observe it. Mode 0600: the file carries resolved secrets (the
    /// posture ADR 0111 states; the same disk already holds them in
    /// the sandbox spec sidecar).
    pub fn store_policy(
        &self,
        policy: &engram_core::types::egress::SessionEgressPolicy,
    ) -> std::io::Result<()> {
        let final_path = self.policy_path_for(policy.session_id);
        let tmp = self.dir.join(format!(
            ".{}.{}.egress.tmp",
            policy.session_id,
            std::process::id(),
        ));
        // The temp file is BORN 0600 (review finding on #992): creating
        // at the umask and tightening afterward leaves a window where
        // the resolved secrets are group/world-readable.
        {
            use std::io::Write as _;
            let mut open = std::fs::OpenOptions::new();
            open.write(true).create(true).truncate(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                open.mode(0o600);
            }
            let mut f = open.open(&tmp)?;
            f.write_all(&serde_json::to_vec(policy)?)?;
        }
        std::fs::rename(&tmp, &final_path)?;
        Ok(())
    }

    /// Every persisted egress policy on disk. Read once at startup by
    /// the registry rebuild pass; unparseable files are skipped with a
    /// warn (only reachable via node death, where no VM survives to
    /// want them — the ADR 0110 reboot argument).
    /// Reverse lookup: the binding record whose `sandbox_id` matches.
    /// #1003 ladder 4: the spec-based re-serve pass maps a surviving
    /// sandbox back to its session from host-local durable state (the
    /// coordinator's rehydrate list can be wrong or incomplete).
    /// Unparseable records are skipped with a warn — the pass treats a
    /// missing mapping as "unserved" and surfaces it on the gauge.
    pub fn find_by_sandbox(&self, sandbox_id: SandboxId) -> std::io::Result<Option<BindingRecord>> {
        for entry in std::fs::read_dir(&self.dir)? {
            let entry = entry?;
            if entry.path().extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            match std::fs::read(entry.path()).and_then(|b| {
                serde_json::from_slice::<BindingRecord>(&b).map_err(std::io::Error::from)
            }) {
                Ok(record) if record.sandbox_id == sandbox_id => return Ok(Some(record)),
                Ok(_) => {}
                Err(e) => {
                    tracing::warn!(
                        path = %entry.path().display(),
                        error = %e,
                        "skipping unparseable binding record in sandbox lookup",
                    );
                }
            }
        }
        Ok(None)
    }

    pub fn list_policies(
        &self,
    ) -> std::io::Result<Vec<engram_core::types::egress::SessionEgressPolicy>> {
        let mut out = Vec::new();
        for entry in std::fs::read_dir(&self.dir)? {
            let entry = entry?;
            if entry.path().extension().and_then(|e| e.to_str()) != Some("egress") {
                continue;
            }
            match std::fs::read(entry.path()).and_then(|bytes| {
                serde_json::from_slice::<engram_core::types::egress::SessionEgressPolicy>(&bytes)
                    .map_err(std::io::Error::from)
            }) {
                Ok(policy) => out.push(policy),
                Err(e) => {
                    tracing::warn!(
                        path = %entry.path().display(),
                        error = %e,
                        "skipping unparseable persisted egress policy",
                    );
                }
            }
        }
        Ok(out)
    }

    fn write_atomic(&self, record: &BindingRecord) -> std::io::Result<()> {
        let final_path = self.path_for(record.session_id);
        // Writer-unique temp name (PR #437's lesson: fixed temp names
        // race across writers; include pid + a counter).
        let tmp = self.dir.join(format!(
            ".{}.{}.{}.tmp",
            record.session_id,
            std::process::id(),
            record.binding_epoch,
        ));
        std::fs::write(&tmp, serde_json::to_vec_pretty(record)?)?;
        std::fs::rename(&tmp, &final_path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    // tests drive a live system; wall clock/OS entropy here is input, not a decision source (ADR 0098 D1)
    #![allow(clippy::disallowed_methods)]
    use super::*;

    fn store() -> BindingStore {
        let dir = tempfile::tempdir().expect("tempdir");
        let s = BindingStore::open(dir.path().join("bindings")).expect("open");
        // Leak the tempdir guard so the dir outlives the test body's
        // store usage; the OS reaps temp dirs.
        std::mem::forget(dir);
        s
    }

    #[test]
    fn bind_read_roundtrip() {
        let s = store();
        let (sid, sbx) = (SessionId::new(), SandboxId::new());
        let rec = s.bind(sid, sbx, 3).expect("bind");
        assert_eq!(rec.binding_epoch, 3);
        let read = s.read(sid).expect("read").expect("some");
        assert_eq!(read, rec);
    }

    #[test]
    fn bind_is_monotonic_in_epoch() {
        let s = store();
        let (sid, a, b) = (SessionId::new(), SandboxId::new(), SandboxId::new());
        s.bind(sid, a, 5).expect("bind@5");
        // Higher epoch wins regardless of arrival order…
        s.bind(sid, b, 6).expect("bind@6");
        // …and the stale generation is refused, leaving 6 in place.
        let err = s.bind(sid, a, 5).expect_err("stale bind must fail");
        assert!(matches!(
            err,
            BindError::Stale {
                existing: 6,
                presented: 5
            }
        ));
        assert_eq!(s.read(sid).unwrap().unwrap().sandbox_id, b);
    }

    #[test]
    fn same_epoch_write_refreshes_or_repoints() {
        let s = store();
        let (sid, sbx) = (SessionId::new(), SandboxId::new());
        s.bind(sid, sbx, 1).expect("first");
        s.bind(sid, sbx, 1).expect("refresh");
        // Live-move shape (ADR 0045 C1): the harness generation is
        // unchanged while the VM identity changes — the record follows.
        let moved = SandboxId::new();
        s.bind(sid, moved, 1).expect("same-epoch re-point");
        assert_eq!(s.read(sid).unwrap().unwrap().sandbox_id, moved);
    }

    #[test]
    fn unbind_removes_and_returns_prior() {
        let s = store();
        let (sid, sbx) = (SessionId::new(), SandboxId::new());
        s.bind(sid, sbx, 2).expect("bind");
        let prior = s.unbind(sid).expect("unbind").expect("prior");
        assert_eq!(prior.sandbox_id, sbx);
        assert!(s.read(sid).expect("read").is_none());
        assert!(s.unbind(sid).expect("second unbind").is_none());
    }

    #[test]
    fn corrupt_record_reads_as_error_not_silent_none() {
        let s = store();
        let sid = SessionId::new();
        std::fs::write(s.path_for(sid), b"{not json").expect("write garbage");
        assert!(s.read(sid).is_err());
    }

    fn policy_for(
        session_id: SessionId,
        sandbox_id: SandboxId,
    ) -> engram_core::types::egress::SessionEgressPolicy {
        engram_core::types::egress::SessionEgressPolicy {
            session_id,
            sandbox_id,
            guest_ip: std::net::Ipv4Addr::new(10, 200, 0, 2),
            network_allow_hosts: vec!["api.anthropic.com".into()],
            network_allow_host_patterns: Vec::new(),
            allow_all: false,
            secrets: Vec::new(),
            injects: Vec::new(),
            observes: Vec::new(),
            metadata_flavor: None,
            cloud_sql_tunnels: Vec::new(),
            secret_mode: engram_core::types::image::SecretMode::Broker,
        }
    }

    // ADR 0111: the persisted policy round-trips VERBATIM through a
    // fresh store on the same directory — the restart shape. The
    // rebuild pass replays exactly these bytes into the registry.
    #[test]
    fn policy_persists_across_a_fresh_store_open() {
        let s = store();
        let (sid, sbx) = (SessionId::new(), SandboxId::new());
        let policy = policy_for(sid, sbx);
        s.store_policy(&policy).expect("store policy");
        let reopened = BindingStore::open(s.dir.clone()).expect("reopen");
        let listed = reopened.list_policies().expect("list");
        assert_eq!(listed, vec![policy]);
    }

    // ADR 0111: a re-apply is a full replace (the same semantics as
    // the in-memory `Registry::register`).
    #[test]
    fn policy_store_is_full_replace() {
        let s = store();
        let (sid, sbx) = (SessionId::new(), SandboxId::new());
        s.store_policy(&policy_for(sid, sbx)).expect("first");
        let mut updated = policy_for(sid, sbx);
        updated.network_allow_hosts = vec!["github.com".into()];
        s.store_policy(&updated).expect("replace");
        assert_eq!(s.list_policies().expect("list"), vec![updated]);
    }

    // ADR 0111: the policy shares the binding record's lifecycle —
    // unbind removes both.
    #[test]
    fn unbind_removes_the_persisted_policy() {
        let s = store();
        let (sid, sbx) = (SessionId::new(), SandboxId::new());
        s.bind(sid, sbx, 1).expect("bind");
        s.store_policy(&policy_for(sid, sbx)).expect("store policy");
        s.unbind(sid).expect("unbind");
        assert!(s.list_policies().expect("list").is_empty());
        assert!(!s.policy_path_for(sid).exists());
    }

    // A torn/garbage policy file is skipped with a warn, never a
    // panic and never a wrong registration (node-death shape; no VM
    // survives to want it).
    #[test]
    fn garbage_policy_file_is_skipped_not_fatal() {
        let s = store();
        let (sid, sbx) = (SessionId::new(), SandboxId::new());
        s.store_policy(&policy_for(sid, sbx)).expect("store good");
        std::fs::write(s.dir.join(format!("{}.egress", SessionId::new())), b"{torn")
            .expect("write garbage");
        let listed = s.list_policies().expect("list");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].session_id, sid);
    }

    // Resolved secrets ride the file; it must not be group/world
    // readable (the ADR 0111 posture).
    #[cfg(unix)]
    #[test]
    fn policy_file_mode_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let s = store();
        let (sid, sbx) = (SessionId::new(), SandboxId::new());
        s.store_policy(&policy_for(sid, sbx)).expect("store");
        let mode = std::fs::metadata(s.policy_path_for(sid))
            .expect("metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}
