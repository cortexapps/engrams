//! One host owner for each managed boot. No ordinary backend map can adopt it.
use crate::{bindings::BindingStore, egress::HostEgress, harness::HarnessHub};
use engram_core::{
    traits::{HarnessSink, SandboxBackend},
    types::{
        egress::SessionEgressPolicy, image::SecretMode, managed_boot::*, managed_create::digest,
        managed_deny::*, manifest::ManifestRef, sandbox::AgentSpec,
    },
    HostId, SandboxError, SandboxId,
};
use engram_egress_proto::private::{Allocation, RegisteredPrivateNetwork};
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU32, Ordering},
        Arc,
    },
};
use tokio::sync::Mutex;

fn invalid(_: impl std::fmt::Display) -> SandboxError {
    SandboxError::InvalidSpec("managed boot ownership changed or unavailable".into())
}

#[derive(Clone)]
pub enum ManagedBackend {
    #[cfg(target_os = "linux")]
    Firecracker(Arc<engram_sandbox_firecracker::FirecrackerBackend>),
    #[cfg(target_os = "macos")]
    Vz {
        backend: Arc<engram_sandbox_vz::VzBackend>,
        helper: engram_sandbox_vz::ManagedHelperArtifact,
    },
}
impl ManagedBackend {
    async fn prepare(
        &self,
        request: &ManagedBootPrepare,
        spec: engram_core::types::sandbox::SandboxSpec,
        network: RegisteredPrivateNetwork,
    ) -> Result<Owner, SandboxError> {
        match self {
            #[cfg(target_os = "linux")]
            Self::Firecracker(backend) => Ok(Owner::Firecracker(
                backend
                    .prepare_managed_cold(request.cold.clone(), spec, network)
                    .await?,
            )),
            #[cfg(target_os = "macos")]
            Self::Vz { backend, helper } => Ok(Owner::Vz(
                backend
                    .prepare_managed_cold(request.cold.clone(), spec, network, helper.clone())
                    .await?,
            )),
        }
    }
}
enum Owner {
    #[cfg(target_os = "linux")]
    Firecracker(engram_sandbox_firecracker::managed::PreparedManagedFc),
    #[cfg(target_os = "macos")]
    Vz(engram_sandbox_vz::PreparedManagedVz),
}
impl Owner {
    async fn closed_identity(&mut self) -> Result<String, SandboxError> {
        let bytes = match self {
            #[cfg(target_os = "linux")]
            Self::Firecracker(owner) => serde_json::to_vec(&owner.closed_receipt().await?),
            #[cfg(target_os = "macos")]
            Self::Vz(owner) => serde_json::to_vec(&owner.closed_receipt().await?),
        }
        .map_err(invalid)?;
        Ok(digest(&bytes))
    }
    async fn release(&mut self, epoch: u64) -> Result<(), SandboxError> {
        match self {
            #[cfg(target_os = "linux")]
            Self::Firecracker(owner) => owner.release(epoch).await,
            #[cfg(target_os = "macos")]
            Self::Vz(owner) => owner.release(epoch).await,
        }
    }
    async fn ready(&mut self, epoch: u64) -> Result<(), SandboxError> {
        match self {
            #[cfg(target_os = "linux")]
            Self::Firecracker(owner) => owner.check_ready(epoch).await,
            #[cfg(target_os = "macos")]
            Self::Vz(owner) => owner.check_ready(epoch).await,
        }
    }
    async fn start(&mut self, agent: AgentSpec, sink: HarnessSink) -> Result<(), SandboxError> {
        match self {
            #[cfg(target_os = "linux")]
            Self::Firecracker(owner) => owner.start_agent(agent, sink).await,
            #[cfg(target_os = "macos")]
            Self::Vz(owner) => owner.start_agent(agent, sink).await,
        }
    }
    async fn retire_disk(&mut self, path: &Path) -> Result<(), SandboxError> {
        match self {
            #[cfg(target_os = "linux")]
            Self::Firecracker(owner) => owner.retire_disk(path).await,
            #[cfg(target_os = "macos")]
            Self::Vz(owner) => owner.retire_disk(path).await,
        }
    }
    async fn close(self) -> Result<(), SandboxError> {
        match self {
            #[cfg(target_os = "linux")]
            Self::Firecracker(owner) => owner.close().await,
            #[cfg(target_os = "macos")]
            Self::Vz(owner) => owner.close().await,
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct CaptureProgress {
    request: ManagedCaptureRequest,
    manifest: ManifestRef,
    /// Present only after retirement, durable file sync and exact hash read.
    artifact: Option<(String, u64)>,
    authority: Option<ManagedCaptureAuthority>,
}
#[derive(Clone, Serialize, Deserialize)]
struct Record {
    version: u32,
    request: ManagedBootPrepare,
    incarnation: String,
    state: ManagedBootHostState,
    allocation: Option<Allocation>,
    prepared: Option<ManagedBootPreparedReceipt>,
    applied: Option<ManagedAppliedState>,
    apply_digest: Option<String>,
    finalization: Option<String>,
    capture: Option<CaptureProgress>,
    /// Exact local disk retained after a live policy retirement, before upload.
    retired: Option<(String, u64)>,
}
struct Entry {
    record: Record,
    attach_owner: Option<crate::harness::ManagedHarnessOwner>,
    owner: Option<Owner>,
    agent: Option<AgentSpec>,
}

pub struct ManagedBootHostConfig {
    pub host_id: HostId,
    pub incarnation: String,
    pub directory: PathBuf,
}

pub struct ManagedBootHost {
    host_id: HostId,
    incarnation: String,
    backend: ManagedBackend,
    ordinary: Arc<dyn SandboxBackend>,
    egress: Arc<HostEgress>,
    hub: Arc<HarnessHub>,
    bindings: BindingStore,
    directory: PathBuf,
    chunks: engram_chunk_store::ChunkStore,
    next_network: AtomicU32,
    binding_lock: std::sync::Mutex<()>,
    entries: dashmap::DashMap<SandboxId, Arc<Mutex<Entry>>>,
}
impl ManagedBootHost {
    pub fn new(
        config: ManagedBootHostConfig,
        backend: ManagedBackend,
        ordinary: Arc<dyn SandboxBackend>,
        egress: Arc<HostEgress>,
        hub: Arc<HarnessHub>,
        chunks: engram_chunk_store::ChunkStore,
    ) -> Result<Self, SandboxError> {
        let ManagedBootHostConfig {
            host_id,
            incarnation,
            directory,
        } = config;
        use std::os::unix::fs::PermissionsExt;
        std::fs::create_dir_all(&directory)?;
        if !std::fs::symlink_metadata(&directory)?.is_dir() {
            return Err(invalid("managed directory"));
        }
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))?;
        let directory = directory.canonicalize()?;
        Ok(Self {
            host_id,
            incarnation,
            backend,
            ordinary,
            egress,
            bindings: hub.binding_store(),
            hub,
            directory,
            chunks,
            next_network: AtomicU32::new(0),
            binding_lock: std::sync::Mutex::new(()),
            entries: Default::default(),
        })
    }
    fn path(&self, id: SandboxId) -> PathBuf {
        self.directory.join(format!("{id}.boot"))
    }
    fn persist(&self, record: &Record) -> Result<(), SandboxError> {
        let id = record.request.key.context.target.sandbox_id;
        let bytes = serde_json::to_vec(record).map_err(invalid)?;
        durable_write(
            &self.path(id),
            &crate::durable_envelope::seal(
                &id.to_string(),
                std::str::from_utf8(&bytes).map_err(invalid)?,
            ),
        )?;
        Ok(())
    }
    fn read_record(&self, id: SandboxId) -> Result<Option<Record>, SandboxError> {
        let bytes = match std::fs::read(self.path(id)) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let bytes = crate::durable_envelope::open(&bytes, &id.to_string()).map_err(invalid)?;
        let record: Record = serde_json::from_slice(&bytes).map_err(invalid)?;
        record.request.spec().map_err(invalid)?;
        if record.version != 1
            || record.request.key.context.target.sandbox_id != id
            || record.incarnation.is_empty()
        {
            return Err(invalid("record"));
        }
        if let Some(prepared) = &record.prepared {
            prepared.validate().map_err(invalid)?;
            if prepared.request != record.request || prepared.host_incarnation != record.incarnation
            {
                return Err(invalid("prepared record"));
            }
        }
        if let Some(capture) = &record.capture {
            if capture.request.key != record.request.key
                || capture.manifest != capture.request.disk_manifest()
            {
                return Err(invalid("capture record"));
            }
            if let Some(authority) = &capture.authority {
                authority.validate().map_err(invalid)?;
                if authority.request != capture.request
                    || authority.host_incarnation != record.incarnation
                    || capture.artifact.as_ref()
                        != Some(&(authority.disk_sha256.clone(), authority.disk_size_bytes))
                {
                    return Err(invalid("capture record"));
                }
            }
        }
        Ok(Some(record))
    }
    fn entry(&self, key: &ManagedBootKey) -> Result<Arc<Mutex<Entry>>, SandboxError> {
        key.validate().map_err(invalid)?;
        if key.context.target.host_id != self.host_id {
            return Err(invalid("host"));
        }
        self.entries
            .get(&key.context.target.sandbox_id)
            .map(|e| e.clone())
            .ok_or_else(|| invalid("owner absent"))
    }
    fn check_key(&self, entry: &Entry, key: &ManagedBootKey) -> Result<(), SandboxError> {
        if &entry.record.request.key != key || entry.record.incarnation != self.incarnation {
            return Err(invalid("key"));
        }
        Ok(())
    }
    fn unbind_owned(&self, key: &ManagedBootKey) {
        let Ok(_guard) = self.binding_lock.lock() else {
            return;
        };
        if self
            .bindings
            .read(key.creation.session_id)
            .ok()
            .flatten()
            .is_some_and(|b| {
                b.sandbox_id == key.context.target.sandbox_id
                    && b.binding_epoch == key.context.target.binding_epoch
            })
        {
            self.hub.unbind_session(key.creation.session_id);
        }
    }
    async fn fail(&self, entry: &mut Entry) {
        entry.attach_owner = None;
        entry.record.state = ManagedBootHostState::Failed {
            key: entry.record.request.key.clone(),
        };
        entry.agent = None;
        let _ = self.persist(&entry.record);
        self.unbind_owned(&entry.record.request.key);
        if let Some(owner) = entry.owner.take() {
            let _ = owner.close().await;
        }
    }
    pub async fn prepare(
        self: &Arc<Self>,
        request: ManagedBootPrepare,
    ) -> Result<ManagedBootPreparedReceipt, SandboxError> {
        let this = self.clone();
        let result = tokio::spawn(async move {
            let key = request.key.clone();
            let had_owner = this.entries.contains_key(&key.context.target.sandbox_id);
            let result = this.prepare_inner(request).await;
            if result.is_ok() && !had_owner {
                let host = Arc::downgrade(&this);
                tokio::spawn(async move {
                    loop {
                        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                        let Some(host) = host.upgrade() else {
                            return;
                        };
                        match host.read(key.clone()).await {
                            Ok(
                                ManagedBootHostState::Failed { .. }
                                | ManagedBootHostState::Captured { .. }
                                | ManagedBootHostState::Retired { .. },
                            )
                            | Err(_) => return,
                            _ => (),
                        }
                    }
                });
            }
            result
        })
        .await
        .map_err(invalid)?;
        result
    }
    async fn prepare_inner(
        &self,
        request: ManagedBootPrepare,
    ) -> Result<ManagedBootPreparedReceipt, SandboxError> {
        let mut spec = request.spec().map_err(invalid)?;
        let key = &request.key;
        if key.context.target.host_id != self.host_id {
            return Err(invalid("host"));
        }
        let id = key.context.target.sandbox_id;
        let entry = match self.entries.entry(id) {
            dashmap::mapref::entry::Entry::Occupied(entry) => entry.get().clone(),
            dashmap::mapref::entry::Entry::Vacant(entry) => {
                if self.read_record(id)?.is_some()
                    || self
                        .bindings
                        .managed_sandbox_required(id)
                        .map_err(invalid)?
                {
                    return Err(invalid("lost owner"));
                }
                let record = Record {
                    version: 1,
                    request: request.clone(),
                    incarnation: self.incarnation.clone(),
                    state: ManagedBootHostState::Failed { key: key.clone() },
                    allocation: None,
                    prepared: None,
                    applied: None,
                    apply_digest: None,
                    finalization: None,
                    capture: None,
                    retired: None,
                };
                // Both markers precede backend I/O; a partial record is never ordinary.
                durable_write(
                    &self
                        .bindings
                        .directory()
                        .join(format!("{id}.managed-sandbox")),
                    b"1",
                )?;
                durable_write(
                    &self
                        .bindings
                        .directory()
                        .join(format!("{}.managed-boot", key.creation.session_id)),
                    id.to_string().as_bytes(),
                )?;
                self.persist(&record)?;
                entry
                    .insert(Arc::new(Mutex::new(Entry {
                        record,
                        attach_owner: None,
                        owner: None,
                        agent: None,
                    })))
                    .clone()
            }
        };
        let mut entry = entry.lock().await;
        self.check_key(&entry, key)?;
        if entry.record.request != request {
            return Err(invalid("retry changed"));
        }
        if let ManagedBootHostState::Prepared { receipt } = &entry.record.state {
            let receipt = receipt.as_ref().clone();
            if entry.record.applied.is_some() {
                entry
                    .owner
                    .as_mut()
                    .ok_or_else(|| invalid("owner"))?
                    .ready(key.context.target.binding_epoch)
                    .await?;
            } else if entry
                .owner
                .as_mut()
                .ok_or_else(|| invalid("owner"))?
                .closed_identity()
                .await?
                != receipt.network_identity
            {
                return Err(invalid("network"));
            }
            return Ok(receipt);
        }
        // None is allowed only for the first attempt, before any allocation exists.
        if entry.record.allocation.is_some() || entry.owner.is_some() {
            return Err(invalid("attempt consumed"));
        }
        if self.ordinary.list().await?.contains(&id)
            || self.hub.bound_sandbox(key.creation.session_id).is_some()
        {
            return Err(invalid("ordinary source"));
        }
        let result = async {
            let work = self.directory.join(id.to_string());
            std::fs::create_dir(&work)?;
            let rootfs = work.join("selected.ext4");
            let manifest = self
                .chunks
                .get_manifest(request.cold.disk.manifest)
                .await
                .map_err(invalid)?;
            self.chunks
                .materialize_to_file(&manifest, &rootfs)
                .await
                .map_err(invalid)?;
            spec.rootfs_source = Some(rootfs);
            let handle = self.egress.handle();
            let (guest_ip, gateway_ip, generation) = loop {
                let slot = self.next_network.fetch_add(1, Ordering::SeqCst);
                if slot >= 512 {
                    return Err(invalid("allocation budget"));
                }
                let second = 18 + (slot / 256) as u8;
                let third = (slot % 256) as u8;
                let guest = std::net::Ipv4Addr::new(198, second, third, 2);
                if handle.lookup_guest(guest).await.map_err(invalid)?.is_none() {
                    break (
                        guest,
                        std::net::Ipv4Addr::new(198, second, third, 1),
                        u64::from(slot) + 1,
                    );
                }
            };
            let allocation = Allocation {
                version: 1,
                host_id: self.host_id,
                host_incarnation: self.incarnation.clone(),
                proxy_incarnation: handle.hello().await.map_err(invalid)?.incarnation,
                session_id: key.creation.session_id,
                sandbox_id: id,
                boot_attempt: key.boot_attempt.clone(),
                attachment_generation: generation,
                guest_ip,
                gateway_ip,
                policy_revision: request.bootstrap_policy.revision,
                policy_digest: request.bootstrap_policy.digest.clone(),
            };
            entry.record.allocation = Some(allocation.clone());
            self.persist(&entry.record)?;
            self.egress
                .apply_policy(bootstrap(&allocation, request.bootstrap_policy.clone()))
                .await
                .map_err(invalid)?;
            let network = handle
                .register_private_network(&allocation)
                .await
                .map_err(invalid)?;
            let mut owner = self.backend.prepare(&request, spec, network).await?;
            let network_identity = owner.closed_identity().await?;
            let mut receipt = ManagedBootPreparedReceipt {
                request: request.clone(),
                host_incarnation: self.incarnation.clone(),
                network_identity,
                guest_ip,
                receipt_id: String::new(),
            };
            receipt.receipt_id = digest(&serde_json::to_vec(&receipt).map_err(invalid)?);
            receipt.validate().map_err(invalid)?;
            entry.owner = Some(owner);
            entry.record.prepared = Some(receipt.clone());
            entry.record.state = ManagedBootHostState::Prepared {
                receipt: Box::new(receipt.clone()),
            };
            self.persist(&entry.record)?;
            Ok(receipt)
        }
        .await;
        if result.is_err() {
            self.fail(&mut entry).await;
        }
        result
    }
    pub async fn read(&self, key: ManagedBootKey) -> Result<ManagedBootHostState, SandboxError> {
        key.validate().map_err(invalid)?;
        if key.context.target.host_id != self.host_id {
            return Err(invalid("host"));
        }
        if let Ok(entry) = self.entry(&key) {
            let mut entry = entry.lock().await;
            if entry.record.request.key != key {
                return Err(invalid("key"));
            }
            if let ManagedBootHostState::Captured { authority } = &entry.record.state {
                authority.validate().map_err(invalid)?;
                return Ok(entry.record.state.clone());
            }
            if matches!(entry.record.state, ManagedBootHostState::Retired { .. }) {
                self.verify_retired(&entry.record).await?;
                return Ok(entry.record.state.clone());
            }
            self.check_key(&entry, &key)?;
            let live = match &entry.record.state {
                ManagedBootHostState::Prepared { .. } if entry.record.applied.is_none() => entry
                    .owner
                    .as_mut()
                    .ok_or_else(|| invalid("owner"))?
                    .closed_identity()
                    .await
                    .map(|_| ()),
                ManagedBootHostState::Prepared { .. }
                | ManagedBootHostState::Finalized { .. }
                | ManagedBootHostState::Started { .. } => {
                    entry
                        .owner
                        .as_mut()
                        .ok_or_else(|| invalid("owner"))?
                        .ready(key.context.target.binding_epoch)
                        .await
                }
                _ => Ok(()),
            };
            if live.is_err() {
                self.fail(&mut entry).await;
            }
            return Ok(entry.record.state.clone());
        }
        match self.read_record(key.context.target.sandbox_id)? {
            Some(record) if record.request.key == key => {
                if matches!(record.state, ManagedBootHostState::Retired { .. }) {
                    self.verify_retired(&record).await?;
                    return Ok(record.state);
                }
                if let Some(authority) = record.capture.and_then(|c| c.authority) {
                    authority.validate().map_err(invalid)?;
                    return Ok(ManagedBootHostState::Captured { authority });
                }
                Ok(ManagedBootHostState::Failed { key })
            }
            Some(_) => Err(invalid("key")),
            None => Ok(ManagedBootHostState::Missing { key }),
        }
    }
    pub async fn apply(
        self: &Arc<Self>,
        request: ManagedPolicyApply,
    ) -> Result<ManagedAppliedState, SandboxError> {
        let this = self.clone();
        tokio::spawn(async move { this.apply_inner(request).await })
            .await
            .map_err(invalid)?
    }
    async fn apply_inner(
        &self,
        request: ManagedPolicyApply,
    ) -> Result<ManagedAppliedState, SandboxError> {
        let id = request.context.target.sandbox_id;
        let entry = self
            .entries
            .get(&id)
            .map(|e| e.clone())
            .ok_or_else(|| invalid("owner"))?;
        let mut entry = entry.lock().await;
        let key = entry.record.request.key.clone();
        self.check_key(&entry, &key)?;
        request.context.validate().map_err(invalid)?;
        if request.context.target != key.context.target {
            return Err(invalid("context"));
        }
        // Retirement is permanent. A newer coordinator deny union must not
        // erase this exact nonrunning proof or manufacture an applied ACK.
        if matches!(entry.record.state, ManagedBootHostState::Retired { .. }) {
            return Err(SandboxError::ManagedRecoveryRequired);
        }
        let context = request.context.clone();
        let allocation = entry
            .record
            .allocation
            .clone()
            .ok_or_else(|| invalid("allocation"))?;
        let policy = request
            .egress
            .managed_deny
            .as_ref()
            .ok_or_else(|| invalid("policy"))?;
        policy.validate().map_err(invalid)?;
        if request.egress.session_id != allocation.session_id
            || request.egress.sandbox_id != id
            || request.egress.guest_ip != allocation.guest_ip
            || !request.egress.tunnels.is_empty()
        {
            return Err(invalid("policy identity"));
        }
        if context.binding_hash.is_none() {
            if !bootstrap_retirement_allowed(&entry.record, &request) {
                return Err(invalid("bootstrap retirement context"));
            }
            if let Err(error) = self.retire_for_policy(&mut entry, policy.clone()).await {
                self.fail(&mut entry).await;
                return Err(error);
            }
            self.egress
                .handle()
                .quarantine_managed(allocation.session_id)
                .await
                .map_err(invalid)?;
            return Err(SandboxError::ManagedRecoveryRequired);
        }
        let request_digest = digest(&serde_json::to_vec(&request).map_err(invalid)?);
        if let Some(applied) = &entry.record.applied {
            let applied = applied.clone();
            if entry.record.apply_digest.as_ref() != Some(&request_digest) {
                if request.expected.as_ref() != Some(&applied)
                    || policy.revision <= applied.policy.revision
                    || !policy.contains(&applied.policy)
                    || context.operator_generation < applied.context.operator_generation
                    || (context.operator_generation == applied.context.operator_generation
                        && context.operator_identity != applied.context.operator_identity)
                {
                    return Err(invalid("policy retry"));
                }
                // A bound allocation cannot transfer its lease. Retire the old
                // physical context; recovery must claim a fresh closed owner.
                if let Err(error) = self.retire_for_policy(&mut entry, policy.clone()).await {
                    self.fail(&mut entry).await;
                    return Err(error);
                }
                self.egress
                    .handle()
                    .quarantine_managed(allocation.session_id)
                    .await
                    .map_err(invalid)?;
                return Err(SandboxError::ManagedRecoveryRequired);
            }
            entry
                .owner
                .as_mut()
                .ok_or_else(|| invalid("owner"))?
                .ready(key.context.target.binding_epoch)
                .await?;
            return Ok(applied);
        }
        let prepared = match &entry.record.state {
            ManagedBootHostState::Prepared { receipt } => receipt.clone(),
            _ => return Err(invalid("phase")),
        };
        let mut bootstrap_context = key.context.clone();
        bootstrap_context.binding_hash = context.binding_hash.clone();
        if request.expected.is_some() || context != bootstrap_context {
            return Err(invalid("expected policy"));
        }
        let result = async {
            if entry
                .owner
                .as_mut()
                .ok_or_else(|| invalid("owner"))?
                .closed_identity()
                .await?
                != prepared.network_identity
            {
                return Err(invalid("network"));
            }
            self.egress
                .handle()
                .handoff_closed_private(engram_egress_proto::private::ClosedPolicyHandoff {
                    allocation: allocation.clone(),
                    policy: request.egress.clone(),
                })
                .await
                .map_err(invalid)?;
            entry
                .owner
                .as_mut()
                .ok_or_else(|| invalid("owner"))?
                .release(key.context.target.binding_epoch)
                .await?;
            let current = self
                .egress
                .handle()
                .read_managed(allocation.session_id)
                .await
                .map_err(invalid)?
                .ok_or_else(|| invalid("policy absent"))?;
            if current.incarnation != allocation.proxy_incarnation
                || current.sandbox_id != id
                || current.guest_ip != allocation.guest_ip
                || current.policy != *policy
            {
                return Err(invalid("policy acknowledgement"));
            }
            let applied = ManagedAppliedState::new(
                context,
                policy.clone(),
                self.incarnation.clone(),
                current.incarnation,
                Some(ManagedNetworkProof {
                    target: key.context.target.clone(),
                    guest_ip: allocation.guest_ip,
                    network_identity: prepared.network_identity,
                }),
            )
            .map_err(invalid)?;
            entry.record.applied = Some(applied.clone());
            entry.record.apply_digest = Some(request_digest);
            self.persist(&entry.record)?;
            Ok(applied)
        }
        .await;
        if result.is_err() {
            self.fail(&mut entry).await;
        }
        result
    }
    async fn retire_for_policy(
        &self,
        entry: &mut Entry,
        policy: ManagedDenyPolicy,
    ) -> Result<(), SandboxError> {
        if let ManagedBootHostState::Retired { receipt } = &entry.record.state {
            if receipt.retained_policy == policy {
                return Ok(());
            }
            return Err(invalid("retirement policy changed"));
        }
        let prepared = entry
            .record
            .prepared
            .clone()
            .ok_or_else(|| invalid("prepared receipt absent"))?;
        let key = entry.record.request.key.clone();
        entry.attach_owner = None;
        entry.record.state = ManagedBootHostState::Failed { key: key.clone() };
        entry.agent = None;
        // Persist the consumed run before stopping it. The owned disk is never
        // selected by a generic pathname or resumed after this point.
        self.persist(&entry.record)?;
        self.unbind_owned(&key);
        let path = self
            .directory
            .join(format!("{}.retired", key.context.target.sandbox_id));
        if path.try_exists()? {
            return Err(invalid("retirement already attempted"));
        }
        entry
            .owner
            .as_mut()
            .ok_or_else(|| invalid("retirement owner"))?
            .retire_disk(&path)
            .await?;
        entry.record.retired = Some(hash_file(&path).await?);
        std::fs::File::open(&self.directory)?.sync_all()?;
        let mut receipt = ManagedBootRetiredReceipt {
            prepared,
            retained_policy: policy,
            receipt_id: String::new(),
        };
        receipt.receipt_id = digest(&serde_json::to_vec(&receipt).map_err(invalid)?);
        receipt
            .validate_for(
                entry
                    .record
                    .prepared
                    .as_ref()
                    .ok_or_else(|| invalid("prepared receipt"))?,
                &entry.record.request.bootstrap_policy,
            )
            .map_err(invalid)?;
        entry.record.state = ManagedBootHostState::Retired {
            receipt: Box::new(receipt),
        };
        self.persist(&entry.record)?;
        if let Some(owner) = entry.owner.take() {
            owner.close().await?;
        }
        Ok(())
    }
    async fn verify_retired(&self, record: &Record) -> Result<(), SandboxError> {
        let ManagedBootHostState::Retired { receipt } = &record.state else {
            return Err(invalid("retired state"));
        };
        receipt
            .validate_for(
                record
                    .prepared
                    .as_ref()
                    .ok_or_else(|| invalid("retired preparation"))?,
                &record.request.bootstrap_policy,
            )
            .map_err(invalid)?;
        let path = self.directory.join(format!(
            "{}.retired",
            record.request.key.context.target.sandbox_id
        ));
        if record.retired.as_ref() != Some(&hash_file(&path).await?) {
            return Err(invalid("retired disk"));
        }
        Ok(())
    }
    pub async fn read_policy(
        &self,
        target: ManagedPolicyTarget,
    ) -> Result<Option<ManagedAppliedState>, SandboxError> {
        let entry = self
            .entries
            .get(&target.sandbox_id)
            .map(|e| e.clone())
            .ok_or_else(|| invalid("owner"))?;
        let mut entry = entry.lock().await;
        if entry.record.request.key.context.target != target {
            return Err(invalid("target"));
        }
        if entry.record.applied.is_some() {
            entry
                .owner
                .as_mut()
                .ok_or_else(|| invalid("owner"))?
                .ready(target.binding_epoch)
                .await?;
        }
        Ok(entry.record.applied.clone())
    }
    pub async fn finalize(
        self: &Arc<Self>,
        request: ManagedBootFinalize,
    ) -> Result<ManagedBootFinalReceipt, SandboxError> {
        let this = self.clone();
        tokio::spawn(async move { this.finalize_inner(request).await })
            .await
            .map_err(invalid)?
    }
    async fn finalize_inner(
        &self,
        request: ManagedBootFinalize,
    ) -> Result<ManagedBootFinalReceipt, SandboxError> {
        request.validate_agent().map_err(invalid)?;
        let f = &request.finalization;
        f.prepared.validate().map_err(invalid)?;
        f.policy.validate().map_err(invalid)?;
        if f.native_version != 1
            || f.combined_manifest_digest != digest(&f.combined_manifest)
            || f.combined_manifest.len() > 1024 * 1024
        {
            return Err(invalid("manifest"));
        }
        let key = &f.prepared.request.key;
        let entry = self.entry(key)?;
        let mut entry = entry.lock().await;
        self.check_key(&entry, key)?;
        if entry.record.applied.as_ref() != Some(&f.policy)
            || f.prepared.host_incarnation != self.incarnation
            || f.policy.host_incarnation != self.incarnation
            || f.policy.network_proof.as_ref().is_none_or(|p| {
                p.target != key.context.target
                    || p.network_identity != f.prepared.network_identity
                    || p.guest_ip != f.prepared.guest_ip
            })
        {
            return Err(invalid("policy"));
        }
        if let ManagedBootHostState::Finalized { receipt } = &entry.record.state {
            receipt.validate_for(f).map_err(invalid)?;
            if entry.record.finalization.as_ref() != Some(&f.digest().map_err(invalid)?) {
                return Err(invalid("final retry"));
            }
            let receipt = receipt.clone();
            entry
                .owner
                .as_mut()
                .ok_or_else(|| invalid("owner"))?
                .ready(key.context.target.binding_epoch)
                .await?;
            return Ok(receipt);
        }
        match &entry.record.state {
            ManagedBootHostState::Prepared { receipt } if receipt.as_ref() == &f.prepared => (),
            _ => return Err(invalid("phase")),
        }
        entry
            .owner
            .as_mut()
            .ok_or_else(|| invalid("owner"))?
            .ready(key.context.target.binding_epoch)
            .await?;
        let mut receipt = ManagedBootFinalReceipt {
            key: key.clone(),
            prepared_receipt_id: f.prepared.receipt_id.clone(),
            finalization_digest: f.digest().map_err(invalid)?,
            host_incarnation: self.incarnation.clone(),
            network_identity: f.prepared.network_identity.clone(),
            receipt_id: String::new(),
        };
        receipt.receipt_id = digest(&serde_json::to_vec(&receipt).map_err(invalid)?);
        entry.record.finalization = Some(f.digest().map_err(invalid)?);
        entry.record.state = ManagedBootHostState::Finalized {
            receipt: receipt.clone(),
        };
        entry.agent = Some(request.agent);
        self.persist(&entry.record)?;
        Ok(receipt)
    }
    pub async fn start(
        self: &Arc<Self>,
        request: ManagedBootStart,
    ) -> Result<ManagedBootHostState, SandboxError> {
        let this = self.clone();
        tokio::spawn(async move { this.start_inner(request).await })
            .await
            .map_err(invalid)?
    }
    async fn start_inner(
        &self,
        request: ManagedBootStart,
    ) -> Result<ManagedBootHostState, SandboxError> {
        request.validate().map_err(invalid)?;
        let entry = self.entry(&request.receipt.key)?;
        let mut entry = entry.lock().await;
        self.check_key(&entry, &request.receipt.key)?;
        if let ManagedBootHostState::Started { intent } = &entry.record.state {
            if intent != &request {
                return Err(invalid("start retry"));
            }
            entry
                .owner
                .as_mut()
                .ok_or_else(|| invalid("owner"))?
                .ready(request.receipt.key.context.target.binding_epoch)
                .await?;
            return Ok(entry.record.state.clone());
        }
        match &entry.record.state {
            ManagedBootHostState::Finalized { receipt } if receipt == &request.receipt => (),
            _ => return Err(invalid("start consumed")),
        }
        let agent = entry.agent.take().ok_or_else(|| invalid("agent"))?;
        entry.record.state = ManagedBootHostState::StartPending {
            intent: request.clone(),
        };
        self.persist(&entry.record)?;
        let key = request.receipt.key.clone();
        let result = async {
            {
                let _guard = self.binding_lock.lock().map_err(invalid)?;
                self.hub
                    .bind_session(
                        key.creation.session_id,
                        key.context.target.sandbox_id,
                        key.context.target.binding_epoch,
                    )
                    .map_err(invalid)?;
            }
            let attach_owner = self.hub.managed_owner(request.clone());
            let sink = attach_owner.sink();
            entry.attach_owner = Some(attach_owner);
            tokio::time::timeout(
                std::time::Duration::from_secs(210),
                entry
                    .owner
                    .as_mut()
                    .ok_or_else(|| invalid("owner"))?
                    .start(agent, sink),
            )
            .await
            .map_err(invalid)??;
            entry.record.state = ManagedBootHostState::Started { intent: request };
            self.persist(&entry.record)?;
            Ok(entry.record.state.clone())
        }
        .await;
        if result.is_err() {
            self.fail(&mut entry).await;
        }
        result
    }
    pub async fn capture(
        self: &Arc<Self>,
        request: ManagedCaptureRequest,
    ) -> Result<ManagedCaptureAuthority, SandboxError> {
        let this = self.clone();
        tokio::spawn(async move { this.capture_inner(request).await })
            .await
            .map_err(invalid)?
    }
    async fn capture_inner(
        &self,
        request: ManagedCaptureRequest,
    ) -> Result<ManagedCaptureAuthority, SandboxError> {
        request.key.validate().map_err(invalid)?;
        if request.capture_id.as_uuid().is_nil() {
            return Err(invalid("capture"));
        }
        let entry = match self.entry(&request.key) {
            Ok(entry) => entry,
            Err(_) => {
                let record = self
                    .read_record(request.key.context.target.sandbox_id)?
                    .ok_or_else(|| invalid("capture record absent"))?;
                if record.request.key != request.key
                    || record.request.key.context.target.host_id != self.host_id
                    || !(record.retired.is_some()
                        || record.capture.as_ref().is_some_and(|c| {
                            c.request == request && (c.artifact.is_some() || c.authority.is_some())
                        }))
                {
                    return Err(invalid("capture source lost"));
                }
                self.entries
                    .entry(request.key.context.target.sandbox_id)
                    .or_insert_with(|| {
                        Arc::new(Mutex::new(Entry {
                            record,
                            attach_owner: None,
                            owner: None,
                            agent: None,
                        }))
                    })
                    .clone()
            }
        };
        let mut entry = entry.lock().await;
        if entry.record.request.key != request.key {
            return Err(invalid("capture key"));
        }
        if let Some(progress) = &entry.record.capture {
            if progress.request != request {
                return Err(invalid("capture retry"));
            }
            if let Some(authority) = &progress.authority {
                return Ok(authority.clone());
            }
        } else {
            if !matches!(entry.record.state, ManagedBootHostState::Started { .. })
                && entry.record.retired.is_none()
            {
                return Err(invalid("capture source"));
            }
            entry.record.capture = Some(CaptureProgress {
                request: request.clone(),
                manifest: request.disk_manifest(),
                artifact: None,
                authority: None,
            });
            entry.attach_owner = None;
            entry.record.state = ManagedBootHostState::Failed {
                key: request.key.clone(),
            };
            self.persist(&entry.record)?;
        }
        let path = self.directory.join(format!("{}.disk", request.capture_id));
        let result = async {
            let mut progress = entry
                .record
                .capture
                .clone()
                .ok_or_else(|| invalid("capture"))?;
            if progress.artifact.is_none() {
                if path.try_exists()? {
                    return Err(invalid("incomplete capture artifact"));
                }
                self.unbind_owned(&request.key);
                if let Some(retired) = &entry.record.retired {
                    let source = self
                        .directory
                        .join(format!("{}.retired", request.key.context.target.sandbox_id));
                    if &hash_file(&source).await? != retired {
                        return Err(invalid("retired disk changed"));
                    }
                    let destination = path.clone();
                    tokio::task::spawn_blocking(move || {
                        std::fs::copy(&source, &destination)?;
                        std::fs::File::open(&destination)?.sync_all()
                    })
                    .await
                    .map_err(invalid)??;
                } else {
                    entry
                        .owner
                        .as_mut()
                        .ok_or_else(|| invalid("source lost"))?
                        .retire_disk(&path)
                        .await?;
                }
                let (hash, size) = hash_file(&path).await?;
                std::fs::File::open(&self.directory)?.sync_all()?;
                progress.artifact = Some((hash, size));
                entry.record.capture = Some(progress.clone());
                self.persist(&entry.record)?;
            }
            let (hash, size) = progress
                .artifact
                .clone()
                .ok_or_else(|| invalid("artifact"))?;
            if hash_file(&path).await? != (hash.clone(), size) {
                return Err(invalid("capture integrity"));
            }
            let manifest = self
                .chunks
                .chunk_file(&path, engram_chunk_store::ManifestKind::Disk, None)
                .await
                .map_err(invalid)?;
            publish_capture_manifest(&self.chunks, progress.manifest, &manifest).await?;
            let cold = &entry.record.request.cold;
            let mut authority = ManagedCaptureAuthority {
                version: 1,
                request,
                host_incarnation: entry.record.incarnation.clone(),
                disk_manifest: progress.manifest,
                disk_sha256: hash,
                disk_size_bytes: size,
                image: cold.image.clone(),
                kernel_sha256: cold.kernel_sha256.clone(),
                aux_ro_drives: cold.aux_ro_drives.clone(),
                consistency: ManagedDiskConsistency::CrashConsistentDiskCutV1,
                source_state: ManagedCapturedSourceState::Destroyed,
                record_digest: String::new(),
            };
            authority.record_digest = authority.digest().map_err(invalid)?;
            authority.validate().map_err(invalid)?;
            progress.authority = Some(authority.clone());
            entry.record.capture = Some(progress);
            entry.record.state = ManagedBootHostState::Captured {
                authority: authority.clone(),
            };
            self.persist(&entry.record)?;
            if let Some(owner) = entry.owner.take() {
                owner.close().await?;
            }
            Ok(authority)
        }
        .await;
        if result.is_err() {
            self.fail(&mut entry).await;
        }
        result
    }
    pub async fn destroy(
        self: &Arc<Self>,
        id: SandboxId,
        fence: engram_core::traits::SessionFence,
    ) -> Result<(), SandboxError> {
        let this = self.clone();
        tokio::spawn(async move {
            let entry = this
                .entries
                .get(&id)
                .map(|e| e.clone())
                .ok_or_else(|| invalid("owner"))?;
            let mut entry = entry.lock().await;
            let target = entry.record.request.key.context.target.clone();
            if target.session_id != fence.session_id || fence.epoch == 0 {
                return Err(invalid("destroy target"));
            }
            if entry.owner.is_none()
                && matches!(
                    entry.record.state,
                    ManagedBootHostState::Retired { .. } | ManagedBootHostState::Captured { .. }
                )
            {
                // The physical source is already gone. Preserve its durable proof.
                return Ok(());
            }
            entry.attach_owner = None;
            entry.record.state = ManagedBootHostState::Failed {
                key: entry.record.request.key.clone(),
            };
            this.persist(&entry.record)?;
            entry.agent = None;
            this.unbind_owned(&entry.record.request.key);
            if let Some(owner) = entry.owner.take() {
                owner.close().await?;
            }
            Ok(())
        })
        .await
        .map_err(invalid)?
    }
    pub fn ordinary_spec(
        &self,
        spec: &engram_core::types::sandbox::SandboxSpec,
    ) -> Result<(), SandboxError> {
        self.ordinary_disk(spec.rootfs_manifest, spec.rootfs_source.as_deref())?;
        self.ordinary_disk(spec.swap_manifest, spec.swap_source.as_deref())
    }
    pub fn ordinary_disk(
        &self,
        manifest: Option<ManifestRef>,
        source: Option<&Path>,
    ) -> Result<(), SandboxError> {
        for entry in std::fs::read_dir(&self.directory)? {
            let path = entry?.path();
            if path.extension().and_then(|e| e.to_str()) != Some("boot") {
                continue;
            }
            let id: SandboxId = path
                .file_stem()
                .and_then(|s| s.to_str())
                .ok_or_else(|| invalid("record name"))?
                .parse()
                .map_err(invalid)?;
            let record = self
                .read_record(id)?
                .ok_or_else(|| invalid("record absent"))?;
            let key = &record.request.key;
            let fc_name = format!(
                "m-{}",
                &digest(
                    format!("{}\0{}", key.context.target.sandbox_id, key.boot_attempt).as_bytes()
                )[7..31]
            );
            let vz_name = format!(
                "managed-{}-{}",
                key.context.target.sandbox_id, key.boot_attempt
            );
            let private_path = source.is_some_and(|path| {
                let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
                path.starts_with(&self.directory)
                    || path.components().any(|c| {
                        c.as_os_str() == std::ffi::OsStr::new(&fc_name)
                            || c.as_os_str() == std::ffi::OsStr::new(&vz_name)
                    })
            });
            if manifest == Some(record.request.cold.disk.manifest)
                || record
                    .capture
                    .as_ref()
                    .is_some_and(|c| manifest == Some(c.manifest))
                || private_path
            {
                return Err(invalid("managed disk cannot use ordinary create"));
            }
        }
        Ok(())
    }
    pub async fn guard_running(&self, id: SandboxId) -> Result<(), SandboxError> {
        let entry = self
            .entries
            .get(&id)
            .map(|e| e.clone())
            .ok_or_else(|| invalid("owner"))?;
        let mut entry = entry.lock().await;
        if !matches!(entry.record.state, ManagedBootHostState::Started { .. }) {
            return Err(invalid("not started"));
        }
        let epoch = entry.record.request.key.context.target.binding_epoch;
        entry
            .owner
            .as_mut()
            .ok_or_else(|| invalid("owner"))?
            .ready(epoch)
            .await
    }
    pub async fn shutdown(&self) {
        let entries: Vec<_> = self.entries.iter().map(|e| e.value().clone()).collect();
        for entry in entries {
            let mut entry = entry.lock().await;
            if matches!(entry.record.state, ManagedBootHostState::Started { .. }) {
                if let Some(policy) = entry.record.applied.as_ref().map(|a| a.policy.clone()) {
                    if self.retire_for_policy(&mut entry, policy).await.is_ok() {
                        continue;
                    }
                }
            }
            if entry.owner.is_some() {
                self.fail(&mut entry).await;
            }
        }
    }
    /// Diagnostic residency only. A busy lifecycle is unknown, never absence.
    /// Do not wait for native I/O or reuse this inventory as start/policy proof.
    pub fn list(&self) -> Result<Vec<SandboxId>, SandboxError> {
        let entries: Vec<_> = self
            .entries
            .iter()
            .map(|e| (*e.key(), e.value().clone()))
            .collect();
        let mut residents = Vec::new();
        for (id, entry) in entries {
            let entry = entry.try_lock().map_err(|_| invalid("inventory busy"))?;
            if entry.owner.is_some()
                || matches!(
                    entry.record.state,
                    ManagedBootHostState::Prepared { .. }
                        | ManagedBootHostState::Finalized { .. }
                        | ManagedBootHostState::StartPending { .. }
                        | ManagedBootHostState::Started { .. }
                )
            {
                residents.push(id);
            }
        }
        Ok(residents)
    }
}

/// Called only while holding the same owner lock as finalize and start.
fn bootstrap_retirement_allowed(record: &Record, request: &ManagedPolicyApply) -> bool {
    let key = &record.request.key;
    let Some(policy) = request.egress.managed_deny.as_ref() else {
        return false;
    };
    matches!(&record.state, ManagedBootHostState::Prepared { receipt }
        if receipt.request == record.request && receipt.host_incarnation == record.incarnation)
        && record.applied.is_none()
        && request.expected.is_none()
        && request.context.target == key.context.target
        && request.context.binding_hash.is_none()
        && request.context.operator_generation > key.context.operator_generation
        && policy.revision >= record.request.bootstrap_policy.revision
        && policy.contains(&record.request.bootstrap_policy)
        && (policy.revision != record.request.bootstrap_policy.revision
            || policy == &record.request.bootstrap_policy)
}

fn bootstrap(allocation: &Allocation, policy: ManagedDenyPolicy) -> SessionEgressPolicy {
    SessionEgressPolicy {
        session_id: allocation.session_id,
        sandbox_id: allocation.sandbox_id,
        guest_ip: allocation.guest_ip,
        network_allow_hosts: vec![],
        network_allow_host_patterns: vec![],
        allow_all: false,
        secrets: vec![],
        injects: vec![],
        observes: vec![],
        guest_services: vec![],
        tunnels: vec![],
        secret_mode: SecretMode::Literal,
        apps: vec![],
        managed_deny: Some(policy),
    }
}
fn durable_write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let temporary = path.with_extension("partial");
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(&temporary)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    std::fs::rename(&temporary, path)?;
    std::fs::File::open(
        path.parent()
            .ok_or_else(|| std::io::Error::other("missing parent"))?,
    )?
    .sync_all()
}
async fn hash_file(path: &Path) -> Result<(String, u64), SandboxError> {
    let path = path.to_owned();
    tokio::task::spawn_blocking(move || {
        use sha2::{Digest, Sha256};
        use std::io::Read;
        let mut file = std::fs::File::open(path)?;
        let size = file.metadata()?.len();
        let mut hash = Sha256::new();
        let mut buffer = [0; 65536];
        loop {
            let n = file.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            hash.update(&buffer[..n]);
        }
        Ok::<_, std::io::Error>((hex::encode(hash.finalize()), size))
    })
    .await
    .map_err(invalid)?
    .map_err(Into::into)
}

async fn publish_capture_manifest(
    chunks: &engram_chunk_store::ChunkStore,
    reference: ManifestRef,
    manifest: &engram_chunk_store::Manifest,
) -> Result<(), SandboxError> {
    if let Err(error) = chunks.put_manifest(reference, manifest).await {
        // The immutable write may have completed before its reply was lost.
        // Only the same reserved name and exact chunk list can complete a retry.
        if chunks.get_manifest(reference).await.map_err(invalid)? != *manifest {
            return Err(invalid(error));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use engram_core::{
        traits::HostClient,
        types::{
            managed_cold::*,
            managed_create::ManagedCreateIdentity,
            mcp_policy::{McpPolicyCreation, WriteEligibility},
            sandbox::AuxRoDrive,
        },
    };

    fn fixture(path: &Path) -> (Arc<ManagedBootHost>, ManagedBootPrepare) {
        let host_id = HostId::new();
        let session_id = engram_core::SessionId::new();
        let sandbox_id = SandboxId::new();
        let manifest = ManifestRef::new();
        let drives: Vec<_> = (0..3)
            .map(|i| AuxRoDrive {
                drive_id: AuxRoDrive::slot_drive_id(i),
                guest_mount: AuxRoDrive::slot_guest_mount(i),
                fs_type: "squashfs".into(),
                sha256: Some("a".repeat(64)),
            })
            .collect();
        let spec = serde_json::json!({"image":"image-v1", "rootfs_source":path.join("selected"), "rootfs_manifest":manifest, "cpu":{"vcpus":1}, "memory":{"max_mib":128}, "disk":{"max_gib":1}, "env":{}, "aux_ro_drives":drives});
        let request = ManagedBootPrepare {
            key: ManagedBootKey {
                creation: ManagedCreateIdentity {
                    session_id,
                    task_id: "task".into(),
                    policy: McpPolicyCreation {
                        actor_id: "actor".into(),
                        input_id: "input".into(),
                        parent_session_id: None,
                        eligibility: WriteEligibility::ReadOnly,
                    },
                    input_digest: digest(b"input"),
                },
                context: ManagedPolicyContext {
                    target: ManagedPolicyTarget {
                        session_id,
                        sandbox_id,
                        host_id,
                        binding_epoch: 1,
                    },
                    operator_generation: 1,
                    operator_identity: digest(b"operator"),
                    binding_hash: None,
                },
                boot_attempt: "boot-1".into(),
                native_digest: digest(b"[]"),
            },
            cold: ManagedColdPrepare {
                version: 1,
                session_id,
                sandbox_id,
                boot_attempt: "boot-1".into(),
                image: "image-v1".into(),
                disk: ManagedColdDisk {
                    manifest,
                    sha256: "b".repeat(64),
                    size_bytes: 4,
                    source: ManagedColdSource::FreshImage {
                        base_manifest: ManifestRef::new(),
                    },
                },
                kernel_sha256: "c".repeat(64),
                aux_ro_drives: drives,
            },
            bootstrap_policy: ManagedDenyPolicy::new(1, ["mcp.example".into()]).unwrap(),
            capture_authority: None,
            sandbox_spec_json: spec.to_string(),
        };
        request.spec().unwrap();
        std::fs::write(path.join("kernel"), b"kernel").unwrap();
        #[cfg(target_os = "macos")]
        let backend = ManagedBackend::Vz {
            backend: Arc::new(
                engram_sandbox_vz::VzBackend::new(
                    path.join("native"),
                    engram_sandbox_vz::VzConfig::with_kernel(path.join("kernel")),
                )
                .unwrap(),
            ),
            helper: engram_sandbox_vz::ManagedHelperArtifact {
                path: path.join("absent-helper"),
                sha256: "a".repeat(64),
            },
        };
        #[cfg(target_os = "linux")]
        let backend = ManagedBackend::Firecracker(Arc::new(
            engram_sandbox_firecracker::FirecrackerBackend::new(
                path.join("native"),
                engram_sandbox_firecracker::FirecrackerConfig::with_kernel(path.join("kernel")),
            ),
        ));
        let ordinary = Arc::new(engram_sandbox_process::ProcessBackend::new(
            path.join("ordinary"),
        ));
        let bindings = BindingStore::open(path.join("bindings")).unwrap();
        let hub = Arc::new(HarnessHub::new(
            crate::harness::event_sink_to(|_, _, _, _| async { Ok(()) }),
            bindings,
        ));
        let egress = Arc::new(HostEgress::new(
            Arc::new(crate::proxyd_client::ProxydHandle::new(
                path.join("absent.sock"),
            )),
            String::new(),
            None,
        ));
        let chunks = engram_chunk_store::ChunkStore::new(Arc::new(
            engram_storage_local::LocalBlobStorage::new(path.join("blobs")),
        ));
        let host = Arc::new(
            ManagedBootHost::new(
                ManagedBootHostConfig {
                    host_id,
                    incarnation: "current-host".into(),
                    directory: path.join("records"),
                },
                backend,
                ordinary,
                egress,
                hub,
                chunks,
            )
            .unwrap(),
        );
        (host, request)
    }
    fn failed(request: ManagedBootPrepare) -> Record {
        Record {
            version: 1,
            state: ManagedBootHostState::Failed {
                key: request.key.clone(),
            },
            request,
            incarnation: "retired-host".into(),
            allocation: None,
            prepared: None,
            applied: None,
            apply_digest: None,
            finalization: None,
            capture: None,
            retired: None,
        }
    }

    fn issued_start(request: &ManagedBootPrepare) -> ManagedBootStart {
        ManagedBootStart {
            receipt: ManagedBootFinalReceipt {
                key: request.key.clone(),
                prepared_receipt_id: digest(b"prepared"),
                finalization_digest: digest(b"final"),
                host_incarnation: "current-host".into(),
                network_identity: digest(b"network"),
                receipt_id: digest(b"receipt"),
            },
            start_id: "issued-start".into(),
        }
    }

    #[tokio::test]
    async fn managed_attach_requires_exact_private_owner_and_revokes_pending_and_live_streams() {
        use engram_harness_proto::{read_msg, write_msg, HarnessAttach, HarnessAttachAck};
        use tokio::io::AsyncReadExt;
        let temp = tempfile::tempdir().unwrap();
        let (host, request) = fixture(temp.path());
        let target = &request.key.context.target;
        host.hub
            .bind_session(target.session_id, target.sandbox_id, target.binding_epoch)
            .unwrap();
        durable_write(
            &host
                .bindings
                .directory()
                .join(format!("{}.managed-boot", target.session_id)),
            b"1",
        )
        .unwrap();
        durable_write(
            &host
                .bindings
                .directory()
                .join(format!("{}.managed-sandbox", target.sandbox_id)),
            b"1",
        )
        .unwrap();
        let intent = issued_start(&request);
        let mut record = failed(request.clone());
        record.state = ManagedBootHostState::StartPending {
            intent: intent.clone(),
        };
        host.persist(&record).unwrap();
        let entry = Arc::new(Mutex::new(Entry {
            record,
            attach_owner: Some(host.hub.managed_owner(intent)),
            owner: None,
            agent: None,
        }));
        host.entries.insert(target.sandbox_id, entry.clone());
        let mut guard = entry.lock().await;
        let sink = guard.attach_owner.as_ref().unwrap().sink();
        let exact = HarnessAttach {
            session_id: target.session_id,
            sandbox_id: target.sandbox_id,
            binding_epoch: target.binding_epoch,
            harness_version: "test".into(),
        };
        // A valid token from another private source still cannot enter this owner.
        let other_session = engram_core::SessionId::new();
        let other_sandbox = SandboxId::new();
        host.hub
            .bind_session(other_session, other_sandbox, target.binding_epoch)
            .unwrap();
        for wrong in [
            HarnessAttach {
                session_id: other_session,
                sandbox_id: other_sandbox,
                ..exact.clone()
            },
            HarnessAttach {
                sandbox_id: other_sandbox,
                ..exact.clone()
            },
            HarnessAttach {
                binding_epoch: target.binding_epoch + 1,
                ..exact.clone()
            },
        ] {
            let (transport, mut guest) = tokio::io::duplex(4096);
            sink(Box::pin(transport));
            write_msg(&mut guest, &wrong).await.unwrap();
            let ack: HarnessAttachAck = read_msg(&mut guest).await.unwrap();
            assert!(!ack.ok);
            assert!(!host.hub.is_attached(target.sandbox_id));
            assert!(!host.hub.is_attached(other_sandbox));
        }
        let (transport, mut live) = tokio::io::duplex(4096);
        sink(Box::pin(transport));
        write_msg(&mut live, &exact).await.unwrap();
        let ack: HarnessAttachAck =
            tokio::time::timeout(std::time::Duration::from_millis(100), read_msg(&mut live))
                .await
                .unwrap()
                .unwrap();
        assert!(
            ack.ok,
            "first attach must work while issued start holds its lifecycle lock"
        );
        assert!(matches!(
            guard.record.state,
            ManagedBootHostState::StartPending { .. }
        ));
        assert!(host.hub.is_attached(target.sandbox_id));
        // A second accepted transport is waiting for Hello when the owner is lost.
        let (transport, mut waiting) = tokio::io::duplex(4096);
        sink(Box::pin(transport));
        tokio::task::yield_now().await;
        host.fail(&mut guard).await;
        assert!(!host.hub.is_attached(target.sandbox_id));
        for guest in [&mut live, &mut waiting] {
            assert_eq!(
                tokio::time::timeout(
                    std::time::Duration::from_millis(100),
                    guest.read(&mut [0u8; 1])
                )
                .await
                .unwrap()
                .unwrap(),
                0
            );
        }
        // The sink outlives its owner but cannot issue another capability.
        let (transport, mut late) = tokio::io::duplex(4096);
        sink(Box::pin(transport));
        assert_eq!(late.read(&mut [0u8; 1]).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn managed_attach_revocation_serializes_registration_and_preserves_replacement() {
        use engram_harness_proto::{read_msg, write_msg, HarnessAttach, HarnessAttachAck};
        let temp = tempfile::tempdir().unwrap();
        let (host, request) = fixture(temp.path());
        let target = &request.key.context.target;
        host.hub
            .bind_session(target.session_id, target.sandbox_id, target.binding_epoch)
            .unwrap();
        let intent = issued_start(&request);
        let old = host.hub.managed_owner(intent.clone());
        let old_sink = old.sink();
        let (transport, mut old_guest) = tokio::io::duplex(4096);
        old_sink(Box::pin(transport));
        let exact = HarnessAttach {
            session_id: target.session_id,
            sandbox_id: target.sandbox_id,
            binding_epoch: target.binding_epoch,
            harness_version: "test".into(),
        };
        write_msg(&mut old_guest, &exact).await.unwrap();
        assert!(
            read_msg::<_, HarnessAttachAck>(&mut old_guest)
                .await
                .unwrap()
                .ok
        );
        let replacement = host.hub.managed_owner(intent.clone());
        let (transport, mut new_guest) = tokio::io::duplex(4096);
        replacement.sink()(Box::pin(transport));
        write_msg(&mut new_guest, &exact).await.unwrap();
        assert!(
            read_msg::<_, HarnessAttachAck>(&mut new_guest)
                .await
                .unwrap()
                .ok
        );
        drop(old);
        tokio::task::yield_now().await;
        assert!(
            host.hub.is_attached(target.sandbox_id),
            "stale owner cleanup cannot remove replacement registration"
        );
        drop(replacement);
        assert!(!host.hub.is_attached(target.sandbox_id));
        // The ACK is blocked after registration. Revocation must remove the
        // entry immediately and cut the pending write, not wait for the peer.
        let pending = host.hub.managed_owner(intent);
        let (transport, mut pending_guest) = tokio::io::duplex(1);
        pending.sink()(Box::pin(transport));
        write_msg(&mut pending_guest, &exact).await.unwrap();
        tokio::time::timeout(std::time::Duration::from_millis(100), async {
            while !host.hub.is_attached(target.sandbox_id) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        drop(pending);
        assert!(!host.hub.is_attached(target.sandbox_id));
        assert!(tokio::time::timeout(
            std::time::Duration::from_millis(100),
            read_msg::<_, HarnessAttachAck>(&mut pending_guest)
        )
        .await
        .unwrap()
        .is_err());
    }

    #[tokio::test]
    async fn managed_inventory_busy_is_unknown_without_waiting_for_lifecycle() {
        let temp = tempfile::tempdir().unwrap();
        let (host, request) = fixture(temp.path());
        let id = request.key.context.target.sandbox_id;
        let mut record = failed(request.clone());
        record.state = ManagedBootHostState::Prepared {
            receipt: Box::new(ManagedBootPreparedReceipt {
                request,
                host_incarnation: host.incarnation.clone(),
                network_identity: digest(b"network"),
                guest_ip: "192.0.2.2".parse().unwrap(),
                receipt_id: digest(b"prepared"),
            }),
        };
        let entry = Arc::new(Mutex::new(Entry {
            record,
            attach_owner: None,
            owner: None,
            agent: None,
        }));
        host.entries.insert(id, entry.clone());
        let local =
            crate::host_client::LocalHostClient::new(host.ordinary.clone(), host.hub.clone())
                .with_managed_boot(host.clone());
        let guard = entry.lock().await;
        let inventory = tokio::time::timeout(std::time::Duration::from_millis(100), local.list())
            .await
            .expect("heartbeat inventory must not wait for lifecycle work");
        assert!(
            inventory.is_err(),
            "busy is unknown, never an empty known inventory"
        );
        drop(guard);
        assert!(local.list().await.unwrap().contains(&id));
    }

    #[tokio::test]
    async fn managed_disk_reservation_covers_swap_create_and_both_restores() {
        let temp = tempfile::tempdir().unwrap();
        let (host, request) = fixture(temp.path());
        let capture = ManagedCaptureRequest {
            key: request.key.clone(),
            capture_id: engram_core::SnapshotId::new(),
        };
        let mut record = failed(request.clone());
        record.capture = Some(CaptureProgress {
            request: capture.clone(),
            manifest: capture.disk_manifest(),
            artifact: None,
            authority: None,
        });
        host.persist(&record).unwrap();
        let local =
            crate::host_client::LocalHostClient::new(host.ordinary.clone(), host.hub.clone())
                .with_managed_boot(host.clone());
        let mut ordinary: engram_core::types::sandbox::SandboxSpec =
            serde_json::from_str(&request.sandbox_spec_json).unwrap();
        ordinary.rootfs_manifest = Some(ManifestRef::new());
        ordinary.rootfs_source = Some(temp.path().join("ordinary.ext4"));
        host.ordinary_spec(&ordinary).unwrap();
        let rejected = |result: Result<SandboxId, SandboxError>| {
            assert!(
                matches!(result, Err(SandboxError::InvalidSpec(message)) if message == "managed boot ownership changed or unavailable")
            );
        };
        for protected in [request.cold.disk.manifest, capture.disk_manifest()] {
            let mut spec = ordinary.clone();
            spec.swap_manifest = Some(protected);
            rejected(local.create(spec).await);
            let metadata: engram_core::types::snapshot::SnapshotMetadata =
                serde_json::from_value(serde_json::json!({
                    "id": engram_core::SnapshotId::new(), "size_bytes": 4,
                    "created_at": "2026-01-01T00:00:00Z", "image_version": "image-v1",
                    "disk_manifest": ordinary.rootfs_manifest, "swap_manifest": protected
                }))
                .unwrap();
            let fence = engram_core::traits::SessionFence {
                session_id: engram_core::SessionId::new(),
                epoch: 1,
            };
            rejected(local.restore(metadata.clone(), fence).await);
            rejected(
                local
                    .restore_base_for_session(metadata, Default::default(), vec![], fence)
                    .await,
            );
        }
        for path in [
            host.directory.join("private.ext4"),
            temp.path().join(format!(
                "managed-{}-{}/rootfs.ext4",
                request.key.context.target.sandbox_id, request.key.boot_attempt
            )),
            temp.path().join(format!(
                "m-{}/rootfs.ext4",
                &digest(
                    format!(
                        "{}\0{}",
                        request.key.context.target.sandbox_id, request.key.boot_attempt
                    )
                    .as_bytes()
                )[7..31]
            )),
        ] {
            let mut spec = ordinary.clone();
            spec.swap_source = Some(path);
            rejected(local.create(spec).await);
        }
    }

    #[tokio::test]
    async fn durable_managed_identity_blocks_generic_paths_after_owner_loss() {
        let temp = tempfile::tempdir().unwrap();
        let (host, request) = fixture(temp.path());
        let key = request.key.clone();
        let id = key.context.target.sandbox_id;
        host.persist(&failed(request.clone())).unwrap();
        assert!(host
            .ordinary_disk(Some(request.cold.disk.manifest), None)
            .is_err());
        assert!(host
            .ordinary_disk(None, Some(&host.directory.join("private.ext4")))
            .is_err());
        durable_write(
            &host
                .bindings
                .directory()
                .join(format!("{id}.managed-sandbox")),
            b"1",
        )
        .unwrap();
        durable_write(
            &host
                .bindings
                .directory()
                .join(format!("{}.managed-boot", key.creation.session_id)),
            id.to_string().as_bytes(),
        )
        .unwrap();
        let local =
            crate::host_client::LocalHostClient::new(host.ordinary.clone(), host.hub.clone())
                .with_managed_boot(host.clone());
        assert!(matches!(
            host.read(key.clone()).await.unwrap(),
            ManagedBootHostState::Failed { .. }
        ));
        assert!(host.prepare(request).await.is_err());
        assert!(local
            .pause(
                id,
                engram_core::traits::SessionFence {
                    session_id: key.creation.session_id,
                    epoch: 1
                }
            )
            .await
            .is_err());
        assert!(local
            .resume(
                id,
                engram_core::traits::SessionFence {
                    session_id: key.creation.session_id,
                    epoch: 1
                }
            )
            .await
            .is_err());
        assert!(local
            .snapshot(
                id,
                engram_core::traits::SessionFence {
                    session_id: key.creation.session_id,
                    epoch: 1
                }
            )
            .await
            .is_err());
        assert!(local
            .bind_session(key.creation.session_id, id, 1)
            .await
            .is_err());
        assert!(local.interrupt(id).await.is_err());
        assert!(local.list().await.unwrap().is_empty());
        let mut wrong = key;
        wrong.boot_attempt = "other".into();
        assert!(host.read(wrong).await.is_err());
    }

    #[tokio::test]
    async fn durable_capture_retries_exact_artifact_after_lost_manifest_reply() {
        let temp = tempfile::tempdir().unwrap();
        let (host, request) = fixture(temp.path());
        let capture = ManagedCaptureRequest {
            key: request.key.clone(),
            capture_id: engram_core::SnapshotId::new(),
        };
        let path = host.directory.join(format!("{}.disk", capture.capture_id));
        durable_write(&path, b"retired disk cut").unwrap();
        let artifact = hash_file(&path).await.unwrap();
        let mut record = failed(request);
        record.capture = Some(CaptureProgress {
            request: capture.clone(),
            manifest: capture.disk_manifest(),
            artifact: Some(artifact.clone()),
            authority: None,
        });
        host.persist(&record).unwrap();
        let manifest = host
            .chunks
            .chunk_file(&path, engram_chunk_store::ManifestKind::Disk, None)
            .await
            .unwrap();
        // The prior call wrote the manifest, then lost its reply before receipt persistence.
        host.chunks
            .put_manifest(capture.disk_manifest(), &manifest)
            .await
            .unwrap();
        let authority = host.capture(capture.clone()).await.unwrap();
        assert_eq!(
            (authority.disk_sha256.clone(), authority.disk_size_bytes),
            artifact
        );
        assert_eq!(authority.host_incarnation, "retired-host");
        assert_eq!(host.capture(capture.clone()).await.unwrap(), authority);
        assert_eq!(
            host.read(capture.key.clone()).await.unwrap(),
            ManagedBootHostState::Captured {
                authority: authority.clone()
            }
        );
        let mut changed = capture.clone();
        changed.capture_id = engram_core::SnapshotId::new();
        assert!(host.capture(changed).await.is_err());
        let mut spec = record.request.spec().unwrap();
        spec.rootfs_manifest = Some(capture.disk_manifest());
        assert!(host.ordinary_spec(&spec).is_err());
    }

    #[tokio::test]
    async fn capture_never_adopts_unrecorded_or_mutated_disk_path() {
        for recorded in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let (host, request) = fixture(temp.path());
            let capture = ManagedCaptureRequest {
                key: request.key.clone(),
                capture_id: engram_core::SnapshotId::new(),
            };
            let path = host.directory.join(format!("{}.disk", capture.capture_id));
            durable_write(&path, b"old").unwrap();
            let mut record = failed(request);
            record.capture = Some(CaptureProgress {
                request: capture.clone(),
                manifest: capture.disk_manifest(),
                artifact: if recorded {
                    Some(hash_file(&path).await.unwrap())
                } else {
                    None
                },
                authority: None,
            });
            host.persist(&record).unwrap();
            durable_write(&path, b"new").unwrap();
            assert!(host.capture(capture.clone()).await.is_err());
            assert!(host
                .chunks
                .get_manifest(capture.disk_manifest())
                .await
                .is_err());
        }
    }

    #[tokio::test]
    async fn retired_source_survives_restart_and_pinned_capture_without_old_attach() {
        let temp = tempfile::tempdir().unwrap();
        let (host, request) = fixture(temp.path());
        let key = request.key.clone();
        let id = key.context.target.sandbox_id;
        let mut record = failed(request.clone());
        let prepared = ManagedBootPreparedReceipt {
            request,
            host_incarnation: record.incarnation.clone(),
            network_identity: digest(b"network"),
            guest_ip: "192.0.2.2".parse().unwrap(),
            receipt_id: digest(b"prepared"),
        };
        let retired = ManagedBootRetiredReceipt {
            prepared: prepared.clone(),
            retained_policy: ManagedDenyPolicy::new(
                2,
                ["mcp.example".into(), "new.example".into()],
            )
            .unwrap(),
            receipt_id: digest(b"retirement"),
        };
        record.prepared = Some(prepared);
        record.state = ManagedBootHostState::Retired {
            receipt: Box::new(retired.clone()),
        };
        let path = host.directory.join(format!("{id}.retired"));
        durable_write(&path, b"retained source disk").unwrap();
        record.retired = Some(hash_file(&path).await.unwrap());
        host.persist(&record).unwrap();
        assert_eq!(host.read(key.clone()).await.unwrap(), record.state);
        assert!(host.guard_running(id).await.is_err());
        assert!(host.prepare(record.request.clone()).await.is_err());
        let next = SandboxId::new();
        host.hub
            .bind_session(key.creation.session_id, next, 2)
            .unwrap();
        host.unbind_owned(&key);
        assert_eq!(host.hub.bound_sandbox(key.creation.session_id), Some(next));
        let capture = ManagedCaptureRequest {
            key,
            capture_id: engram_core::SnapshotId::new(),
        };
        let authority = host.capture(capture.clone()).await.unwrap();
        assert_eq!(authority.disk_sha256, record.retired.unwrap().0);
        assert_eq!(host.capture(capture).await.unwrap(), authority);
        assert!(
            path.is_file(),
            "retirement disk is retained until explicit cleanup"
        );
    }

    #[tokio::test]
    async fn bootstrap_retirement_cannot_cross_a_start_intent_or_same_generation() {
        let temp = tempfile::tempdir().unwrap();
        let (host, prepare) = fixture(temp.path());
        let key = prepare.key.clone();
        let id = key.context.target.sandbox_id;
        let mut record = failed(prepare.clone());
        record.incarnation = host.incarnation.clone();
        let receipt = ManagedBootPreparedReceipt {
            request: prepare,
            host_incarnation: host.incarnation.clone(),
            network_identity: digest(b"network"),
            guest_ip: "192.0.2.2".parse().unwrap(),
            receipt_id: digest(b"prepared"),
        };
        let allocation = Allocation {
            version: 1,
            host_id: host.host_id,
            host_incarnation: host.incarnation.clone(),
            proxy_incarnation: "proxy".into(),
            session_id: key.creation.session_id,
            sandbox_id: id,
            boot_attempt: key.boot_attempt.clone(),
            attachment_generation: 1,
            guest_ip: receipt.guest_ip,
            gateway_ip: "192.0.2.1".parse().unwrap(),
            policy_revision: 1,
            policy_digest: record.request.bootstrap_policy.digest.clone(),
        };
        record.prepared = Some(receipt.clone());
        record.allocation = Some(allocation.clone());
        record.state = ManagedBootHostState::Prepared {
            receipt: Box::new(receipt.clone()),
        };
        let mut request = ManagedPolicyApply {
            expected: None,
            context: key.context.clone(),
            egress: bootstrap(&allocation, record.request.bootstrap_policy.clone()),
        };
        assert!(!bootstrap_retirement_allowed(&record, &request));
        request.context.operator_generation += 1;
        assert!(
            bootstrap_retirement_allowed(&record, &request),
            "a later generation may republish the same identity"
        );
        let mut changed = request.clone();
        changed.context.binding_hash = Some(digest(b"binding"));
        assert!(!bootstrap_retirement_allowed(&record, &changed));
        changed = request.clone();
        changed.egress.managed_deny =
            Some(ManagedDenyPolicy::new(2, ["other.example".into()]).unwrap());
        assert!(!bootstrap_retirement_allowed(&record, &changed));
        let entry = Arc::new(Mutex::new(Entry {
            record,
            attach_owner: None,
            owner: None,
            agent: None,
        }));
        host.entries.insert(id, entry.clone());
        let mut guard = entry.lock().await;
        let pending = ManagedBootHostState::StartPending {
            intent: ManagedBootStart {
                receipt: ManagedBootFinalReceipt {
                    key: key.clone(),
                    prepared_receipt_id: receipt.receipt_id,
                    finalization_digest: digest(b"final"),
                    host_incarnation: host.incarnation.clone(),
                    network_identity: receipt.network_identity,
                    receipt_id: digest(b"final receipt"),
                },
                start_id: "start".into(),
            },
        };
        let caller = host.clone();
        let race = tokio::spawn(async move { caller.apply(request).await });
        tokio::task::yield_now().await;
        // The actual start method uses this same lock before persisting intent.
        guard.record.state = pending.clone();
        drop(guard);
        assert!(race.await.unwrap().is_err());
        assert_eq!(entry.lock().await.record.state, pending);
        assert!(!host.directory.join(format!("{id}.retired")).exists());
    }

    #[tokio::test]
    async fn later_policy_apply_preserves_exact_retired_proof() {
        let temp = tempfile::tempdir().unwrap();
        let (host, request) = fixture(temp.path());
        let key = request.key.clone();
        let id = key.context.target.sandbox_id;
        let mut record = failed(request.clone());
        record.incarnation = host.incarnation.clone();
        let prepared = ManagedBootPreparedReceipt {
            request,
            host_incarnation: host.incarnation.clone(),
            network_identity: digest(b"network"),
            guest_ip: "192.0.2.2".parse().unwrap(),
            receipt_id: digest(b"prepared"),
        };
        let allocation = Allocation {
            version: 1,
            host_id: host.host_id,
            host_incarnation: host.incarnation.clone(),
            proxy_incarnation: "proxy".into(),
            session_id: key.creation.session_id,
            sandbox_id: id,
            boot_attempt: key.boot_attempt.clone(),
            attachment_generation: 1,
            guest_ip: prepared.guest_ip,
            gateway_ip: "192.0.2.1".parse().unwrap(),
            policy_revision: 1,
            policy_digest: record.request.bootstrap_policy.digest.clone(),
        };
        let mut context = key.context.clone();
        context.binding_hash = Some(digest(b"binding"));
        let applied = ManagedAppliedState::new(
            context.clone(),
            record.request.bootstrap_policy.clone(),
            host.incarnation.clone(),
            "proxy".into(),
            Some(ManagedNetworkProof {
                target: key.context.target.clone(),
                guest_ip: prepared.guest_ip,
                network_identity: prepared.network_identity.clone(),
            }),
        )
        .unwrap();
        let retained =
            ManagedDenyPolicy::new(2, ["mcp.example".into(), "new.example".into()]).unwrap();
        record.state = ManagedBootHostState::Retired {
            receipt: Box::new(ManagedBootRetiredReceipt {
                prepared: prepared.clone(),
                retained_policy: retained,
                receipt_id: digest(b"retired"),
            }),
        };
        record.prepared = Some(prepared);
        record.allocation = Some(allocation.clone());
        record.applied = Some(applied.clone());
        let path = host.directory.join(format!("{id}.retired"));
        durable_write(&path, b"exact retained disk").unwrap();
        record.retired = Some(hash_file(&path).await.unwrap());
        let expected = record.state.clone();
        host.persist(&record).unwrap();
        host.entries.insert(
            id,
            Arc::new(Mutex::new(Entry {
                record,
                attach_owner: None,
                owner: None,
                agent: None,
            })),
        );
        for revision in [2, 3] {
            let request = ManagedPolicyApply {
                expected: Some(applied.clone()),
                context: context.clone(),
                egress: bootstrap(
                    &allocation,
                    ManagedDenyPolicy::new(
                        revision,
                        [
                            "mcp.example".into(),
                            "new.example".into(),
                            "later.example".into(),
                        ],
                    )
                    .unwrap(),
                ),
            };
            assert!(matches!(
                host.apply(request).await,
                Err(SandboxError::ManagedRecoveryRequired)
            ));
            assert_eq!(host.read(key.clone()).await.unwrap(), expected);
            assert!(host.read_policy(key.context.target.clone()).await.is_err());
            assert_eq!(std::fs::read(&path).unwrap(), b"exact retained disk");
        }
        host.destroy(
            id,
            engram_core::traits::SessionFence::new(key.creation.session_id, 1),
        )
        .await
        .unwrap();
        assert_eq!(host.read(key).await.unwrap(), expected);
    }

    #[test]
    fn torn_host_records_never_authorize_an_ordinary_fallback() {
        let temp = tempfile::tempdir().unwrap();
        let (host, request) = fixture(temp.path());
        let id = request.key.context.target.sandbox_id;
        host.persist(&failed(request)).unwrap();
        let path = host.path(id);
        let bytes = std::fs::read(&path).unwrap();
        for end in 0..bytes.len() {
            std::fs::write(&path, &bytes[..end]).unwrap();
            assert!(host.read_record(id).is_err(), "offset {end}");
        }
        std::fs::write(&path, &bytes).unwrap();
        std::fs::write(path.with_extension("partial"), b"garbage sibling").unwrap();
        assert!(host.read_record(id).unwrap().is_some());
    }
}
