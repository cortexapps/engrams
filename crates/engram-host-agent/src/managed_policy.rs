//! Durable managed policy application. It does not start a guest.

use crate::{bindings::BindingStore, egress::HostEgress};
use engram_core::{
    traits::SandboxBackend, types::managed_deny::*, HostId, SandboxError, SessionId,
};
use serde::{Deserialize, Serialize};
use std::{io::Write, sync::Arc};

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct ManagedRecord {
    version: u32,
    context: ManagedPolicyContext,
    pub(crate) egress: engram_core::types::egress::SessionEgressPolicy,
    applied: Option<ManagedAppliedState>,
    expected: Option<ManagedAppliedState>,
    accepted_host_incarnation: String,
    accepted_proxy_incarnation: String,
}
fn invalid(e: impl std::fmt::Display) -> SandboxError {
    SandboxError::InvalidSpec(e.to_string())
}
fn io_invalid(e: impl std::fmt::Display) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string())
}
fn write_atomic(
    store: &BindingStore,
    sid: SessionId,
    suffix: &str,
    bytes: &[u8],
) -> std::io::Result<()> {
    let path = store.directory().join(format!("{sid}.{suffix}"));
    let temp = path.with_extension(format!("{suffix}.partial"));
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts.open(&temp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    std::fs::rename(temp, path)?;
    std::fs::File::open(store.directory())?.sync_all()
}
fn persist(store: &BindingStore, record: &ManagedRecord) -> std::io::Result<()> {
    let sid = record.context.target.session_id;
    // This completeness marker makes a missing record distinguishable from no policy.
    write_atomic(store, sid, "managed-required", b"1")?;
    let body = serde_json::to_string(record).map_err(io_invalid)?;
    write_atomic(
        store,
        sid,
        "managed",
        &crate::durable_envelope::seal(&sid.to_string(), &body),
    )
}
pub(crate) fn read_record(
    store: &BindingStore,
    sid: SessionId,
) -> std::io::Result<Option<ManagedRecord>> {
    let path = store.directory().join(format!("{sid}.managed"));
    let bytes = match std::fs::read(path) {
        Ok(v) => v,
        Err(e)
            if e.kind() == std::io::ErrorKind::NotFound
                && !store
                    .directory()
                    .join(format!("{sid}.managed-required"))
                    .exists() =>
        {
            return Ok(None)
        }
        Err(e) => return Err(e),
    };
    if std::fs::read(store.directory().join(format!("{sid}.managed-required")))? != b"1" {
        return Err(io_invalid("invalid managed completeness marker"));
    }
    let body = crate::durable_envelope::open(&bytes, &sid.to_string()).map_err(io_invalid)?;
    let record: ManagedRecord = serde_json::from_slice(&body).map_err(io_invalid)?;
    record.context.validate().map_err(io_invalid)?;
    if record.version != 1
        || record.context.target.session_id != sid
        || record.egress.session_id != sid
        || record.egress.sandbox_id != record.context.target.sandbox_id
    {
        return Err(io_invalid("invalid managed record identity"));
    }
    record
        .egress
        .managed_deny
        .as_ref()
        .ok_or_else(|| io_invalid("missing managed deny"))?
        .validate()
        .map_err(io_invalid)?;
    if let Some(applied) = &record.applied {
        applied.validate().map_err(io_invalid)?;
        if applied.context != record.context
            || Some(&applied.policy) != record.egress.managed_deny.as_ref()
        {
            return Err(io_invalid(
                "managed applied marker differs from desired policy",
            ));
        }
    }
    Ok(Some(record))
}

/// Exclusive ownership of this directory. The stable lock file is also the
/// durable marker that prevents a later plaintext host from reopening it.
pub struct ManagedOwnership {
    _file: std::fs::File,
    directory: std::path::PathBuf,
}
impl ManagedOwnership {
    pub fn for_mode(
        directory: &std::path::Path,
        managed: bool,
    ) -> std::io::Result<Option<Arc<Self>>> {
        let path = directory.join("managed-owner.lock");
        if !managed {
            if path.exists() {
                return Err(io_invalid(
                    "managed host directory requires authenticated mode",
                ));
            }
            return Ok(None);
        }
        std::fs::create_dir_all(directory)?;
        let mut options = std::fs::OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600).custom_flags(libc::O_CLOEXEC);
        }
        let file = options.open(path)?;
        file.try_lock().map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                "managed host directory is already owned",
            )
        })?;
        file.sync_all()?;
        std::fs::File::open(directory)?.sync_all()?;
        Ok(Some(Arc::new(Self {
            _file: file,
            directory: directory.canonicalize()?,
        })))
    }
}

pub struct ManagedPolicyHost {
    _ownership: Arc<ManagedOwnership>,
    host_id: HostId,
    incarnation: String,
    bindings: BindingStore,
    backend: Arc<dyn SandboxBackend>,
    egress: Arc<HostEgress>,
    locks: dashmap::DashMap<SessionId, Arc<tokio::sync::Mutex<()>>>,
}
impl ManagedPolicyHost {
    pub fn new(
        host_id: HostId,
        incarnation: String,
        bindings: BindingStore,
        backend: Arc<dyn SandboxBackend>,
        egress: Arc<HostEgress>,
        ownership: Arc<ManagedOwnership>,
    ) -> std::io::Result<Self> {
        if ownership.directory != bindings.directory().canonicalize()? {
            return Err(io_invalid("managed ownership directory mismatch"));
        }
        Ok(Self {
            _ownership: ownership,
            host_id,
            incarnation,
            bindings,
            backend,
            egress,
            locks: dashmap::DashMap::new(),
        })
    }
    /// Quarantine trusted proxy registrations before reading any durable policy.
    /// Ordinary replay must run only after this succeeds.
    pub async fn recover_all(&self) -> Result<(), SandboxError> {
        let handle = self.egress.handle();
        let known: std::collections::BTreeSet<_> = handle
            .managed_sessions()
            .await
            .map_err(invalid)?
            .into_iter()
            .collect();
        for sid in &known {
            handle.quarantine_managed(*sid).await.map_err(invalid)?;
        }
        let mut sessions = known.clone();
        for entry in std::fs::read_dir(self.bindings.directory()).map_err(invalid)? {
            let path = entry.map_err(invalid)?.path();
            if matches!(
                path.extension().and_then(|s| s.to_str()),
                Some("managed" | "managed-required")
            ) {
                sessions.insert(
                    path.file_stem()
                        .and_then(|s| s.to_str())
                        .ok_or_else(|| invalid("invalid managed filename"))?
                        .parse::<SessionId>()
                        .map_err(invalid)?,
                );
            }
        }
        for sid in &sessions {
            handle.quarantine_managed(*sid).await.map_err(invalid)?;
        }
        // Validate the complete set before reopening any session.
        let mut records = Vec::new();
        for sid in &sessions {
            let sid = *sid;
            let record = read_record(&self.bindings, sid)
                .map_err(invalid)?
                .ok_or_else(|| invalid("managed record missing"))?;
            if !known.contains(&sid) && self.bindings.read(sid).map_err(invalid)?.is_none() {
                continue;
            }
            self.check_record(&record).await?;
            records.push(record);
        }
        for record in records {
            if let Err(error) = self.read_applied(&record.context.target).await {
                for sid in &sessions {
                    handle.quarantine_managed(*sid).await.map_err(invalid)?;
                }
                return Err(error);
            }
        }
        Ok(())
    }
    async fn check_target(&self, target: &ManagedPolicyTarget) -> Result<(), SandboxError> {
        if target.host_id != self.host_id {
            return Err(invalid("managed host identity mismatch"));
        }
        let binding = self
            .bindings
            .read(target.session_id)
            .map_err(invalid)?
            .ok_or_else(|| invalid("managed session binding absent"))?;
        if binding.sandbox_id != target.sandbox_id || binding.binding_epoch != target.binding_epoch
        {
            return Err(invalid("managed physical binding changed"));
        }
        let endpoint = self
            .backend
            .guest_endpoints(target.sandbox_id)
            .await
            .ok_or_else(|| invalid("managed guest identity absent"))?;
        if endpoint.egress_identity.is_unspecified() {
            return Err(invalid("managed guest identity absent"));
        }
        Ok(())
    }
    async fn check_record(
        &self,
        record: &ManagedRecord,
    ) -> Result<Option<ManagedNetworkProof>, SandboxError> {
        self.check_target(&record.context.target).await?;
        let endpoint = self
            .backend
            .guest_endpoints(record.context.target.sandbox_id)
            .await
            .ok_or_else(|| invalid("managed guest identity absent"))?;
        if endpoint.egress_identity != record.egress.guest_ip {
            return Err(invalid("managed guest identity mismatch"));
        }
        if !record.egress.tunnels.is_empty() {
            return Err(invalid("managed policy does not support tunnels"));
        }
        let proof = self
            .backend
            .managed_network_proof(&record.context.target)
            .await;
        if record.context.binding_hash.is_some() && proof.is_none() {
            return Err(invalid("strict managed network capability unavailable"));
        }
        if proof.as_ref().is_some_and(|p| {
            p.target != record.context.target
                || p.guest_ip != record.egress.guest_ip
                || !is_digest(&p.network_identity)
        }) {
            return Err(invalid("strict managed network identity mismatch"));
        }
        Ok(proof)
    }
    async fn replay(&self, mut record: ManagedRecord) -> Result<ManagedAppliedState, SandboxError> {
        let proof = self.check_record(&record).await?;
        let sid = record.context.target.session_id;
        let proxy = self
            .egress
            .handle()
            .read_managed(sid)
            .await
            .map_err(invalid)?;
        let matches = proxy.as_ref().is_some_and(|p| {
            p.sandbox_id == record.egress.sandbox_id
                && p.guest_ip == record.egress.guest_ip
                && Some(&p.policy) == record.egress.managed_deny.as_ref()
        });
        if let Some(applied) = &record.applied {
            if matches
                && applied.network_proof == proof
                && applied.host_incarnation == self.incarnation
                && proxy
                    .as_ref()
                    .is_some_and(|p| p.incarnation == applied.proxy_incarnation)
            {
                return Ok(applied.clone());
            }
        }
        self.backend
            .notify_session_policy(record.egress.clone())
            .await?;
        let actual = self
            .egress
            .handle()
            .read_managed(sid)
            .await
            .map_err(invalid)?
            .ok_or_else(|| invalid("managed policy not installed"))?;
        if actual.sandbox_id != record.egress.sandbox_id
            || actual.guest_ip != record.egress.guest_ip
            || Some(&actual.policy) != record.egress.managed_deny.as_ref()
        {
            return Err(invalid("managed proxy acknowledgement mismatch"));
        }
        self.check_target(&record.context.target).await?;
        let applied = ManagedAppliedState::new(
            record.context.clone(),
            actual.policy,
            self.incarnation.clone(),
            actual.incarnation,
            proof,
        )
        .map_err(invalid)?;
        record.applied = Some(applied.clone());
        persist(&self.bindings, &record).map_err(invalid)?;
        Ok(applied)
    }
    async fn read_inner(
        &self,
        target: &ManagedPolicyTarget,
    ) -> Result<Option<ManagedAppliedState>, SandboxError> {
        self.check_target(target).await?;
        match read_record(&self.bindings, target.session_id).map_err(invalid)? {
            Some(record) => {
                if &record.context.target != target {
                    return Err(invalid("managed policy belongs to another placement"));
                }
                self.replay(record).await.map(Some)
            }
            None => Ok(None),
        }
    }
    pub async fn read_applied(
        &self,
        target: &ManagedPolicyTarget,
    ) -> Result<Option<ManagedAppliedState>, SandboxError> {
        let lock = self.locks.entry(target.session_id).or_default().clone();
        let _guard = lock.lock().await;
        let result = self.read_inner(target).await;
        if result.is_err() {
            let _ = self
                .egress
                .handle()
                .quarantine_managed(target.session_id)
                .await;
        }
        result
    }
    pub async fn compare_and_apply(
        &self,
        request: ManagedPolicyApply,
    ) -> Result<ManagedAppliedState, SandboxError> {
        request.context.validate().map_err(invalid)?;
        if !request.egress.tunnels.is_empty() {
            return Err(invalid("managed policy does not support tunnels"));
        }
        let target = request.context.target.clone();
        let lock = self.locks.entry(target.session_id).or_default().clone();
        let _guard = lock.lock().await;
        self.check_target(&target).await?;
        let policy = request
            .egress
            .managed_deny
            .as_ref()
            .ok_or_else(|| invalid("managed policy missing"))?;
        policy.validate().map_err(invalid)?;
        let endpoint = self
            .backend
            .guest_endpoints(target.sandbox_id)
            .await
            .ok_or_else(|| invalid("managed guest identity absent"))?;
        if request.egress.session_id != target.session_id
            || request.egress.sandbox_id != target.sandbox_id
            || request.egress.guest_ip != endpoint.egress_identity
        {
            return Err(invalid("managed guest identity mismatch"));
        }
        if let Some(expected) = &request.expected {
            expected.validate().map_err(invalid)?;
        }
        let current = match self.read_inner(&target).await {
            Ok(value) => value,
            Err(error) => {
                let _ = self
                    .egress
                    .handle()
                    .quarantine_managed(target.session_id)
                    .await;
                return Err(error);
            }
        };
        if let Some(state) = &current {
            // An exact lost-response retry has already committed this candidate.
            if state.context == request.context && &state.policy == policy {
                let record = read_record(&self.bindings, target.session_id)
                    .map_err(invalid)?
                    .ok_or_else(|| invalid("managed record absent"))?;
                if request.expected == current
                    || (request.expected == record.expected
                        && state.host_incarnation == record.accepted_host_incarnation
                        && state.proxy_incarnation == record.accepted_proxy_incarnation)
                {
                    return Ok(state.clone());
                }
                return Err(invalid("managed expected state changed"));
            }
        }
        if current != request.expected {
            return Err(invalid("managed expected state changed"));
        }
        if let Some(old) = &current {
            if policy.revision <= old.policy.revision
                || !policy.contains(&old.policy)
                || (request.context.operator_generation < old.context.operator_generation
                    || (request.context.operator_generation == old.context.operator_generation
                        && request.context.operator_identity != old.context.operator_identity))
            {
                return Err(invalid("managed policy regressed"));
            }
        }
        let proxy_incarnation = self
            .egress
            .handle()
            .hello()
            .await
            .map_err(invalid)?
            .incarnation;
        let record = ManagedRecord {
            accepted_host_incarnation: self.incarnation.clone(),
            accepted_proxy_incarnation: proxy_incarnation,
            version: 1,
            context: request.context,
            egress: request.egress,
            applied: None,
            expected: request.expected,
        };
        persist(&self.bindings, &record).map_err(invalid)?;
        match self.replay(record).await {
            Ok(state) => Ok(state),
            Err(error) => {
                let _ = self
                    .egress
                    .handle()
                    .quarantine_managed(target.session_id)
                    .await;
                Err(error)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn torn_managed_record_never_becomes_absent() {
        let dir = tempfile::tempdir().unwrap();
        let sid = engram_core::SessionId::new();
        let store = crate::bindings::BindingStore::open(dir.path()).unwrap();
        let target = ManagedPolicyTarget {
            session_id: sid,
            sandbox_id: engram_core::SandboxId::new(),
            host_id: HostId::new(),
            binding_epoch: 1,
        };
        let record = ManagedRecord {
            version: 1,
            context: ManagedPolicyContext {
                target: target.clone(),
                operator_generation: 1,
                operator_identity: format!("sha256:{}", "a".repeat(64)),
                binding_hash: None,
            },
            egress: super::live_tests::egress(&target, 1, &["mcp.example"]),
            applied: None,
            expected: None,
            accepted_host_incarnation: "host-a".into(),
            accepted_proxy_incarnation: "proxy-a".into(),
        };
        let bytes = crate::durable_envelope::seal(
            &sid.to_string(),
            &serde_json::to_string(&record).unwrap(),
        );
        let marker = dir.path().join(format!("{sid}.managed-required"));
        let path = dir.path().join(format!("{sid}.managed"));
        std::fs::write(&marker, b"1").unwrap();
        assert!(read_record(&store, sid).is_err());
        for end in 0..bytes.len() {
            std::fs::write(&path, &bytes[..end]).unwrap();
            assert!(read_record(&store, sid).is_err(), "offset {end}");
        }
        std::fs::write(&path, &bytes).unwrap();
        assert!(read_record(&store, sid).unwrap().is_some());
        std::fs::write(
            dir.path().join(format!("{sid}.managed.partial")),
            b"garbage",
        )
        .unwrap();
        assert!(
            read_record(&store, sid).unwrap().is_some(),
            "incomplete sibling cannot replace committed policy"
        );
        std::fs::remove_file(&marker).unwrap();
        assert!(
            read_record(&store, sid).is_err(),
            "missing completeness marker cannot be accepted"
        );
    }
}

#[cfg(test)]
mod live_tests {
    use super::*;
    use engram_core::{
        types::{
            egress::SessionEgressPolicy,
            endpoints::GuestEndpoints,
            image::SecretMode,
            sandbox::{ExecRequest, ExecStream, SandboxSpec},
            snapshot::SnapshotMetadata,
        },
        SandboxId, SnapshotId,
    };
    struct EndpointFixture {
        egress: Arc<HostEgress>,
    }
    #[async_trait::async_trait]
    impl SandboxBackend for EndpointFixture {
        async fn create(&self, _: SandboxSpec) -> Result<SandboxId, SandboxError> {
            Err(invalid("unused fixture"))
        }
        async fn exec_stream(
            &self,
            _: SandboxId,
            _: ExecRequest,
        ) -> Result<ExecStream, SandboxError> {
            Err(invalid("unused fixture"))
        }
        async fn snapshot(&self, _: SandboxId) -> Result<SnapshotMetadata, SandboxError> {
            Err(invalid("unused fixture"))
        }
        fn snapshot_path_for(&self, _: SnapshotId) -> std::path::PathBuf {
            std::path::PathBuf::new()
        }
        async fn restore(&self, _: SnapshotMetadata) -> Result<SandboxId, SandboxError> {
            Err(invalid("unused fixture"))
        }
        async fn destroy(&self, _: SandboxId) -> Result<(), SandboxError> {
            Ok(())
        }
        async fn list(&self) -> Result<Vec<SandboxId>, SandboxError> {
            Ok(Vec::new())
        }
        async fn guest_endpoints(&self, _: SandboxId) -> Option<GuestEndpoints> {
            Some(GuestEndpoints {
                egress_identity: "10.200.0.42".parse().unwrap(),
                dial_ip: "10.200.0.42".parse().unwrap(),
                netns: None,
                vsock_uds: None,
            })
        }
        async fn notify_session_policy(
            &self,
            policy: SessionEgressPolicy,
        ) -> Result<(), SandboxError> {
            self.egress.apply_policy(policy).await.map_err(invalid)
        }
    }
    pub(super) fn egress(
        target: &ManagedPolicyTarget,
        revision: u64,
        hosts: &[&str],
    ) -> SessionEgressPolicy {
        SessionEgressPolicy {
            session_id: target.session_id,
            sandbox_id: target.sandbox_id,
            guest_ip: "10.200.0.42".parse().unwrap(),
            network_allow_hosts: vec![],
            network_allow_host_patterns: vec![],
            allow_all: true,
            secrets: vec![],
            injects: vec![],
            observes: vec![],
            guest_services: vec![],
            tunnels: vec![],
            secret_mode: SecretMode::Literal,
            apps: vec![],
            managed_deny: Some(
                ManagedDenyPolicy::new(revision, hosts.iter().map(|s| s.to_string())).unwrap(),
            ),
        }
    }
    #[tokio::test]
    async fn durable_cas_replays_pending_union_and_fences_receipts() {
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            let temp = tempfile::tempdir().unwrap();
            let dir = temp.path();
            let ca = engram_egress_proxy::Ca::load_or_generate(&dir.join("ca")).unwrap();
            let host_id = HostId::new();
            let mut args = engram_egress_proxyd::DaemonArgs::new(
                dir.to_path_buf(),
                ca.cert_pem.clone(),
                ca.key_pair.serialize_pem(),
                "http://127.0.0.1:1".into(),
                None,
                host_id,
            );
            [args.proxy_port, args.dns_port, args.gateway_port] =
                engram_testkit::net::free_tcp_udp_ports();
            let daemon = tokio::spawn(engram_egress_proxyd::run(args));
            let handle = Arc::new(crate::proxyd_client::ProxydHandle::new(
                dir.join(engram_egress_proto::CONTROL_SOCK_NAME),
            ));
            loop {
                if handle.hello().await.is_ok() {
                    break;
                }
                assert!(!daemon.is_finished(), "daemon stopped");
                tokio::task::yield_now().await;
            }
            let egress_host = Arc::new(HostEgress::new(handle.clone(), ca.cert_pem, None));
            let backend = Arc::new(EndpointFixture {
                egress: egress_host.clone(),
            });
            let bindings = BindingStore::open(dir.join("bindings")).unwrap();
            let ownership = ManagedOwnership::for_mode(bindings.directory(), true)
                .unwrap()
                .unwrap();
            let target = ManagedPolicyTarget {
                session_id: SessionId::new(),
                sandbox_id: SandboxId::new(),
                host_id,
                binding_epoch: 1,
            };
            bindings
                .bind(target.session_id, target.sandbox_id, 1)
                .unwrap();
            let host = ManagedPolicyHost::new(
                host_id,
                "host-a".into(),
                bindings.clone(),
                backend.clone(),
                egress_host.clone(),
                ownership.clone(),
            )
            .unwrap();
            let context = ManagedPolicyContext {
                target: target.clone(),
                operator_generation: 1,
                operator_identity: format!("sha256:{}", "a".repeat(64)),
                binding_hash: None,
            };
            assert!(host.read_applied(&target).await.unwrap().is_none());
            let first = ManagedPolicyApply {
                expected: None,
                context: context.clone(),
                egress: egress(&target, 1, &["mcp.example"]),
            };
            let ack = host.compare_and_apply(first.clone()).await.unwrap();
            assert_eq!(host.compare_and_apply(first.clone()).await.unwrap(), ack);
            let mut bad = first.clone();
            bad.expected = Some(
                ManagedAppliedState::new(
                    context.clone(),
                    ack.policy.clone(),
                    "foreign".into(),
                    "foreign".into(),
                    None,
                )
                .unwrap(),
            );
            assert!(host.compare_and_apply(bad).await.is_err());
            assert_eq!(
                handle
                    .decide(first.egress.guest_ip, "MCP.EXAMPLE.")
                    .await
                    .unwrap()
                    .as_deref(),
                Some("reject")
            );
            let second = ManagedPolicyApply {
                expected: Some(ack.clone()),
                context: context.clone(),
                egress: egress(&target, 2, &["mcp.example", "alias.example"]),
            };
            let ack2 = host.compare_and_apply(second.clone()).await.unwrap();
            assert!(host.compare_and_apply(first).await.is_err());
            let mut shrink = second.clone();
            shrink.expected = Some(ack2.clone());
            shrink.egress = egress(&target, 3, &["mcp.example"]);
            assert!(host.compare_and_apply(shrink).await.is_err());
            // Construct the crash state: desired rename completed, proxy apply/ACK did not.
            let pending = ManagedRecord {
                version: 1,
                context: context.clone(),
                egress: egress(&target, 3, &["mcp.example", "alias.example", "new.example"]),
                applied: None,
                expected: Some(ack2.clone()),
                accepted_host_incarnation: "host-a".into(),
                accepted_proxy_incarnation: ack2.proxy_incarnation.clone(),
            };
            persist(&bindings, &pending).unwrap();
            let successor = ManagedPolicyHost::new(
                host_id,
                "host-b".into(),
                bindings.clone(),
                backend,
                egress_host,
                ownership,
            )
            .unwrap();
            successor.recover_all().await.unwrap();
            let recovered = successor.read_applied(&target).await.unwrap().unwrap();
            assert_eq!(recovered.policy.revision, 3);
            assert_eq!(recovered.host_incarnation, "host-b");
            assert_eq!(
                handle
                    .decide(pending.egress.guest_ip, "new.example")
                    .await
                    .unwrap()
                    .as_deref(),
                Some("reject")
            );
            let stale = ManagedPolicyApply {
                expected: Some(ack2),
                context: context.clone(),
                egress: pending.egress.clone(),
            };
            assert!(successor.compare_and_apply(stale).await.is_err());
            let current = ManagedPolicyApply {
                expected: Some(recovered.clone()),
                context,
                egress: pending.egress,
            };
            assert_eq!(
                successor.compare_and_apply(current.clone()).await.unwrap(),
                recovered
            );
            let record_path = bindings
                .directory()
                .join(format!("{}.managed", target.session_id));
            let valid_bytes = std::fs::read(&record_path).unwrap();
            std::fs::remove_file(&record_path).unwrap();
            assert!(successor.recover_all().await.is_err());
            assert!(handle
                .read_managed(target.session_id)
                .await
                .unwrap()
                .is_none());
            std::fs::write(&record_path, b"corrupt").unwrap();
            assert!(successor.recover_all().await.is_err());
            assert!(handle
                .read_managed(target.session_id)
                .await
                .unwrap()
                .is_none());
            std::fs::write(&record_path, valid_bytes).unwrap();
            successor.recover_all().await.unwrap();
            let mut final_request = current.clone();
            final_request.context.binding_hash = Some(format!("sha256:{}", "b".repeat(64)));
            final_request.egress =
                egress(&target, 4, &["mcp.example", "alias.example", "new.example"]);
            assert!(successor.compare_and_apply(final_request).await.is_err());
            assert!(successor.read_applied(&target).await.is_err());
            bindings
                .bind(target.session_id, target.sandbox_id, 2)
                .unwrap();
            assert!(successor.read_applied(&target).await.is_err());
            assert!(handle
                .read_managed(target.session_id)
                .await
                .unwrap()
                .is_none());
            std::fs::write(
                bindings
                    .directory()
                    .join(format!("{}.managed", target.session_id)),
                b"corrupt",
            )
            .unwrap();
            let mut next = target.clone();
            next.binding_epoch = 2;
            assert!(successor.read_applied(&next).await.is_err());
            let mut socket =
                tokio::net::UnixStream::connect(dir.join(engram_egress_proto::CONTROL_SOCK_NAME))
                    .await
                    .unwrap();
            engram_egress_proto::write_frame(&mut socket, &engram_egress_proto::ToProxyd::Shutdown)
                .await
                .unwrap();
            assert_eq!(daemon.await.unwrap(), 0);
        })
        .await
        .expect("bounded host policy test");
    }
}

#[cfg(test)]
mod ownership_tests {
    // Tests drive subprocesses; OS time is a bounded test input (ADR 0098 D1).
    #![allow(clippy::disallowed_methods)]
    use super::*;
    #[test]
    fn ownership_child() {
        let Some(dir) = std::env::var_os("ENGRAM_MANAGED_OWNERSHIP_TEST_DIR") else {
            return;
        };
        let path = std::path::PathBuf::from(dir);
        let _owner = ManagedOwnership::for_mode(&path, true).unwrap().unwrap();
        std::fs::write(path.join("test-ready"), b"ready").unwrap();
        loop {
            std::thread::park();
        }
    }
    #[test]
    fn subprocess_overlap_rejected_and_crash_releases_ownership() {
        let temp = tempfile::tempdir().unwrap();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "managed_policy::ownership_tests::ownership_child",
                "--nocapture",
            ])
            .env("ENGRAM_MANAGED_OWNERSHIP_TEST_DIR", temp.path())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !temp.path().join("test-ready").exists() {
            if std::time::Instant::now() >= deadline || child.try_wait().unwrap().is_some() {
                let _ = child.kill();
                let _ = child.wait();
                panic!("ownership child did not become ready");
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let rejected = ManagedOwnership::for_mode(temp.path(), true).is_err();
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(rejected);
        let _successor = ManagedOwnership::for_mode(temp.path(), true)
            .unwrap()
            .unwrap();
        assert!(
            ManagedOwnership::for_mode(temp.path(), false).is_err(),
            "plaintext mode cannot reopen a managed directory"
        );
    }
}
