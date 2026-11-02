//! Managed cold preparation. Closed owners have no start or resume operation.
#[cfg(any(target_os = "linux", test))]
mod adapter;
#[cfg(any(target_os = "linux", test))]
mod child;
#[cfg(any(target_os = "linux", test))]
mod configure;
#[cfg(any(target_os = "linux", test))]
mod files;
mod network;
#[cfg(target_os = "linux")]
mod process;

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
/// A never-started process, exact private boot files and closed private network.
/// It cannot enter the ordinary sandbox map or expose a start/resume permit.
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
}
impl crate::FirecrackerBackend {
    pub async fn prepare_managed_cold(
        &self,
        request: ManagedColdPrepare,
        spec: SandboxSpec,
        network: RegisteredPrivateNetwork,
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
            prepare(self, request, spec, network, shape).await
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = shape;
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
    /// Wait for adapter and VM cleanup. Drop initiates cleanup only.
    pub async fn close(mut self) -> Result<(), SandboxError> {
        #[cfg(target_os = "linux")]
        {
            self.inner.failed = true;
            let network = self.inner.adapter.close().await;
            let process = self.inner.process.close().await;
            network?;
            process?;
            Ok(())
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = &mut self;
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
) -> Result<PreparedManagedFc, SandboxError> {
    use std::{io, sync::Arc};
    let allocation = registered.allocation().clone();
    let private = registered.into_stream();
    let files = {
        let work = backend.work_dir.clone();
        let cfg = backend.config.clone();
        let req = request.clone();
        let spec = spec.clone();
        let pin = tokio::task::spawn_blocking(move || {
            files::PinnedFiles::prepare(&work, &cfg, &req, &spec)
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
    let mut process = process::Process::reserve(namespace.clone(), files)?;
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
