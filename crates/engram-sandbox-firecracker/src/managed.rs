//! Exact managed Firecracker ownership from CLOSED preparation to retirement.
#[cfg(any(target_os = "linux", test))]
mod adapter;
#[cfg(any(target_os = "linux", test))]
mod child;
#[cfg(any(target_os = "linux", test))]
mod configure;
#[cfg(any(target_os = "linux", test))]
mod disk;
#[cfg(any(target_os = "linux", test))]
mod files;
mod network;
#[cfg(target_os = "linux")]
mod process;

#[cfg(target_os = "linux")]
use engram_core::types::sandbox::{SessionFileMetadata, SessionFileSpec, SessionFileStream};
use engram_core::{
    types::{
        managed_cold::{ManagedColdPrepare, ManagedColdPrepared},
        sandbox::SandboxSpec,
    },
    SandboxError,
};
use engram_egress_proto::private::{Allocation, RegisteredPrivateNetwork};

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct ManagedFcClosedReceipt {
    pub prepared: ManagedColdPrepared,
    pub allocation: Allocation,
    pub shape: network::NetworkShape,
    pub namespace_device: u64,
    pub namespace_inode: u64,
    pub tap_ifindex: u32,
    pub rules_sha256: String,
    pub configuration_sha256: String,
}
/// Exact private process, boot files and network ownership through retirement.
/// It cannot enter the ordinary sandbox map or expose a generic resume permit.
pub struct PreparedManagedFc {
    #[cfg(not(target_os = "linux"))]
    _unavailable: std::convert::Infallible,
    #[cfg(target_os = "linux")]
    inner: Prepared,
}
#[cfg(target_os = "linux")]
struct Prepared {
    process: process::Process,
    adapter: adapter::Adapter,
    namespace: std::sync::Arc<network::Namespace>,
    api: crate::FirecrackerClient,
    config: serde_json::Value,
    receipt: ManagedFcClosedReceipt,
    failed: bool,
    files: std::sync::Arc<files::PinnedFiles>,
    released_epoch: Option<u64>,
    started: bool,
    streams: tokio::task::JoinSet<()>,
}
impl crate::FirecrackerBackend {
    /// Transfer disposable staging to the blocking pin operation. Cancellation
    /// can close ingress while that operation still owns its input files.
    pub async fn prepare_managed_cold(
        &self,
        request: ManagedColdPrepare,
        spec: SandboxSpec,
        network: RegisteredPrivateNetwork,
        staging: tempfile::TempDir,
        session_disk: std::sync::Arc<dyn engram_host_core::SessionDisk>,
    ) -> Result<PreparedManagedFc, SandboxError> {
        request.validate(&spec).map_err(SandboxError::InvalidSpec)?;
        let allocation = network.allocation();
        if allocation.session_id != request.session_id
            || allocation.sandbox_id != request.sandbox_id
            || allocation.boot_attempt != request.boot_attempt
        {
            return Err(SandboxError::InvalidSpec(
                "managed allocation does not match cold request".into(),
            ));
        }
        let shape = network::NetworkShape::new(allocation.guest_ip, allocation.gateway_ip)?;
        #[cfg(target_os = "linux")]
        {
            prepare(self, request, spec, network, shape, staging, session_disk).await
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (shape, staging, session_disk);
            Err(SandboxError::InvalidSpec(
                "managed Firecracker preparation requires Linux".into(),
            ))
        }
    }
}
impl PreparedManagedFc {
    pub async fn closed_receipt(&mut self) -> Result<ManagedFcClosedReceipt, SandboxError> {
        #[cfg(target_os = "linux")]
        {
            let result = self.inner.check().await;
            if result.is_err() {
                self.inner.failed = true;
                let _ = self.inner.adapter.close().await;
                let _ = self.inner.process.close().await;
            }
            result
        }
        #[cfg(not(target_os = "linux"))]
        {
            Err(SandboxError::InvalidSpec(
                "managed Firecracker preparation requires Linux".into(),
            ))
        }
    }
    #[cfg(target_os = "linux")]
    pub async fn release(&mut self, epoch: u64) -> Result<(), SandboxError> {
        self.closed_receipt().await?;
        self.inner.failed = true;
        self.inner.adapter.release(epoch).await?;
        let pid = self.inner.process.live_pid()?;
        let namespace = self.inner.namespace.clone();
        tokio::task::spawn_blocking(move || namespace.release(pid))
            .await
            .map_err(std::io::Error::other)??;
        self.inner.adapter.check_ready(epoch)?;
        self.inner.released_epoch = Some(epoch);
        self.inner.failed = false;
        Ok(())
    }
    #[cfg(target_os = "linux")]
    pub async fn check_ready(&mut self, epoch: u64) -> Result<(), SandboxError> {
        if self.inner.failed || self.inner.released_epoch != Some(epoch) {
            return Err(std::io::Error::other("managed owner is not ready").into());
        }
        self.inner.adapter.check_ready(epoch)?;
        let pid = self.inner.process.live_pid()?;
        let namespace = self.inner.namespace.clone();
        tokio::task::spawn_blocking(move || namespace.check(Some(pid)))
            .await
            .map_err(std::io::Error::other)??;
        if self.inner.api.managed_get("/vm/config").await? != self.inner.config
            || self.inner.api.managed_get("/").await?["state"]
                != if self.inner.started {
                    "Running"
                } else {
                    "Not started"
                }
        {
            return Err(std::io::Error::other("managed VM configuration changed").into());
        }
        Ok(())
    }
    #[cfg(target_os = "linux")]
    pub async fn start_agent(
        &mut self,
        agent: engram_core::types::sandbox::AgentSpec,
        sink: engram_core::traits::HarnessSink,
    ) -> Result<(), SandboxError> {
        self.check_ready(agent.binding_epoch).await?;
        if self.inner.started {
            return Err(std::io::Error::other("managed start already issued").into());
        }
        self.inner.started = true;
        // An issued start can change the disk before its reply. Preserve the
        // exact private lineage through cancellation, Drop and failure cleanup.
        self.inner
            .files
            .retain
            .store(true, std::sync::atomic::Ordering::Release);
        self.inner.failed = true;
        let vsock = self.inner.files.path.join("guest.vsock");
        let listener = tokio::net::UnixListener::bind(crate::harness_uds_for(&vsock))?;
        self.inner.streams.spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                sink(Box::pin(stream));
            }
        });
        self.inner
            .api
            .put_action(crate::client::ActionType::InstanceStart)
            .await?;
        let mut conn = tokio::time::timeout(std::time::Duration::from_secs(180), async {
            loop {
                self.inner.process.live_pid()?;
                self.inner.adapter.check_ready(agent.binding_epoch)?;
                if let Ok(conn) =
                    crate::FirecrackerBackend::connect_fc_vsock_once(&vsock, 1024).await
                {
                    return Ok::<_, SandboxError>(conn);
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .map_err(|_| std::io::Error::other("managed agent connection timed out"))??;
        let request =
            engram_agentd::WireRequest::SpawnHarness(engram_agentd::SpawnHarnessRequest {
                argv: agent.argv,
                env: agent.env.into_iter().collect(),
                session_env: agent.session_env.into_iter().collect(),
                host_ca_pem: agent.host_ca_pem,
            });
        // Never retry the write or unknown response in this fresh guest context.
        engram_agentd::write_msg(&mut conn, &request).await?;
        let response: engram_agentd::WireResponse = engram_agentd::read_msg(&mut conn).await?;
        if !matches!(response, engram_agentd::WireResponse::HarnessSpawned { .. }) {
            return Err(std::io::Error::other("managed harness start failed").into());
        }
        self.inner.failed = false;
        Ok(())
    }
    #[cfg(target_os = "linux")]
    pub fn owns(&self, request: &engram_core::types::managed_cold::ManagedColdPrepare) -> bool {
        &self.inner.receipt.prepared.request == request
    }
    #[cfg(target_os = "linux")]
    async fn exec_connection(
        &mut self,
        epoch: u64,
    ) -> Result<tokio::net::UnixStream, SandboxError> {
        self.check_ready(epoch).await?;
        if !self.inner.started {
            return Err(std::io::Error::other("managed execution requires a started owner").into());
        }
        crate::FirecrackerBackend::connect_fc_vsock_once(
            &self.inner.files.path.join("guest.vsock"),
            1024,
        )
        .await
    }
    #[cfg(target_os = "linux")]
    pub async fn exec_stream(
        &mut self,
        epoch: u64,
        cmd: engram_core::types::sandbox::ExecRequest,
    ) -> Result<engram_core::types::sandbox::ExecStream, SandboxError> {
        let probe = self.exec_connection(epoch).await?;
        let durable = crate::probe_durable_exec(Box::new(probe)).await?;
        let connection = self.exec_connection(epoch).await?;
        // Never hand a reconnect capability out of this live owner. On EOF,
        // the caller retries the same journal ticket through current ownership.
        crate::drive_exec_protocol(
            self.inner.receipt.prepared.request.sandbox_id,
            Box::new(connection),
            cmd,
            durable,
            None,
        )
        .await
    }
    #[cfg(target_os = "linux")]
    pub async fn prepare_read_file(
        &mut self,
        epoch: u64,
        path: String,
        mut closed: tokio::sync::watch::Receiver<()>,
    ) -> Result<
        impl std::future::Future<
                Output = Result<(SessionFileMetadata, SessionFileStream), SandboxError>,
            > + Send
            + 'static,
        SandboxError,
    > {
        let connection = self.exec_connection(epoch).await?;
        // This future owns the exact connection and has no reconnect capability.
        Ok(engram_agentd::file_transfer::read_file_until_closed(
            connection,
            path,
            async move {
                let _ = closed.changed().await;
            },
        ))
    }
    #[cfg(target_os = "linux")]
    pub async fn prepare_write_file(
        &mut self,
        epoch: u64,
        spec: SessionFileSpec,
        bytes: SessionFileStream,
    ) -> Result<
        impl std::future::Future<Output = Result<SessionFileMetadata, SandboxError>> + Send + 'static,
        SandboxError,
    > {
        let connection = self.exec_connection(epoch).await?;
        Ok(engram_agentd::file_transfer::write_file(
            connection, spec, bytes,
        ))
    }
    #[cfg(target_os = "linux")]
    pub async fn cancel_exec(&mut self, epoch: u64, exec_id: String) -> Result<(), SandboxError> {
        let connection = self.exec_connection(epoch).await?;
        crate::cancel_exec_roundtrip(Box::new(connection), &exec_id).await
    }
    #[cfg(target_os = "linux")]
    pub async fn retire_disk(&mut self, destination: &std::path::Path) -> Result<(), SandboxError> {
        if self.inner.failed {
            return Err(std::io::Error::other("managed capture source unavailable").into());
        }
        self.inner
            .files
            .retain
            .store(true, std::sync::atomic::Ordering::Release);
        self.inner
            .files
            .disks
            .as_ref()
            .ok_or_else(|| std::io::Error::other("managed disk owner missing"))?
            .retain_source();
        if !self.inner.started {
            if let Some(epoch) = self.inner.released_epoch {
                self.check_ready(epoch).await?;
            } else {
                self.closed_receipt().await?;
            }
        }
        self.inner.failed = true;
        if self.inner.started {
            self.inner.api.pause().await?;
        }
        self.inner.streams.shutdown().await;
        self.inner.adapter.close().await?;
        self.inner.process.close().await?;
        self.inner.files.export_capture(destination).await?;
        Ok(())
    }
    /// The host has durably recorded the exported file's exact hash and size.
    #[cfg(target_os = "linux")]
    pub fn capture_stored(&mut self) -> Result<(), SandboxError> {
        self.inner.files.capture_stored()?;
        Ok(())
    }
    /// Wait for adapter and VM cleanup. Drop initiates cleanup only.
    pub async fn close(self) -> Result<(), SandboxError> {
        self.close_inner(false).await
    }
    /// An authenticated delete may discard a known live source after reaping.
    /// It cannot discard an uncertain start or an incomplete capture.
    pub async fn destroy(self) -> Result<(), SandboxError> {
        self.close_inner(true).await
    }
    async fn close_inner(mut self, destroy: bool) -> Result<(), SandboxError> {
        #[cfg(target_os = "linux")]
        {
            let uncertain = self.inner.failed;
            self.inner.failed = true;
            self.inner.streams.shutdown().await;
            let network = self.inner.adapter.close().await;
            let process = self.inner.process.close().await;
            network?;
            process?;
            self.inner.files.close(destroy, uncertain).await?;
            Ok(())
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (&mut self, destroy);
            Ok(())
        }
    }
}
#[cfg(target_os = "linux")]
impl Prepared {
    async fn check(&mut self) -> Result<ManagedFcClosedReceipt, SandboxError> {
        if self.failed {
            return Err(std::io::Error::other("managed preparation failed").into());
        }
        self.adapter.check_closed()?;
        let pid = self.process.live_pid()?;
        let ns = self.namespace.clone();
        tokio::task::spawn_blocking(move || ns.check(Some(pid)))
            .await
            .map_err(|_| std::io::Error::other("namespace check stopped"))??;
        if self.api.managed_get("/").await?["state"] != "Not started"
            || self.api.managed_get("/vm/config").await? != self.config
        {
            return Err(std::io::Error::other("managed VM state changed").into());
        }
        self.adapter.check_closed()?;
        Ok(self.receipt.clone())
    }
}
#[cfg(target_os = "linux")]
async fn prepare(
    backend: &crate::FirecrackerBackend,
    request: ManagedColdPrepare,
    spec: SandboxSpec,
    registered: RegisteredPrivateNetwork,
    shape: network::NetworkShape,
    staging: tempfile::TempDir,
    session_disk: std::sync::Arc<dyn engram_host_core::SessionDisk>,
) -> Result<PreparedManagedFc, SandboxError> {
    use std::{io, sync::Arc};
    let allocation = registered.allocation().clone();
    let private = registered.into_stream();
    let files = {
        let work = backend.work_dir.clone();
        let cfg = backend.config.clone();
        let req = request.clone();
        let spec = spec.clone();
        // Own creation through completion: cancellation cannot abandon an ioctl
        // after it creates a volume but before the owner records its name.
        let pin = tokio::spawn(async move {
            let pin_request = req.clone();
            let pin_spec = spec.clone();
            let mut files = tokio::task::spawn_blocking(move || {
                files::PinnedFiles::prepare(&work, &cfg, &pin_request, &pin_spec, staging)
            })
            .await
            .map_err(io::Error::other)??;
            files.disks = Some(disk::Disks::new(session_disk, req.sandbox_id));
            files
                .disks
                .as_ref()
                .expect("disk owner installed")
                .install(
                    &files.path,
                    &req.disk.sha256,
                    req.disk.size_bytes,
                    spec.swap_mib,
                )
                .await?;
            Ok::<_, io::Error>(files)
        });
        tokio::select! { biased;
            _ = wait_for_stop(&private) => return Err(io::Error::other("private allocation stopped").into()),
            files = pin => Arc::new(files.map_err(|_| io::Error::other("file preparation stopped"))??),
        }
    };
    let (namespace, tcp, dns) = tokio::select! { biased;
        _ = wait_for_stop(&private) => return Err(io::Error::other("private allocation stopped").into()),
        network = network::Namespace::create(shape.clone()) => network?,
    };
    let path = files.path.clone();
    let mut process = process::Process::reserve(namespace.clone(), files.clone())?;
    let adapter = adapter::Adapter::spawn(
        private,
        tokio::net::TcpListener::from_std(tcp)?,
        tokio::net::UdpSocket::from_std(dns)?,
        shape.guest,
        shape.gateway,
        Some(process.stop_signal()),
    )?;
    process.spawn(&backend.config.firecracker_bin)?;
    let api = crate::FirecrackerClient::new(path.join("firecracker.sock"));
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            process.live_pid()?;
            adapter.check_closed()?;
            if tokio::net::UnixStream::connect(api.socket()).await.is_ok() {
                return Ok::<_, io::Error>(());
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .map_err(|_| io::Error::other("managed VM API timed out"))??;
    let cid = backend
        .next_cid
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if cid < 3 || cid == u32::MAX {
        return Err(io::Error::other("invalid vsock identity").into());
    }
    configure::configure(&api, &backend.config, &request, &spec, &path, &shape, cid).await?;
    let config = api.managed_get("/vm/config").await?;
    let devices = config["network-interfaces"]
        .as_array()
        .ok_or_else(|| io::Error::other("missing VM network readback"))?;
    if devices.len() != 1
        || devices[0]["host_dev_name"] != network::TAP
        || devices[0]["guest_mac"] != network::GUEST_MAC
    {
        return Err(io::Error::other("wrong VM network attachment").into());
    }
    let identity = namespace.identity.clone();
    use sha2::{Digest, Sha256};
    let configuration_sha256 = hex::encode(Sha256::digest(
        serde_json::to_vec(&config).map_err(io::Error::other)?,
    ));
    let mut prepared = PreparedManagedFc {
        inner: Prepared {
            process,
            adapter,
            namespace,
            api,
            config,
            failed: false,
            files,
            released_epoch: None,
            started: false,
            streams: tokio::task::JoinSet::new(),
            receipt: ManagedFcClosedReceipt {
                prepared: ManagedColdPrepared {
                    version: 1,
                    request,
                    state: engram_core::types::managed_cold::ManagedColdState::Closed,
                },
                allocation,
                shape,
                namespace_device: identity.namespace_device,
                namespace_inode: identity.namespace_inode,
                tap_ifindex: identity.tap_ifindex,
                rules_sha256: identity.rules_sha256,
                configuration_sha256,
            },
        },
    };
    prepared.closed_receipt().await?;
    Ok(prepared)
}
#[cfg(any(target_os = "linux", test))]
async fn wait_for_stop(stream: &tokio::net::UnixStream) {
    loop {
        match stream.try_read(&mut [0]) {
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if stream.readable().await.is_err() {
                    return;
                }
            }
            _ => return,
        }
    }
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn proxy_shutdown_and_unexpected_data_stop_preparation_before_copy_completes() {
        use tokio::io::AsyncWriteExt;
        for send_data in [false, true] {
            let (private, mut peer) = tokio::net::UnixStream::pair().unwrap();
            if send_data {
                peer.write_all(b"x").await.unwrap();
            } else {
                peer.shutdown().await.unwrap();
            }
            tokio::time::timeout(
                std::time::Duration::from_millis(100),
                super::wait_for_stop(&private),
            )
            .await
            .unwrap();
        }
    }
}
