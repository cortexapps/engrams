//! One host owner for each managed boot. No ordinary backend map can adopt it.
use crate::{
    bindings::BindingStore, egress::HostEgress, harness::HarnessHub, managed_assets::hash_file,
};
use engram_core::{
    traits::{HarnessSink, SandboxBackend},
    types::{
        egress::SessionEgressPolicy,
        image::SecretMode,
        managed_boot::*,
        managed_create::digest,
        managed_deny::*,
        manifest::ManifestRef,
        sandbox::{
            AgentSpec, ExecRequest, ExecStream, SessionFileMetadata, SessionFileSpec,
            SessionFileStream,
        },
    },
    HostId, SandboxError, SandboxId,
};
use engram_egress_proto::private::{Allocation, RegisteredPrivateNetwork};
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
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
        staging: tempfile::TempDir,
    ) -> Result<Owner, SandboxError> {
        match self {
            #[cfg(target_os = "linux")]
            Self::Firecracker(backend) => Ok(Owner::Firecracker(Box::new(
                backend
                    .prepare_managed_cold(request.cold.clone(), spec, network, staging)
                    .await?,
            ))),
            #[cfg(target_os = "macos")]
            Self::Vz { backend, helper } => Ok(Owner::Vz(Box::new(
                backend
                    .prepare_managed_cold(
                        request.cold.clone(),
                        spec,
                        network,
                        helper.clone(),
                        staging,
                    )
                    .await?,
            ))),
        }
    }
}
type FileRead = futures::future::BoxFuture<
    'static,
    Result<(SessionFileMetadata, SessionFileStream), SandboxError>,
>;
type FileWrite = futures::future::BoxFuture<'static, Result<SessionFileMetadata, SandboxError>>;

enum Owner {
    #[cfg(test)]
    ExecTest(Box<tests::ExecOwner>),
    #[cfg(target_os = "linux")]
    Firecracker(Box<engram_sandbox_firecracker::managed::PreparedManagedFc>),
    #[cfg(target_os = "macos")]
    Vz(Box<engram_sandbox_vz::PreparedManagedVz>),
}
impl Owner {
    async fn closed_identity(&mut self) -> Result<String, SandboxError> {
        let bytes = match self {
            #[cfg(test)]
            Self::ExecTest(_) => panic!("unexpected execution-test lifecycle call"),
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
            #[cfg(test)]
            Self::ExecTest(_) => panic!("unexpected execution-test lifecycle call"),
            #[cfg(target_os = "linux")]
            Self::Firecracker(owner) => owner.release(epoch).await,
            #[cfg(target_os = "macos")]
            Self::Vz(owner) => owner.release(epoch).await,
        }
    }
    async fn ready(&mut self, epoch: u64) -> Result<(), SandboxError> {
        match self {
            #[cfg(test)]
            Self::ExecTest(owner) => {
                if owner.blocked {
                    futures::future::pending::<()>().await;
                }
                owner.ready(epoch)
            }
            #[cfg(target_os = "linux")]
            Self::Firecracker(owner) => owner.check_ready(epoch).await,
            #[cfg(target_os = "macos")]
            Self::Vz(owner) => owner.check_ready(epoch).await,
        }
    }
    async fn start(&mut self, agent: AgentSpec, sink: HarnessSink) -> Result<(), SandboxError> {
        match self {
            #[cfg(test)]
            Self::ExecTest(_) => panic!("unexpected execution-test lifecycle call"),
            #[cfg(target_os = "linux")]
            Self::Firecracker(owner) => owner.start_agent(agent, sink).await,
            #[cfg(target_os = "macos")]
            Self::Vz(owner) => owner.start_agent(agent, sink).await,
        }
    }
    fn owns(&self, request: &engram_core::types::managed_cold::ManagedColdPrepare) -> bool {
        match self {
            #[cfg(target_os = "linux")]
            Self::Firecracker(owner) => owner.owns(request),
            #[cfg(target_os = "macos")]
            Self::Vz(owner) => owner.owns(request),
            #[cfg(test)]
            Self::ExecTest(owner) => &owner.request == request,
        }
    }
    async fn exec_stream(
        &mut self,
        epoch: u64,
        cmd: ExecRequest,
    ) -> Result<ExecStream, SandboxError> {
        match self {
            #[cfg(target_os = "linux")]
            Self::Firecracker(owner) => owner.exec_stream(epoch, cmd).await,
            #[cfg(target_os = "macos")]
            Self::Vz(owner) => owner.exec_stream(epoch, cmd).await,
            #[cfg(test)]
            Self::ExecTest(owner) => owner.exec_stream(cmd),
        }
    }
    async fn cancel_exec(&mut self, epoch: u64, exec_id: String) -> Result<(), SandboxError> {
        match self {
            #[cfg(target_os = "linux")]
            Self::Firecracker(owner) => owner.cancel_exec(epoch, exec_id).await,
            #[cfg(target_os = "macos")]
            Self::Vz(owner) => owner.cancel_exec(epoch, exec_id).await,
            #[cfg(test)]
            Self::ExecTest(owner) => owner.cancel_exec(exec_id),
        }
    }
    async fn prepare_read_file(
        &mut self,
        epoch: u64,
        path: String,
        closed: tokio::sync::watch::Receiver<()>,
    ) -> Result<FileRead, SandboxError> {
        Ok(match self {
            #[cfg(target_os = "linux")]
            Self::Firecracker(owner) => {
                Box::pin(owner.prepare_read_file(epoch, path, closed).await?)
            }
            #[cfg(target_os = "macos")]
            Self::Vz(owner) => Box::pin(owner.prepare_read_file(epoch, path, closed).await?),
            #[cfg(test)]
            Self::ExecTest(owner) => {
                let result = owner.read_file(path, closed);
                Box::pin(async move { result })
            }
        })
    }
    async fn prepare_write_file(
        &mut self,
        epoch: u64,
        spec: SessionFileSpec,
        bytes: SessionFileStream,
    ) -> Result<FileWrite, SandboxError> {
        Ok(match self {
            #[cfg(target_os = "linux")]
            Self::Firecracker(owner) => {
                Box::pin(owner.prepare_write_file(epoch, spec, bytes).await?)
            }
            #[cfg(target_os = "macos")]
            Self::Vz(owner) => Box::pin(owner.prepare_write_file(epoch, spec, bytes).await?),
            #[cfg(test)]
            Self::ExecTest(owner) => owner.prepare_write_file(spec, bytes),
        })
    }
    async fn retire_disk(&mut self, path: &Path) -> Result<(), SandboxError> {
        match self {
            #[cfg(test)]
            Self::ExecTest(_) => panic!("unexpected execution-test lifecycle call"),
            #[cfg(target_os = "linux")]
            Self::Firecracker(owner) => owner.retire_disk(path).await,
            #[cfg(target_os = "macos")]
            Self::Vz(owner) => owner.retire_disk(path).await,
        }
    }
    async fn close(self) -> Result<(), SandboxError> {
        match self {
            #[cfg(test)]
            Self::ExecTest(mut owner) => {
                if let Some(wait) = owner.close_wait.take() {
                    let _ = wait.await;
                }
                if owner.close_fails {
                    Err(invalid("test close failed"))
                } else {
                    Ok(())
                }
            }
            #[cfg(target_os = "linux")]
            Self::Firecracker(owner) => (*owner).close().await,
            #[cfg(target_os = "macos")]
            Self::Vz(owner) => (*owner).close().await,
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct CaptureProgress {
    request: ManagedCaptureRequest,
    manifest: ManifestRef,
    /// Present only after retirement, durable file sync and exact hash read.
    artifact: Option<(String, u64)>,
    /// Published only after exact backend and proxy cleanup succeed.
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
    /// Replaced with each prepared owner; close signals before native I/O.
    transfer_stop: tokio::sync::watch::Sender<()>,
    record: Record,
    close_failed: bool,
    attach_owner: Option<crate::harness::ManagedHarnessOwner>,
    owner: Option<Owner>,
    agent: Option<AgentSpec>,
}

pub struct ManagedBootHostConfig {
    pub host_id: HostId,
    pub incarnation: String,
    pub directory: PathBuf,
}

/// Address slots and attachment identities have separate lifetimes.
struct NetworkSlots {
    generation: u64,
    slots: Vec<Option<u64>>,
}
impl NetworkSlots {
    fn new(capacity: usize) -> Self {
        Self {
            generation: 0,
            slots: vec![None; capacity],
        }
    }
    fn reserve(&mut self) -> Result<(usize, u64), SandboxError> {
        let slot = self.slots.iter().position(Option::is_none).ok_or_else(|| {
            SandboxError::Unavailable("managed network address capacity exhausted".into())
        })?;
        self.generation = self
            .generation
            .checked_add(1)
            .ok_or_else(|| invalid("attachment generation exhausted"))?;
        self.slots[slot] = Some(self.generation);
        Ok((slot, self.generation))
    }
    fn contains(&self, generation: u64) -> bool {
        self.slots.contains(&Some(generation))
    }
    fn release(&mut self, generation: u64) {
        if let Some(slot) = self
            .slots
            .iter_mut()
            .find(|slot| **slot == Some(generation))
        {
            *slot = None;
        }
    }
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
    networks: std::sync::Mutex<NetworkSlots>,
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
            networks: std::sync::Mutex::new(NetworkSlots::new(512)),
            binding_lock: std::sync::Mutex::new(()),
            entries: Default::default(),
        })
    }
    pub async fn read_cold_assets(
        &self,
        request: engram_core::types::managed_assets::ManagedColdAssetsRequest,
    ) -> Result<engram_core::types::managed_assets::ManagedColdAssetsReceipt, SandboxError> {
        use engram_core::types::managed_assets::{ManagedColdAssetsReceipt, ManagedColdBackend};
        request.validate().map_err(invalid)?;
        if request.host_id != self.host_id
            || self.incarnation.is_empty()
            || self.incarnation.len() > 128
        {
            return Err(invalid("host identity"));
        }
        let (backend, kernel, bundles) = match &self.backend {
            #[cfg(target_os = "linux")]
            ManagedBackend::Firecracker(backend) => (
                ManagedColdBackend::Firecracker,
                backend.config().kernel_image_path.as_path(),
                backend.bundle_dir(),
            ),
            #[cfg(target_os = "macos")]
            ManagedBackend::Vz { backend, .. } => (
                ManagedColdBackend::Vz,
                backend.managed_kernel_path(),
                backend.bundle_dir(),
            ),
        };
        let (kernel_sha256, disk_sha256, disk_size_bytes) = crate::managed_assets::inspect(
            &self.chunks,
            request.disk_manifest,
            kernel,
            bundles,
            &request.aux_ro_drives,
        )
        .await?;
        let receipt = ManagedColdAssetsReceipt {
            request,
            host_incarnation: self.incarnation.clone(),
            backend,
            kernel_sha256,
            disk_sha256,
            disk_size_bytes,
        };
        receipt.validate_for(&receipt.request).map_err(invalid)?;
        Ok(receipt)
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
    async fn close_resources(&self, entry: &mut Entry) -> Result<(), SandboxError> {
        entry.transfer_stop.send_replace(());
        if entry.close_failed {
            return Err(invalid("prior owner cleanup was uncertain"));
        }
        if let Some(owner) = entry.owner.take() {
            // A retry must not treat a consumed, failed owner as a clean absence.
            entry.close_failed = true;
            owner.close().await?;
            entry.close_failed = false;
        }
        let Some(allocation) = &entry.record.allocation else {
            return Ok(());
        };
        // Recovery must still prove old proxy cleanup before capture. It must
        // never release a current reservation with the same generation number.
        let current_incarnation = allocation.host_incarnation == self.incarnation
            && entry.record.incarnation == self.incarnation;
        if current_incarnation
            && !self
                .networks
                .lock()
                .expect("network slots")
                .contains(allocation.attachment_generation)
        {
            return Ok(());
        }
        // Each operation owns its control stream and compares the allocation
        // under the daemon's lifecycle locks. A late request cannot remove a
        // replacement, and its late ACK cannot complete a different request.
        tokio::time::timeout(
            std::time::Duration::from_secs(2 * engram_egress_proto::private::HANDOFF_SECONDS),
            self.egress.handle().retire_private_network(allocation),
        )
        .await
        .map_err(|_| invalid("proxy cleanup timed out"))?
        .map_err(invalid)?;
        if current_incarnation {
            self.networks
                .lock()
                .expect("network slots")
                .release(allocation.attachment_generation);
        }
        Ok(())
    }
    async fn fail(&self, entry: &mut Entry) {
        entry.attach_owner = None;
        entry.record.state = ManagedBootHostState::Failed {
            key: entry.record.request.key.clone(),
        };
        entry.agent = None;
        let _ = self.persist(&entry.record);
        self.unbind_owned(&entry.record.request.key);
        let _ = self.close_resources(entry).await;
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
                        transfer_stop: tokio::sync::watch::channel(()).0,
                        close_failed: false,
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
            let staging = tempfile::Builder::new()
                .prefix("staging-")
                .tempdir_in(&self.directory)?;
            let rootfs = staging.path().join("selected.ext4");
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
            let proxy_incarnation = handle.hello().await.map_err(invalid)?.incarnation;
            let mut unavailable = Vec::new();
            let selected = loop {
                let reserved = self.networks.lock().expect("network slots").reserve();
                let (slot, generation) = match reserved {
                    Ok(value) => value,
                    Err(error) => break Err(error),
                };
                let second = 18 + (slot / 256) as u8;
                let third = (slot % 256) as u8;
                let guest = std::net::Ipv4Addr::new(198, second, third, 2);
                unavailable.push(generation);
                match handle.lookup_guest(guest).await.map_err(invalid) {
                    Ok(None) => {
                        unavailable.pop();
                        break Ok((
                            guest,
                            std::net::Ipv4Addr::new(198, second, third, 1),
                            generation,
                        ));
                    }
                    Ok(Some(_)) => (),
                    Err(error) => break Err(error),
                }
            };
            for generation in unavailable {
                self.networks
                    .lock()
                    .expect("network slots")
                    .release(generation);
            }
            let (guest_ip, gateway_ip, generation) = selected?;
            let allocation = Allocation {
                version: 1,
                host_id: self.host_id,
                host_incarnation: self.incarnation.clone(),
                proxy_incarnation,
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
            let mut owner = self
                .backend
                .prepare(&request, spec, network, staging)
                .await?;
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
            entry.transfer_stop = tokio::sync::watch::channel(()).0;
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
        entry.transfer_stop.send_replace(());
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
        self.close_resources(entry).await?;
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
                            transfer_stop: tokio::sync::watch::channel(()).0,
                            close_failed: false,
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
                    entry.transfer_stop.send_replace(());
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
            // Replacement authority is durable only after exact owner and
            // proxy cleanup succeed. A failed attempt leaves an artifact to retry.
            self.close_resources(&mut entry).await?;
            progress.authority = Some(authority.clone());
            entry.record.capture = Some(progress);
            entry.record.state = ManagedBootHostState::Captured {
                authority: authority.clone(),
            };
            self.persist(&entry.record)?;
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
            this.destroy_entry(&mut entry).await
        })
        .await
        .map_err(invalid)?
    }
    /// Heartbeat tombstones name the sandbox, independently of a session fence.
    pub(crate) async fn destroy_tombstoned(
        self: &Arc<Self>,
        id: SandboxId,
    ) -> Result<(), SandboxError> {
        let this = self.clone();
        tokio::spawn(async move {
            let entry = this
                .entries
                .get(&id)
                .map(|e| e.clone())
                .ok_or_else(|| invalid("tombstone owner absent"))?;
            let mut entry = entry.lock().await;
            let key = &entry.record.request.key;
            this.check_key(&entry, key)?;
            if key.context.target.host_id != this.host_id
                || key.context.target.sandbox_id != id
                || entry
                    .owner
                    .as_ref()
                    .is_some_and(|owner| !owner.owns(&entry.record.request.cold))
            {
                return Err(invalid("tombstone owner identity"));
            }
            this.destroy_entry(&mut entry).await
        })
        .await
        .map_err(invalid)?
    }
    async fn destroy_entry(&self, entry: &mut Entry) -> Result<(), SandboxError> {
        if entry.owner.is_none()
            && matches!(
                entry.record.state,
                ManagedBootHostState::Retired { .. } | ManagedBootHostState::Captured { .. }
            )
        {
            // The physical source is already gone. Preserve its durable proof.
            return self.close_resources(entry).await;
        }
        entry.attach_owner = None;
        entry.record.state = ManagedBootHostState::Failed {
            key: entry.record.request.key.clone(),
        };
        self.persist(&entry.record)?;
        entry.agent = None;
        self.unbind_owned(&entry.record.request.key);
        self.close_resources(entry).await
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
    /// Execution is authorized by the exact live start and durable binding.
    /// Keep the lifecycle lock through submission, with bounded guest I/O.
    async fn execution_owner<'a>(
        &self,
        id: SandboxId,
        entry: &'a mut Entry,
    ) -> Result<(u64, &'a mut Owner), SandboxError> {
        let key = &entry.record.request.key;
        self.check_key(entry, key)?;
        let target = &key.context.target;
        if target.host_id != self.host_id
            || target.sandbox_id != id
            || !matches!(&entry.record.state, ManagedBootHostState::Started { intent } if intent.receipt.key == *key)
            || entry.attach_owner.is_none()
            || !self
                .bindings
                .read(target.session_id)
                .map_err(invalid)?
                .is_some_and(|binding| {
                    binding.sandbox_id == id && binding.binding_epoch == target.binding_epoch
                })
        {
            return Err(invalid("execution owner"));
        }
        let epoch = target.binding_epoch;
        let owner = entry.owner.as_mut().ok_or_else(|| invalid("owner"))?;
        if !owner.owns(&entry.record.request.cold) {
            return Err(invalid("execution owner identity"));
        }
        owner.ready(epoch).await?;
        Ok((epoch, owner))
    }
    pub async fn exec_stream(
        &self,
        id: SandboxId,
        cmd: ExecRequest,
    ) -> Result<ExecStream, SandboxError> {
        let entry = self
            .entries
            .get(&id)
            .map(|e| e.clone())
            .ok_or_else(|| invalid("owner"))?;
        let mut entry = entry.lock().await;
        tokio::time::timeout(std::time::Duration::from_secs(30), async {
            let (epoch, owner) = self.execution_owner(id, &mut entry).await?;
            owner.exec_stream(epoch, cmd).await
        })
        .await
        .map_err(|_| {
            SandboxError::Unavailable(
                "managed execution submission timed out; retry with the same execution ticket"
                    .into(),
            )
        })?
    }
    pub async fn cancel_exec(&self, id: SandboxId, exec_id: String) -> Result<(), SandboxError> {
        let entry = self
            .entries
            .get(&id)
            .map(|e| e.clone())
            .ok_or_else(|| invalid("owner"))?;
        let mut entry = entry.lock().await;
        tokio::time::timeout(std::time::Duration::from_secs(30), async {
            let (epoch, owner) = self.execution_owner(id, &mut entry).await?;
            owner.cancel_exec(epoch, exec_id).await
        })
        .await
        .map_err(|_| SandboxError::Unavailable("managed execution cancellation timed out".into()))?
    }
    async fn transfer<T>(
        mut closed: tokio::sync::watch::Receiver<()>,
        transfer: impl std::future::Future<Output = Result<T, SandboxError>>,
    ) -> Result<T, SandboxError> {
        tokio::select! {
            biased;
            _ = closed.changed() => Err(SandboxError::Unavailable("managed file owner closed".into())),
            result = transfer => result,
        }
    }
    pub async fn read_file(
        &self,
        id: SandboxId,
        path: String,
    ) -> Result<(SessionFileMetadata, SessionFileStream), SandboxError> {
        let (closed, transfer) = {
            let entry = self
                .entries
                .get(&id)
                .map(|e| e.clone())
                .ok_or_else(|| invalid("owner"))?;
            let mut entry = entry.lock().await;
            let closed = entry.transfer_stop.subscribe();
            let transfer = tokio::time::timeout(std::time::Duration::from_secs(30), async {
                let (epoch, owner) = self.execution_owner(id, &mut entry).await?;
                owner.prepare_read_file(epoch, path, closed.clone()).await
            })
            .await
            .map_err(|_| {
                SandboxError::Unavailable("managed file read submission timed out".into())
            })??;
            (closed, transfer)
        };
        Self::transfer(closed, transfer).await
    }
    pub async fn write_file(
        &self,
        id: SandboxId,
        spec: SessionFileSpec,
        bytes: SessionFileStream,
    ) -> Result<SessionFileMetadata, SandboxError> {
        let (closed, transfer) = {
            let entry = self
                .entries
                .get(&id)
                .map(|e| e.clone())
                .ok_or_else(|| invalid("owner"))?;
            let mut entry = entry.lock().await;
            let closed = entry.transfer_stop.subscribe();
            let transfer = tokio::time::timeout(std::time::Duration::from_secs(30), async {
                let (epoch, owner) = self.execution_owner(id, &mut entry).await?;
                owner.prepare_write_file(epoch, spec, bytes).await
            })
            .await
            .map_err(|_| {
                SandboxError::Unavailable("managed file write submission timed out".into())
            })??;
            (closed, transfer)
        };
        Self::transfer(closed, transfer).await
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
            let network_reserved = entry.record.allocation.as_ref().is_some_and(|allocation| {
                allocation.host_incarnation == self.incarnation
                    && entry.record.incarnation == self.incarnation
                    && self
                        .networks
                        .lock()
                        .expect("network slots")
                        .contains(allocation.attachment_generation)
            });
            if entry.owner.is_some()
                || entry.close_failed
                || network_reserved
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

    struct TestFile {
        metadata: SessionFileMetadata,
        chunks: Vec<bytes::Bytes>,
    }
    pub(super) struct ExecOwner {
        pub(super) close_fails: bool,
        pub(super) close_wait: Option<tokio::sync::oneshot::Receiver<()>>,
        epoch: u64,
        live: bool,
        pub(super) blocked: bool,
        id: SandboxId,
        pub(super) request: ManagedColdPrepare,
        file: Arc<std::sync::Mutex<Option<TestFile>>>,
        file_read_fails: bool,
        file_read_stalls: bool,
    }
    impl ExecOwner {
        pub(super) fn exec_stream(&self, cmd: ExecRequest) -> Result<ExecStream, SandboxError> {
            Ok(ExecStream {
                sandbox_id: self.id,
                exec_id: cmd.exec_id.clone().unwrap(),
                events: Box::pin(futures::stream::iter([
                    engram_core::types::sandbox::ExecEvent::Stdout(bytes::Bytes::from(
                        serde_json::to_vec(&cmd).unwrap(),
                    )),
                ])),
            })
        }
        pub(super) fn cancel_exec(&self, exec_id: String) -> Result<(), SandboxError> {
            if exec_id == "ticket-1" {
                Ok(())
            } else {
                Err(invalid("wrong cancel ticket"))
            }
        }
        pub(super) fn prepare_write_file(
            &self,
            spec: SessionFileSpec,
            bytes: SessionFileStream,
        ) -> FileWrite {
            let file = self.file.clone();
            Box::pin(async move {
                use futures::TryStreamExt;
                let chunks = bytes.try_collect::<Vec<_>>().await?;
                let metadata = SessionFileMetadata {
                    path: spec.path,
                    size_bytes: spec.size_bytes,
                    sha256: spec.sha256,
                };
                *file.lock().unwrap() = Some(TestFile {
                    metadata: metadata.clone(),
                    chunks,
                });
                Ok(metadata)
            })
        }
        pub(super) fn read_file(
            &self,
            path: String,
            closed: tokio::sync::watch::Receiver<()>,
        ) -> Result<(SessionFileMetadata, SessionFileStream), SandboxError> {
            let file = self.file.lock().unwrap();
            let TestFile { metadata, chunks } = file.as_ref().ok_or(SandboxError::NotFound)?;
            if path != metadata.path {
                return Err(SandboxError::NotFound);
            }
            let metadata = metadata.clone();
            let mut events = chunks.iter().cloned().map(Ok).collect::<Vec<_>>();
            if self.file_read_fails {
                events.push(Err(SandboxError::Unavailable("owner stream closed".into())));
            }
            let events: SessionFileStream = if self.file_read_stalls {
                Box::pin(futures::stream::pending())
            } else {
                Box::pin(futures::stream::iter(events))
            };
            let stream = futures::stream::unfold(Some((events, closed)), |state| async move {
                use futures::StreamExt;
                let (mut events, mut closed) = state?;
                tokio::select! {
                    biased;
                    _ = closed.changed() => Some((Err(SandboxError::Unavailable("owner stream closed".into())), None)),
                    item = events.next() => item.map(|item| (item, Some((events, closed)))),
                }
            });
            Ok((metadata, Box::pin(stream)))
        }
        pub(super) fn ready(&self, epoch: u64) -> Result<(), SandboxError> {
            if self.live && self.epoch == epoch {
                Ok(())
            } else {
                Err(invalid("test owner closed or epoch changed"))
            }
        }
    }

    fn install_exec_owner(
        host: &Arc<ManagedBootHost>,
        request: &ManagedBootPrepare,
    ) -> Arc<Mutex<Entry>> {
        let key = &request.key;
        let id = key.context.target.sandbox_id;
        durable_write(
            &host
                .bindings
                .directory()
                .join(format!("{id}.managed-sandbox")),
            b"1",
        )
        .unwrap();
        host.hub
            .bind_session(key.creation.session_id, id, 1)
            .unwrap();
        let intent = issued_start(request);
        let mut record = failed(request.clone());
        record.incarnation = host.incarnation.clone();
        record.state = ManagedBootHostState::Started {
            intent: intent.clone(),
        };
        let entry = Arc::new(Mutex::new(Entry {
            transfer_stop: tokio::sync::watch::channel(()).0,
            close_failed: false,
            record,
            attach_owner: Some(host.hub.managed_owner(intent)),
            owner: Some(Owner::ExecTest(Box::new(ExecOwner {
                close_fails: false,
                close_wait: None,
                epoch: 1,
                live: true,
                blocked: false,
                id,
                request: request.cold.clone(),
                file: Arc::new(std::sync::Mutex::new(None)),
                file_read_fails: false,
                file_read_stalls: false,
            }))),
            agent: None,
        }));
        host.entries.insert(id, entry.clone());
        entry
    }
    fn exec_request() -> ExecRequest {
        ExecRequest {
            command: vec!["printf".into(), "ready".into()],
            stdin: Some(b"input".to_vec()),
            env: [("CHECK".into(), "value".into())].into(),
            workdir: Some("/workspace".into()),
            timeout: Some(std::time::Duration::from_secs(3)),
            exec_id: Some("ticket-1".into()),
            stdout_offset: Some(7),
            stderr_offset: Some(2),
            wake: Some(false),
        }
    }
    #[tokio::test]
    async fn managed_exec_uses_current_live_owner() {
        use futures::StreamExt;
        let temp = tempfile::tempdir().unwrap();
        let (host, request) = fixture(temp.path());
        let id = request.key.context.target.sandbox_id;
        install_exec_owner(&host, &request);
        let local =
            crate::host_client::LocalHostClient::new(host.ordinary.clone(), host.hub.clone())
                .with_managed_boot(host);
        let mut stream = local
            .exec_stream(id, exec_request())
            .await
            .expect("marked managed execution uses its live owner");
        assert_eq!(stream.sandbox_id, id);
        assert_eq!(stream.exec_id, "ticket-1");
        let Some(engram_core::types::sandbox::ExecEvent::Stdout(bytes)) =
            stream.events.next().await
        else {
            panic!("owner output missing")
        };
        let routed: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            routed,
            serde_json::json!({
                "command": ["printf", "ready"], "stdin": [105, 110, 112, 117, 116],
                "env": {"CHECK": "value"}, "workdir": "/workspace", "timeout": {"secs": 3, "nanos": 0},
                "exec_id": "ticket-1", "stdout_offset": 7, "stderr_offset": 2, "wake": false,
            })
        );
        local.cancel_exec(id, "ticket-1".into()).await.unwrap();
        assert!(local.cancel_exec(id, "wrong-ticket".into()).await.is_err());
        assert!(matches!(
            local.probe_sandbox(id).await,
            Err(SandboxError::InvalidSpec(_))
        ));
        assert!(matches!(
            local.exec_stream(SandboxId::new(), exec_request()).await,
            Err(SandboxError::NotFound)
        ));
        assert!(matches!(
            local.cancel_exec(SandboxId::new(), "ticket-1".into()).await,
            Err(SandboxError::NotFound)
        ));
        local
            .destroy(
                id,
                engram_core::traits::SessionFence::new(request.key.creation.session_id, 1),
            )
            .await
            .unwrap();
        assert!(matches!(
            local.exec_stream(id, exec_request()).await,
            Err(SandboxError::InvalidSpec(_))
        ));
        assert!(matches!(
            local.cancel_exec(id, "ticket-1".into()).await,
            Err(SandboxError::InvalidSpec(_))
        ));
    }
    #[tokio::test]
    async fn managed_exec_denies_stale_closed_and_recovered_owners() {
        for case in 0..11 {
            let temp = tempfile::tempdir().unwrap();
            let (host, request) = fixture(temp.path());
            let id = request.key.context.target.sandbox_id;
            let entry = install_exec_owner(&host, &request);
            {
                let mut entry = entry.lock().await;
                match case {
                    0 => {
                        if let Some(Owner::ExecTest(owner)) = &mut entry.owner {
                            owner.live = false;
                        }
                    }
                    1 => {
                        if let Some(Owner::ExecTest(owner)) = &mut entry.owner {
                            owner.epoch = 2;
                        }
                    }
                    2 => entry.owner = None,
                    3 => entry.record.incarnation = "old-host".into(),
                    4 => {
                        entry.record.state = ManagedBootHostState::Failed {
                            key: request.key.clone(),
                        }
                    }
                    5 => host
                        .hub
                        .bind_session(request.key.creation.session_id, id, 2)
                        .unwrap(),
                    6 => {
                        if let ManagedBootHostState::Started { intent } = &mut entry.record.state {
                            intent.receipt.key.boot_attempt = "other-boot".into();
                        }
                    }
                    7 => entry.attach_owner = None,
                    8 => entry.record.request.key.context.target.host_id = HostId::new(),
                    9 => entry.record.request.key.context.target.sandbox_id = SandboxId::new(),
                    _ => {
                        if let Some(Owner::ExecTest(owner)) = &mut entry.owner {
                            owner.request.boot_attempt = "other-owner".into();
                        }
                    }
                }
            }
            let local =
                crate::host_client::LocalHostClient::new(host.ordinary.clone(), host.hub.clone())
                    .with_managed_boot(host.clone());
            assert!(
                matches!(
                    local.exec_stream(id, exec_request()).await,
                    Err(SandboxError::InvalidSpec(_))
                ),
                "case {case}"
            );
            assert!(
                matches!(
                    local.cancel_exec(id, "ticket-1".into()).await,
                    Err(SandboxError::InvalidSpec(_))
                ),
                "case {case}"
            );
            host.entries.remove(&id);
            assert!(
                matches!(
                    local.exec_stream(id, exec_request()).await,
                    Err(SandboxError::InvalidSpec(_))
                ),
                "lost owner {case}"
            );
        }
    }
    #[tokio::test(start_paused = true)]
    async fn managed_exec_submission_timeout_releases_lifecycle_lock() {
        let temp = tempfile::tempdir().unwrap();
        let (host, request) = fixture(temp.path());
        let id = request.key.context.target.sandbox_id;
        let entry = install_exec_owner(&host, &request);
        if let Some(Owner::ExecTest(owner)) = &mut entry.lock().await.owner {
            owner.blocked = true;
        }
        let local =
            crate::host_client::LocalHostClient::new(host.ordinary.clone(), host.hub.clone())
                .with_managed_boot(host);
        assert!(matches!(
            local.exec_stream(id, exec_request()).await,
            Err(SandboxError::Unavailable(_))
        ));
        assert!(entry.try_lock().is_ok());
        assert!(matches!(
            local.cancel_exec(id, "ticket-1".into()).await,
            Err(SandboxError::Unavailable(_))
        ));
        assert!(entry.try_lock().is_ok());
    }

    #[derive(Default)]
    struct FileBackendCalls(std::sync::atomic::AtomicUsize, Vec<SandboxId>);
    #[async_trait::async_trait]
    impl SandboxBackend for FileBackendCalls {
        async fn exec_stream(
            &self,
            _: SandboxId,
            _: ExecRequest,
        ) -> Result<ExecStream, SandboxError> {
            panic!("unexpected ordinary execution")
        }
        fn snapshot_path_for(&self, _: engram_core::SnapshotId) -> PathBuf {
            panic!("unexpected ordinary snapshot path")
        }
        async fn create(
            &self,
            _: SandboxId,
            _: engram_core::types::sandbox::SandboxSpec,
        ) -> Result<(), SandboxError> {
            panic!("unexpected ordinary create")
        }
        async fn snapshot(
            &self,
            _: SandboxId,
        ) -> Result<engram_core::types::snapshot::SnapshotMetadata, SandboxError> {
            panic!("unexpected ordinary snapshot")
        }
        async fn restore(
            &self,
            _: SandboxId,
            _: engram_core::types::snapshot::SnapshotMetadata,
            _: Option<engram_core::types::sandbox::SessionDisks>,
        ) -> Result<(), SandboxError> {
            panic!("unexpected ordinary restore")
        }
        async fn destroy(&self, _: SandboxId) -> Result<(), SandboxError> {
            panic!("unexpected ordinary destroy")
        }
        async fn list(&self) -> Result<Vec<SandboxId>, SandboxError> {
            Ok(self.1.clone())
        }
        async fn read_file(
            &self,
            _: SandboxId,
            _: String,
        ) -> Result<(SessionFileMetadata, SessionFileStream), SandboxError> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Err(SandboxError::NotFound)
        }
        async fn write_file(
            &self,
            _: SandboxId,
            _: SessionFileSpec,
            _: SessionFileStream,
        ) -> Result<SessionFileMetadata, SandboxError> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Err(SandboxError::NotFound)
        }
    }
    fn file_spec() -> SessionFileSpec {
        SessionFileSpec {
            path: "/tmp/uploads/file.bin".into(),
            size_bytes: 11,
            sha256: "a".repeat(64),
            mode: Some(0o640),
        }
    }
    fn file_bytes() -> SessionFileStream {
        Box::pin(futures::stream::iter([Ok(bytes::Bytes::from_static(
            b"owner-bytes",
        ))]))
    }
    fn file_client(
        host: &Arc<ManagedBootHost>,
        ordinary: &Arc<FileBackendCalls>,
    ) -> crate::host_client::LocalHostClient {
        crate::host_client::LocalHostClient::new(ordinary.clone(), host.hub.clone())
            .with_managed_boot(host.clone())
    }
    #[tokio::test]
    async fn inventory_propagates_unknown_managed_binding_state() {
        let temp = tempfile::tempdir().unwrap();
        let (host, request) = fixture(temp.path());
        let managed = request.key.context.target.sandbox_id;
        install_exec_owner(&host, &request);
        let ordinary = SandboxId::new();
        let backend = Arc::new(FileBackendCalls(
            Default::default(),
            vec![ordinary, managed],
        ));
        let local = file_client(&host, &backend);
        // The managed ID comes from its live owner, once, after ordinary IDs.
        assert_eq!(local.list().await.unwrap(), vec![ordinary, managed]);
        let corrupt = host.bindings.directory().join("invalid.managed-required");
        std::fs::write(&corrupt, b"1").unwrap();
        assert!(matches!(
            local.list().await,
            Err(SandboxError::InvalidSpec(_))
        ));
        std::fs::remove_file(corrupt).unwrap();
        assert_eq!(local.list().await.unwrap(), vec![ordinary, managed]);
    }
    #[tokio::test]
    async fn tombstone_retries_proxy_cleanup_before_inventory_ack() {
        let temp = tempfile::tempdir_in("/tmp").unwrap();
        let (host, request) = fixture(temp.path());
        let id = request.cold.sandbox_id;
        let entry = install_exec_owner(&host, &request);
        let generation = reserve_test_network(&host, &mut *entry.lock().await);
        let local = file_client(&host, &Arc::new(FileBackendCalls::default()));
        let retained = host.directory.join(format!("{id}.retired"));
        durable_write(&retained, b"retained disk").unwrap();
        let replacement = SandboxId::new();
        host.hub
            .bind_session(request.cold.session_id, replacement, 2)
            .unwrap();
        let (server, mut removals, _) = cleanup_proxy(temp.path());
        for succeeds in [false, true] {
            let cleanup = local.destroy_tombstoned(id);
            let ack = async {
                let reply = removals.recv().await.unwrap();
                assert!(host.networks.lock().unwrap().contains(generation));
                reply
                    .send(if succeeds {
                        engram_egress_proto::FromProxyd::Ok
                    } else {
                        engram_egress_proto::FromProxyd::Err("still draining".into())
                    })
                    .unwrap();
            };
            let (result, ()) = tokio::join!(cleanup, ack);
            assert_eq!(result.is_ok(), succeeds);
            assert_eq!(local.list().await.unwrap().contains(&id), !succeeds);
            assert_eq!(
                host.networks.lock().unwrap().contains(generation),
                !succeeds
            );
            let entry = entry.lock().await;
            assert!(entry.owner.is_none());
            assert!(entry.attach_owner.is_none());
            assert!(matches!(
                entry.record.state,
                ManagedBootHostState::Failed { .. }
            ));
        }
        local.destroy_tombstoned(id).await.unwrap();
        assert_eq!(
            host.hub.bound_sandbox(request.cold.session_id),
            Some(replacement)
        );
        assert_eq!(std::fs::read(retained).unwrap(), b"retained disk");
        assert!(host.bindings.managed_sandbox_required(id).unwrap());
        assert!(host.read_record(id).unwrap().is_some());
        server.abort();
        let _ = server.await;
    }

    #[tokio::test]
    async fn tombstone_uncertain_native_close_remains_in_inventory() {
        let temp = tempfile::tempdir().unwrap();
        let (host, request) = fixture(temp.path());
        let id = request.cold.sandbox_id;
        let entry = install_exec_owner(&host, &request);
        if let Some(Owner::ExecTest(owner)) = &mut entry.lock().await.owner {
            owner.close_fails = true;
        }
        let local = file_client(&host, &Arc::new(FileBackendCalls::default()));
        // Generic callers still need a session fence, even with a live owner.
        assert!(local
            .destroy(id, engram_core::traits::SessionFence::unfenced())
            .await
            .is_err());
        assert!(entry.lock().await.owner.is_some());
        for _ in 0..2 {
            assert!(local.destroy_tombstoned(id).await.is_err());
            assert_eq!(local.list().await.unwrap(), vec![id]);
            assert!(entry.lock().await.close_failed);
        }
    }

    #[tokio::test]
    async fn tombstone_rejects_unknown_or_changed_managed_owner() {
        let temp = tempfile::tempdir().unwrap();
        let (host, request) = fixture(temp.path());
        let id = request.cold.sandbox_id;
        let entry = install_exec_owner(&host, &request);
        let local = file_client(&host, &Arc::new(FileBackendCalls::default()));
        entry.lock().await.record.incarnation = "previous-host".into();
        assert!(local.destroy_tombstoned(id).await.is_err());
        assert!(entry.lock().await.owner.is_some());
        entry.lock().await.record.incarnation = host.incarnation.clone();
        if let Some(Owner::ExecTest(owner)) = &mut entry.lock().await.owner {
            owner.request.boot_attempt = "different-owner".into();
        }
        assert!(local.destroy_tombstoned(id).await.is_err());
        assert!(entry.lock().await.owner.is_some());
        host.entries.remove(&id);
        assert!(local.destroy_tombstoned(id).await.is_err());
        assert!(host.bindings.managed_sandbox_required(id).unwrap());
        let corrupt = host.bindings.directory().join("invalid.managed-required");
        std::fs::write(&corrupt, b"1").unwrap();
        assert!(local.destroy_tombstoned(SandboxId::new()).await.is_err());
        assert_eq!(std::fs::read(corrupt).unwrap(), b"1");
    }
    #[tokio::test]
    async fn managed_file_io_uses_current_owner() {
        use futures::TryStreamExt;
        let temp = tempfile::tempdir().unwrap();
        let (host, request) = fixture(temp.path());
        let id = request.key.context.target.sandbox_id;
        install_exec_owner(&host, &request);
        let ordinary = Arc::new(FileBackendCalls::default());
        let local = file_client(&host, &ordinary);
        let metadata = local
            .write_file(id, file_spec(), file_bytes())
            .await
            .expect("managed upload uses its current owner");
        assert_eq!(metadata.path, "/tmp/uploads/file.bin");
        assert_eq!(metadata.size_bytes, 11);
        assert_eq!(metadata.sha256, "a".repeat(64));
        let (download, stream) = local
            .read_file(id, metadata.path.clone())
            .await
            .expect("managed download uses its current owner");
        assert_eq!(download, metadata);
        assert_eq!(
            stream.try_collect::<Vec<_>>().await.unwrap(),
            [bytes::Bytes::from_static(b"owner-bytes")]
        );
        assert_eq!(ordinary.0.load(std::sync::atomic::Ordering::SeqCst), 0);
    }
    #[tokio::test(start_paused = true)]
    async fn managed_file_io_rejects_stale_or_closed_owner() {
        for case in 0..13 {
            let temp = tempfile::tempdir().unwrap();
            let (host, request) = fixture(temp.path());
            let id = request.key.context.target.sandbox_id;
            let entry = install_exec_owner(&host, &request);
            {
                let mut entry = entry.lock().await;
                match case {
                    0 => {
                        if let Some(Owner::ExecTest(owner)) = &mut entry.owner {
                            owner.live = false;
                        }
                    }
                    1 => {
                        if let Some(Owner::ExecTest(owner)) = &mut entry.owner {
                            owner.epoch = 2;
                        }
                    }
                    2 => entry.owner = None,
                    3 => entry.record.incarnation = "old-host".into(),
                    4 => {
                        entry.record.state = ManagedBootHostState::Failed {
                            key: request.key.clone(),
                        }
                    }
                    5 => host
                        .hub
                        .bind_session(request.key.creation.session_id, id, 2)
                        .unwrap(),
                    6 => {
                        if let ManagedBootHostState::Started { intent } = &mut entry.record.state {
                            intent.receipt.key.boot_attempt = "other-boot".into();
                        }
                    }
                    7 => entry.attach_owner = None,
                    8 => entry.record.request.key.context.target.host_id = HostId::new(),
                    9 => entry.record.request.key.context.target.sandbox_id = SandboxId::new(),
                    10 => {
                        if let Some(Owner::ExecTest(owner)) = &mut entry.owner {
                            owner.request.boot_attempt = "replacement".into();
                        }
                    }
                    11 => {
                        if let Some(Owner::ExecTest(owner)) = &mut entry.owner {
                            owner.blocked = true;
                        }
                    }
                    _ => {
                        host.entries.remove(&id);
                    }
                }
            }
            let ordinary = Arc::new(FileBackendCalls::default());
            let local = file_client(&host, &ordinary);
            let read = local
                .read_file(id, file_spec().path)
                .await
                .err()
                .expect("stale read denied");
            let write = local
                .write_file(id, file_spec(), file_bytes())
                .await
                .unwrap_err();
            if case == 11 {
                assert!(
                    matches!(read, SandboxError::Unavailable(_)),
                    "blocked read readiness is bounded"
                );
                assert!(
                    matches!(write, SandboxError::Unavailable(_)),
                    "blocked write readiness is bounded"
                );
            } else {
                assert!(
                    matches!(read, SandboxError::InvalidSpec(_)),
                    "read case {case}"
                );
                assert!(
                    matches!(write, SandboxError::InvalidSpec(_)),
                    "write case {case}"
                );
            }
            assert!(
                entry.try_lock().is_ok(),
                "submission must release the lifecycle lock"
            );
            assert_eq!(
                ordinary.0.load(std::sync::atomic::Ordering::SeqCst),
                0,
                "case {case}"
            );
        }
    }
    #[tokio::test]
    async fn managed_file_io_never_uses_ordinary_backend() {
        let temp = tempfile::tempdir().unwrap();
        let (host, request) = fixture(temp.path());
        let id = request.key.context.target.sandbox_id;
        let entry = install_exec_owner(&host, &request);
        entry.lock().await.owner = None;
        let ordinary = Arc::new(FileBackendCalls::default());
        let local = file_client(&host, &ordinary);
        assert!(matches!(
            local.read_file(id, file_spec().path).await,
            Err(SandboxError::InvalidSpec(_))
        ));
        assert!(matches!(
            local.write_file(id, file_spec(), file_bytes()).await,
            Err(SandboxError::InvalidSpec(_))
        ));
        let no_boot = crate::host_client::LocalHostClient::new(ordinary.clone(), host.hub.clone());
        assert!(matches!(
            no_boot.read_file(id, file_spec().path).await,
            Err(SandboxError::InvalidSpec(_))
        ));
        assert!(matches!(
            no_boot.write_file(id, file_spec(), file_bytes()).await,
            Err(SandboxError::InvalidSpec(_))
        ));
        assert_eq!(ordinary.0.load(std::sync::atomic::Ordering::SeqCst), 0);
        let ordinary_id = SandboxId::new();
        assert!(matches!(
            local.read_file(ordinary_id, file_spec().path).await,
            Err(SandboxError::NotFound)
        ));
        assert!(matches!(
            local
                .write_file(ordinary_id, file_spec(), file_bytes())
                .await,
            Err(SandboxError::NotFound)
        ));
        assert_eq!(
            ordinary.0.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "ordinary IDs still use the ordinary backend"
        );
    }

    #[tokio::test]
    async fn managed_file_io_cancellation_and_replacement_keep_stream_owner() {
        use futures::TryStreamExt;
        let temp = tempfile::tempdir().unwrap();
        let (host, request) = fixture(temp.path());
        let id = request.key.context.target.sandbox_id;
        let entry = install_exec_owner(&host, &request);
        let ordinary = Arc::new(FileBackendCalls::default());
        let local = file_client(&host, &ordinary);
        local
            .write_file(id, file_spec(), file_bytes())
            .await
            .unwrap();
        let (_, opened) = local.read_file(id, file_spec().path).await.unwrap();
        if let Some(Owner::ExecTest(owner)) = &mut entry.lock().await.owner {
            owner.file_read_fails = true;
        }
        let (_, mut failed_stream) = local.read_file(id, file_spec().path).await.unwrap();
        let mut upload =
            Box::pin(local.write_file(id, file_spec(), Box::pin(futures::stream::pending())));
        tokio::select! {
            result = &mut upload => panic!("upload must wait for its source: {result:?}"),
            _ = tokio::task::yield_now() => {},
        }
        assert!(
            entry.try_lock().is_ok(),
            "body transfer releases the lifecycle lock"
        );
        drop(upload);
        assert!(
            entry.try_lock().is_ok(),
            "cancelled upload releases its lifecycle lock"
        );
        let replacement = install_exec_owner(&host, &request);
        local
            .write_file(
                id,
                file_spec(),
                Box::pin(futures::stream::iter([Ok(bytes::Bytes::from_static(
                    b"other-bytes",
                ))])),
            )
            .await
            .unwrap();
        assert_eq!(
            opened.try_collect::<Vec<_>>().await.unwrap(),
            [bytes::Bytes::from_static(b"owner-bytes")],
            "opened download never reads the replacement owner"
        );
        use futures::StreamExt;
        assert_eq!(
            failed_stream.next().await.unwrap().unwrap(),
            bytes::Bytes::from_static(b"owner-bytes")
        );
        assert!(failed_stream.next().await.unwrap().is_err());
        assert!(
            failed_stream.next().await.is_none(),
            "failed stream must end without reading its replacement"
        );
        let (_, current) = local.read_file(id, file_spec().path).await.unwrap();
        assert_eq!(
            current.try_collect::<Vec<_>>().await.unwrap(),
            [bytes::Bytes::from_static(b"other-bytes")]
        );
        assert!(local
            .write_file(
                id,
                file_spec(),
                Box::pin(futures::stream::iter([Err(SandboxError::Unavailable(
                    "source stopped".into()
                ))]))
            )
            .await
            .is_err());
        assert!(replacement.try_lock().is_ok());
        assert_eq!(ordinary.0.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn managed_file_progressing_upload_can_exceed_submission_timeout() {
        let temp = tempfile::tempdir().unwrap();
        let (host, request) = fixture(temp.path());
        let id = request.key.context.target.sandbox_id;
        install_exec_owner(&host, &request);
        let ordinary = Arc::new(FileBackendCalls::default());
        let local = file_client(&host, &ordinary);
        let source = futures::stream::unfold(0, |n| async move {
            if n == 4 {
                return None;
            }
            tokio::time::sleep(std::time::Duration::from_secs(10)).await;
            Some((Ok(bytes::Bytes::from_static(b"a")), n + 1))
        });
        let mut spec = file_spec();
        spec.size_bytes = 4;
        let result = local
            .write_file(id, spec, Box::pin(source))
            .await
            .expect("progressing body has no fixed submission deadline");
        assert_eq!(result.size_bytes, 4);
        assert_eq!(ordinary.0.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn managed_file_close_cancels_stalled_upload_outside_lifecycle_lock() {
        let temp = tempfile::tempdir().unwrap();
        let (host, request) = fixture(temp.path());
        let id = request.key.context.target.sandbox_id;
        let entry = install_exec_owner(&host, &request);
        let ordinary = Arc::new(FileBackendCalls::default());
        let local = file_client(&host, &ordinary);
        let mut upload =
            Box::pin(local.write_file(id, file_spec(), Box::pin(futures::stream::pending())));
        tokio::select! {
            result = &mut upload => panic!("stalled upload must wait for source: {result:?}"),
            _ = tokio::task::yield_now() => {},
        }
        let mut entry = entry
            .try_lock()
            .expect("body transfer must release lifecycle lock");
        let (release_close, native_close) = tokio::sync::oneshot::channel();
        if let Some(Owner::ExecTest(owner)) = &mut entry.owner {
            owner.close_wait = Some(native_close);
        }
        let mut close = Box::pin(host.close_resources(&mut entry));
        tokio::select! {
            result = &mut close => panic!("native close must wait: {result:?}"),
            _ = tokio::task::yield_now() => {},
        }
        assert!(matches!(
            tokio::time::timeout(std::time::Duration::from_secs(1), &mut upload).await,
            Ok(Err(SandboxError::Unavailable(_)))
        ));
        release_close.send(()).unwrap();
        close.await.unwrap();
        assert!(entry.owner.is_none());
        drop(entry);
        let replacement = install_exec_owner(&host, &request);
        assert!(replacement.try_lock().is_ok());
        local
            .write_file(id, file_spec(), file_bytes())
            .await
            .unwrap();
        assert_eq!(ordinary.0.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn managed_file_close_stalled_read_before_native_close_completes() {
        use futures::StreamExt;
        let temp = tempfile::tempdir().unwrap();
        let (host, request) = fixture(temp.path());
        let id = request.key.context.target.sandbox_id;
        let entry = install_exec_owner(&host, &request);
        let ordinary = Arc::new(FileBackendCalls::default());
        let local = file_client(&host, &ordinary);
        local
            .write_file(id, file_spec(), file_bytes())
            .await
            .unwrap();
        let (release_close, native_close) = tokio::sync::oneshot::channel();
        if let Some(Owner::ExecTest(owner)) = &mut entry.lock().await.owner {
            owner.file_read_stalls = true;
            owner.close_wait = Some(native_close);
        }
        let (_, mut stream) = local.read_file(id, file_spec().path).await.unwrap();
        let mut entry = entry.lock().await;
        let mut close = Box::pin(host.close_resources(&mut entry));
        tokio::select! {
            result = &mut close => panic!("native close must wait: {result:?}"),
            _ = tokio::task::yield_now() => {},
        }
        assert!(
            matches!(
                tokio::time::timeout(std::time::Duration::from_secs(1), stream.next()).await,
                Ok(Some(Err(SandboxError::Unavailable(_))))
            ),
            "old owner close must end the returned read before native close completes"
        );
        release_close.send(()).unwrap();
        close.await.unwrap();
        drop(entry);
        install_exec_owner(&host, &request);
        local
            .write_file(id, file_spec(), file_bytes())
            .await
            .unwrap();
        assert!(
            stream.next().await.is_none(),
            "closed read cannot adopt replacement bytes"
        );
        let (_, mut current) = local.read_file(id, file_spec().path).await.unwrap();
        assert_eq!(
            current.next().await.unwrap().unwrap(),
            bytes::Bytes::from_static(b"owner-bytes")
        );
        assert_eq!(ordinary.0.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

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
                    engram_sandbox_vz::VzConfig::with_kernel(path.join("kernel"))
                        .with_bundle_dir(path),
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
            engram_sandbox_firecracker::FirecrackerBackend::new(path.join("native"), {
                let mut config =
                    engram_sandbox_firecracker::FirecrackerConfig::with_kernel(path.join("kernel"));
                config.bundle_dir = path.to_owned();
                config
            }),
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
    #[test]
    fn address_slots_reuse_retired_capacity_without_reusing_attachment_identity() {
        let mut slots = NetworkSlots::new(2);
        let live = slots.reserve().unwrap();
        let mut previous = live.1;
        for _ in 0..8 {
            let current = slots.reserve().expect("retired capacity is reusable");
            assert_ne!(current.0, live.0);
            assert!(current.1 > previous);
            assert!(slots.reserve().is_err(), "live slots must remain exclusive");
            if previous != live.1 {
                slots.release(previous);
            }
            assert!(
                slots.contains(current.1),
                "stale release must not retire a replacement"
            );
            slots.release(current.1);
            previous = current.1;
        }
    }

    #[test]
    fn concurrent_address_reservations_are_unique() {
        let slots = Arc::new(std::sync::Mutex::new(NetworkSlots::new(8)));
        let owners: Vec<_> = (0..8)
            .map(|_| {
                let slots = slots.clone();
                std::thread::spawn(move || slots.lock().unwrap().reserve().unwrap())
            })
            .collect();
        let reservations: std::collections::HashSet<_> = owners
            .into_iter()
            .map(|owner| owner.join().unwrap().0)
            .collect();
        assert_eq!(reservations.len(), 8);
        assert!(slots.lock().unwrap().reserve().is_err());
    }

    fn reserve_test_network(host: &ManagedBootHost, entry: &mut Entry) -> u64 {
        let (slot, generation) = host.networks.lock().unwrap().reserve().unwrap();
        let request = &entry.record.request;
        entry.record.allocation = Some(Allocation {
            version: 1,
            host_id: host.host_id,
            host_incarnation: host.incarnation.clone(),
            proxy_incarnation: "proxy-1".into(),
            session_id: request.key.creation.session_id,
            sandbox_id: request.cold.sandbox_id,
            boot_attempt: request.cold.boot_attempt.clone(),
            attachment_generation: generation,
            guest_ip: std::net::Ipv4Addr::new(198, 18, slot as u8, 2),
            gateway_ip: std::net::Ipv4Addr::new(198, 18, slot as u8, 1),
            policy_revision: request.bootstrap_policy.revision,
            policy_digest: request.bootstrap_policy.digest.clone(),
        });
        generation
    }

    fn cleanup_proxy(
        path: &Path,
    ) -> (
        tokio::task::JoinHandle<()>,
        tokio::sync::mpsc::Receiver<tokio::sync::oneshot::Sender<engram_egress_proto::FromProxyd>>,
        tokio::sync::mpsc::Receiver<Allocation>,
    ) {
        use engram_egress_proto::{FromProxyd, ToProxyd};
        let listener = tokio::net::UnixListener::bind(path.join("absent.sock")).unwrap();
        let (pending, removals) =
            tokio::sync::mpsc::channel::<tokio::sync::oneshot::Sender<FromProxyd>>(1);
        let (registered, registrations) = tokio::sync::mpsc::channel(1);
        let server = tokio::spawn(async move {
            let mut tasks = tokio::task::JoinSet::new();
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let pending = pending.clone();
                let registered = registered.clone();
                tasks.spawn(async move {
                    while let Ok(message) =
                        engram_egress_proto::read_frame::<_, ToProxyd>(&mut stream).await
                    {
                        let reply = match message {
                            ToProxyd::Hello { .. } => {
                                FromProxyd::HelloAck(engram_egress_proto::HelloInfo {
                                    incarnation: "proxy-1".into(),
                                    proto_version: engram_egress_proto::PROTO_VERSION,
                                    source_fingerprint: None,
                                    proxy_port: 1,
                                    dns_port: 2,
                                    gateway_port: 3,
                                    ca_fingerprint: "test".into(),
                                    coord_url: "test".into(),
                                })
                            }
                            ToProxyd::LookupGuest(_) => FromProxyd::Guest(None),
                            ToProxyd::ApplyPolicy(_) => FromProxyd::Ok,
                            ToProxyd::PrivateNetwork => {
                                use engram_egress_proto::private;
                                let allocation: Allocation =
                                    private::read(&mut stream).await.unwrap();
                                private::write(
                                    &mut stream,
                                    &private::Reply {
                                        version: 1,
                                        sequence: 0,
                                        epoch: 0,
                                        state: "closed".into(),
                                    },
                                )
                                .await
                                .unwrap();
                                registered.send(allocation).await.unwrap();
                                private::drain_to_eof(&stream).await.unwrap();
                                return;
                            }
                            ToProxyd::ReadManaged(_) => FromProxyd::ManagedState(None),
                            ToProxyd::RetirePrivateNetwork(_) => {
                                let (reply, wait) = tokio::sync::oneshot::channel();
                                pending.send(reply).await.unwrap();
                                wait.await.unwrap()
                            }
                            _ => panic!("unexpected cleanup request"),
                        };
                        if engram_egress_proto::write_frame(&mut stream, &reply)
                            .await
                            .is_err()
                        {
                            return;
                        }
                    }
                });
            }
        });
        (server, removals, registrations)
    }

    #[tokio::test]
    async fn address_release_waits_for_successful_proxy_retirement() {
        use engram_egress_proto::FromProxyd;
        let temp = tempfile::tempdir_in("/tmp").unwrap();
        let (host, request) = fixture(temp.path());
        let entry = install_exec_owner(&host, &request);
        let generation = reserve_test_network(&host, &mut *entry.lock().await);
        let (server, mut removals, mut registrations) = cleanup_proxy(temp.path());
        // Once a written removal times out, its late ACK must never complete
        // the next removal, whose drain is still held by the daemon.
        let h = host.clone();
        let e = entry.clone();
        let first = tokio::spawn(async move { h.close_resources(&mut *e.lock().await).await });
        let late = removals.recv().await.unwrap();
        tokio::time::pause();
        tokio::time::advance(std::time::Duration::from_secs(11)).await;
        assert!(first.await.unwrap().is_err());
        tokio::time::resume();
        assert!(host.networks.lock().unwrap().contains(generation));
        late.send(FromProxyd::Ok).unwrap();
        let h = host.clone();
        let e = entry.clone();
        let mut second = tokio::spawn(async move { h.close_resources(&mut *e.lock().await).await });
        let held = removals.recv().await.unwrap();
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(1), &mut second)
                .await
                .is_err(),
            "a late ACK must not complete the next removal"
        );
        assert!(host.networks.lock().unwrap().contains(generation));
        held.send(FromProxyd::Err("still draining".into())).unwrap();
        assert!(second.await.unwrap().is_err());
        for succeed in [false, true] {
            let h = host.clone();
            let e = entry.clone();
            let close = tokio::spawn(async move { h.close_resources(&mut *e.lock().await).await });
            let reply = tokio::time::timeout(std::time::Duration::from_secs(5), removals.recv())
                .await
                .unwrap()
                .unwrap();
            assert!(
                host.networks.lock().unwrap().contains(generation),
                "no address reuse before cleanup ACK"
            );
            reply
                .send(if succeed {
                    FromProxyd::Ok
                } else {
                    FromProxyd::Err("cleanup uncertain".into())
                })
                .unwrap();
            assert_eq!(close.await.unwrap().is_ok(), succeed);
            assert_eq!(host.networks.lock().unwrap().contains(generation), !succeed);
        }
        // Real host prepares reach the actual backend pin and fail verification.
        // With one live address and capacity two, retries must reuse only the
        // other slot, even after more attempts than the capacity.
        host.networks.lock().unwrap().slots.truncate(2);
        let held = host.networks.lock().unwrap().reserve().unwrap();
        host.chunks
            .put_manifest(
                request.cold.disk.manifest,
                &engram_chunk_store::Manifest::empty(engram_chunk_store::ManifestKind::Disk, 4),
            )
            .await
            .unwrap();
        let mut previous = held.1;
        for n in 0..6 {
            let mut next = request.clone();
            let session = engram_core::SessionId::new();
            let sandbox = SandboxId::new();
            next.key.creation.session_id = session;
            next.key.context.target.session_id = session;
            next.key.context.target.sandbox_id = sandbox;
            next.cold.session_id = session;
            next.cold.sandbox_id = sandbox;
            next.key.boot_attempt = format!("failed-{n}");
            next.cold.boot_attempt = next.key.boot_attempt.clone();
            let h = host.clone();
            let prepared = tokio::spawn(async move { h.prepare(next).await });
            let allocation =
                tokio::time::timeout(std::time::Duration::from_secs(5), registrations.recv())
                    .await
                    .unwrap()
                    .unwrap();
            assert_eq!(allocation.guest_ip, std::net::Ipv4Addr::new(198, 18, 1, 2));
            assert!(allocation.attachment_generation > previous);
            previous = allocation.attachment_generation;
            let reply = tokio::time::timeout(std::time::Duration::from_secs(5), removals.recv())
                .await
                .unwrap()
                .unwrap();
            assert!(host.networks.lock().unwrap().contains(previous));
            reply.send(FromProxyd::Ok).unwrap();
            assert!(prepared.await.unwrap().is_err());
            assert!(!host.networks.lock().unwrap().contains(previous));
            assert!(host.networks.lock().unwrap().contains(held.1));
            assert!(std::fs::read_dir(&host.directory)
                .unwrap()
                .all(|entry| !entry.unwrap().file_type().unwrap().is_dir()));
        }
        server.abort();
        let _ = server.await;
    }

    #[tokio::test]
    async fn capture_retry_waits_for_cleanup_after_a_timed_out_removal() {
        use engram_egress_proto::FromProxyd;
        let temp = tempfile::tempdir_in("/tmp").unwrap();
        let (host, request) = fixture(temp.path());
        let entry = install_exec_owner(&host, &request);
        let capture = ManagedCaptureRequest {
            key: request.key.clone(),
            capture_id: engram_core::SnapshotId::new(),
        };
        let path = host.directory.join(format!("{}.disk", capture.capture_id));
        durable_write(&path, b"disk cut").unwrap();
        let artifact = hash_file(&path).await.unwrap();
        let generation = {
            let mut entry = entry.lock().await;
            entry.record.capture = Some(CaptureProgress {
                request: capture.clone(),
                manifest: capture.disk_manifest(),
                artifact: Some(artifact),
                authority: None,
            });
            reserve_test_network(&host, &mut entry)
        };
        let (server, mut removals, _) = cleanup_proxy(temp.path());
        let h = host.clone();
        let c = capture.clone();
        let first = tokio::spawn(async move { h.capture(c).await });
        let late = removals.recv().await.unwrap();
        tokio::time::pause();
        tokio::time::advance(std::time::Duration::from_secs(11)).await;
        tokio::time::resume();
        let _ = late.send(FromProxyd::Ok);
        // The failure path retries cleanup, but that also cannot prove drain.
        let failure_cleanup = removals.recv().await.unwrap();
        failure_cleanup
            .send(FromProxyd::Err("still draining".into()))
            .unwrap();
        assert!(first.await.unwrap().is_err());
        assert!(host.networks.lock().unwrap().contains(generation));
        let stored = host.read_record(request.cold.sandbox_id).unwrap().unwrap();
        assert!(
            stored.capture.unwrap().authority.is_none(),
            "replacement authority must not precede cleanup"
        );
        assert!(!matches!(
            host.read(request.key.clone()).await.unwrap(),
            ManagedBootHostState::Captured { .. }
        ));
        let h = host.clone();
        let c = capture.clone();
        let mut retry = tokio::spawn(async move { h.capture(c).await });
        let current = removals.recv().await.unwrap();
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(1), &mut retry)
                .await
                .is_err()
        );
        assert!(host.networks.lock().unwrap().contains(generation));
        current.send(FromProxyd::Ok).unwrap();
        let authority = retry.await.unwrap().unwrap();
        assert_eq!(authority.request, capture);
        assert!(!host.networks.lock().unwrap().contains(generation));
        assert_eq!(host.capture(capture).await.unwrap(), authority);
        server.abort();
        let _ = server.await;
    }

    #[tokio::test]
    async fn uncertain_destroy_never_releases_an_address_on_retry() {
        let temp = tempfile::tempdir().unwrap();
        let (host, request) = fixture(temp.path());
        let entry = install_exec_owner(&host, &request);
        let mut owned = entry.lock().await;
        let generation = reserve_test_network(&host, &mut owned);
        if let Some(Owner::ExecTest(owner)) = &mut owned.owner {
            owner.close_fails = true;
        }
        drop(owned);
        for _ in 0..2 {
            assert!(host
                .destroy(
                    request.cold.sandbox_id,
                    engram_core::traits::SessionFence {
                        session_id: request.key.creation.session_id,
                        epoch: 1,
                    }
                )
                .await
                .is_err());
            assert!(host.networks.lock().unwrap().contains(generation));
        }
    }

    #[tokio::test]
    async fn recovered_record_cannot_release_current_incarnation_slot() {
        let temp = tempfile::tempdir_in("/tmp").unwrap();
        let (host, request) = fixture(temp.path());
        let entry = install_exec_owner(&host, &request);
        let mut entry = entry.lock().await;
        let generation = reserve_test_network(&host, &mut entry);
        entry.owner = None;
        entry.record.incarnation = "old-host".into();
        entry.record.allocation.as_mut().unwrap().host_incarnation = "old-host".into();
        assert!(
            host.close_resources(&mut entry).await.is_err(),
            "recovery cannot assume missing cleanup proof"
        );
        assert!(host.networks.lock().unwrap().contains(generation));
        let (server, mut removals, _) = cleanup_proxy(temp.path());
        let cleanup = host.close_resources(&mut entry);
        let ack = async {
            removals
                .recv()
                .await
                .unwrap()
                .send(engram_egress_proto::FromProxyd::Ok)
                .unwrap();
        };
        let (result, ()) = tokio::join!(cleanup, ack);
        result.unwrap();
        assert!(host.networks.lock().unwrap().contains(generation));
        server.abort();
        let _ = server.await;
    }

    #[tokio::test]
    async fn failed_materialization_removes_disposable_staging() {
        let temp = tempfile::tempdir().unwrap();
        let (host, request) = fixture(temp.path());
        let id = request.key.context.target.sandbox_id;
        assert!(host.prepare(request).await.is_err());
        assert!(
            !host.directory.join(id.to_string()).exists(),
            "failed prepare retained old staging"
        );
        assert!(
            std::fs::read_dir(&host.directory)
                .unwrap()
                .all(|entry| !entry.unwrap().file_type().unwrap().is_dir()),
            "failed prepare retained staging"
        );
    }

    #[tokio::test]
    async fn cold_assets_hash_exact_sparse_disk_and_configured_files_without_owner() {
        use engram_chunk_store::{ChunkRef, ChunkSize, Manifest, ManifestKind};
        use sha2::{Digest, Sha256};
        let temp = tempfile::tempdir().unwrap();
        let (host, boot) = fixture(temp.path());
        let mut drives = boot.cold.aux_ro_drives.clone();
        for (i, drive) in drives.iter_mut().enumerate() {
            let bytes = vec![i as u8 + 1; 17];
            let hash = hex::encode(Sha256::digest(&bytes));
            std::fs::write(temp.path().join(AuxRoDrive::staged_file_name(&hash)), bytes).unwrap();
            drive.sha256 = Some(hash);
        }
        let mut manifest = Manifest::empty(ManifestKind::Disk, 15);
        manifest.chunk_size = ChunkSize::bytes(4);
        manifest.chunks = vec![
            ChunkRef {
                offset: 4,
                hash: host.chunks.put_chunk(b"abcd").await.unwrap(),
            },
            ChunkRef {
                offset: 12,
                hash: host.chunks.put_chunk(b"xyz").await.unwrap(),
            },
        ];
        host.chunks
            .put_manifest(boot.cold.disk.manifest, &manifest)
            .await
            .unwrap();
        let request = engram_core::types::managed_assets::ManagedColdAssetsRequest {
            version: 1,
            host_id: host.host_id,
            session_id: boot.cold.session_id,
            intent_id: "intent-a".into(),
            image: boot.cold.image,
            image_digest: format!("sha256:{}", "e".repeat(64)),
            disk_manifest: boot.cold.disk.manifest,
            aux_ro_drives: drives,
        };
        let client =
            crate::host_client::LocalHostClient::new(host.ordinary.clone(), host.hub.clone())
                .with_managed_boot(host.clone());
        let receipt = client
            .read_managed_cold_assets(request.clone())
            .await
            .unwrap();
        receipt.validate_for(&request).unwrap();
        assert_eq!(
            receipt.disk_sha256,
            hex::encode(Sha256::digest(b"\0\0\0\0abcd\0\0\0\0xyz"))
        );
        assert_eq!(receipt.disk_size_bytes, 15);
        assert_eq!(
            receipt.kernel_sha256,
            hex::encode(Sha256::digest(b"kernel"))
        );
        assert_eq!(receipt.host_incarnation, "current-host");
        #[cfg(target_os = "macos")]
        assert_eq!(
            receipt.backend,
            engram_core::types::managed_assets::ManagedColdBackend::Vz
        );
        #[cfg(target_os = "linux")]
        assert_eq!(
            receipt.backend,
            engram_core::types::managed_assets::ManagedColdBackend::Firecracker
        );
        assert!(host.entries.is_empty());
        assert_eq!(host.networks.lock().unwrap().generation, 0);
        assert!(host.ordinary.list().await.unwrap().is_empty());
        assert_eq!(std::fs::read_dir(&host.directory).unwrap().count(), 0);
        std::fs::write(temp.path().join("kernel"), b"changed kernel").unwrap();
        let changed = client
            .read_managed_cold_assets(request.clone())
            .await
            .unwrap();
        assert_ne!(changed.kernel_sha256, receipt.kernel_sha256);
        let mut wrong_host = request.clone();
        wrong_host.host_id = HostId::new();
        assert!(client.read_managed_cold_assets(wrong_host).await.is_err());
        let mut reordered = request.clone();
        reordered.aux_ro_drives.swap(0, 1);
        assert!(client.read_managed_cold_assets(reordered).await.is_err());
        for field in 0..6 {
            let mut invalid = request.clone();
            match field {
                0 => invalid.version = 2,
                1 => invalid.intent_id = "i".repeat(129),
                2 => invalid.image = "i".repeat(257),
                3 => invalid.aux_ro_drives.pop().map(|_| ()).unwrap(),
                4 => invalid.aux_ro_drives[0].sha256 = Some("../kernel".into()),
                _ => invalid.disk_manifest.version = 0,
            }
            assert!(client.read_managed_cold_assets(invalid).await.is_err());
        }
        let bundle = temp.path().join(AuxRoDrive::staged_file_name(
            request.aux_ro_drives[1].sha256.as_ref().unwrap(),
        ));
        std::fs::write(&bundle, b"corrupt").unwrap();
        assert!(client
            .read_managed_cold_assets(request.clone())
            .await
            .is_err());
        std::fs::remove_file(bundle).unwrap();
        assert!(client.read_managed_cold_assets(request).await.is_err());
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
            transfer_stop: tokio::sync::watch::channel(()).0,
            close_failed: false,
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
            transfer_stop: tokio::sync::watch::channel(()).0,
            close_failed: false,
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
        let error = local
            .migration_abort_unserved(
                id,
                "unserved-export",
                engram_core::traits::SessionFence::new(key.creation.session_id, 1),
            )
            .await
            .unwrap_err();
        assert!(matches!(error, SandboxError::InvalidSpec(message)
            if message == "managed session requires its current boot owner"));
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
            transfer_stop: tokio::sync::watch::channel(()).0,
            close_failed: false,
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
                transfer_stop: tokio::sync::watch::channel(()).0,
                close_failed: false,
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
        host.destroy_tombstoned(id).await.unwrap();
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
