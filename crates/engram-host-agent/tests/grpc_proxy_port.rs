//! gRPC-layer integration test for `ProxyPort` (ADR 0064).
//!
//! The raw-byte sibling of `grpc_proxy_shell.rs`: stands up the real
//! gRPC `HostService` with a stub `HostClient` and asserts that opaque
//! bytes round-trip **both directions** through the ProxyPort tunnel,
//! and that the `Open` sentinel (sandbox_id + port) is delivered to the
//! host out-of-band — never leaked into the byte stream. Runs in-process
//! (no VM, no netns), so it lives in the standard nextest lane.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use engram_core::error::SandboxError;
use engram_core::traits::sandbox::{HarnessDial, HarnessSink};
use engram_core::traits::HostClient;
use engram_core::types::egress::SessionEgressPolicy;
use engram_core::types::port::{PortTunnel, PortTunnelEnds};
use engram_core::types::sandbox::{AgentSpec, ExecRequest, ExecStream, SandboxSpec};
use engram_core::types::snapshot::SnapshotMetadata;
use engram_core::{SandboxId, SessionId};
use parking_lot::Mutex;

/// Stub HostClient whose only meaningful method is `proxy_port`. Every
/// other non-defaulted method panics so the test fails loudly if it
/// starts touching surface it doesn't intend to cover. `proxy_port`
/// opens a fresh PortTunnel, stashes the host-side ends (which the test
/// inspects), and records the `(sandbox_id, port)` it was asked to open.
#[derive(Default)]
struct FakeHost {
    captured: Mutex<Option<(SandboxId, u16)>>,
    tunnel_ends: Mutex<Option<PortTunnelEnds>>,
}

#[async_trait]
impl HostClient for FakeHost {
    async fn create(&self, _: SandboxSpec) -> Result<SandboxId, SandboxError> {
        unreachable!("proxy_port test path doesn't call create")
    }
    async fn destroy(&self, _: SandboxId) -> Result<(), SandboxError> {
        unreachable!()
    }
    async fn list(&self) -> Result<Vec<SandboxId>, SandboxError> {
        unreachable!()
    }
    async fn probe_sandbox(
        &self,
        _: SandboxId,
    ) -> Result<engram_core::types::sandbox::SandboxProbe, SandboxError> {
        unreachable!()
    }
    async fn exec_stream(&self, _: SandboxId, _: ExecRequest) -> Result<ExecStream, SandboxError> {
        unreachable!()
    }
    async fn snapshot(&self, _: SandboxId) -> Result<SnapshotMetadata, SandboxError> {
        unreachable!()
    }
    async fn restore(&self, _: SnapshotMetadata) -> Result<SandboxId, SandboxError> {
        unreachable!()
    }
    async fn start_agent(
        &self,
        _: SandboxId,
        _: AgentSpec,
        _: SessionEgressPolicy,
    ) -> Result<(), SandboxError> {
        unreachable!()
    }
    async fn apply_egress_policy(&self, _: SessionEgressPolicy) -> Result<(), SandboxError> {
        unreachable!()
    }
    async fn guest_ip(&self, _: SandboxId) -> Option<String> {
        None
    }
    async fn bind_session(&self, _: SessionId, _: SandboxId) {}
    async fn unbind_session(&self, _: SessionId) {}
    async fn send_prompt(&self, _: SandboxId, _: String, _: String) -> Result<(), SandboxError> {
        unreachable!()
    }
    async fn acquire_shell(&self, _: SandboxId) -> Result<(), SandboxError> {
        Ok(())
    }
    async fn release_shell(&self, _: SandboxId) -> Result<(), SandboxError> {
        Ok(())
    }
    async fn proxy_port(
        &self,
        sandbox_id: SandboxId,
        port: u16,
    ) -> Result<PortTunnel, SandboxError> {
        *self.captured.lock() = Some((sandbox_id, port));
        let (tunnel, ends) = PortTunnel::pair();
        *self.tunnel_ends.lock() = Some(ends);
        Ok(tunnel)
    }
    fn harness_dial(&self) -> HarnessDial {
        HarnessDial::Vsock
    }
    fn set_harness_sink(&self, _: HarnessSink) {}
}

/// Bind a random localhost port, return the addr, and free it (tonic
/// rebinds; the race window is acceptable for in-proc tests).
fn pick_local_addr() -> std::net::SocketAddr {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("local_addr");
    drop(listener);
    addr
}

async fn boot_grpc_server(host: Arc<FakeHost>) -> std::net::SocketAddr {
    let addr = pick_local_addr();
    let host_dyn: Arc<dyn HostClient> = host;
    tokio::spawn(async move {
        let _ = engram_host_agent::grpc_server::boot(addr, host_dyn, None).await;
    });
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if tokio::net::TcpStream::connect(addr).await.is_ok() {
            return addr;
        }
        if std::time::Instant::now() >= deadline {
            panic!("gRPC server did not bind {addr} within 5s");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn connect_grpc_client(
    addr: std::net::SocketAddr,
) -> engram_protocol::grpc_client::GrpcHostClient {
    let endpoint = format!("http://{addr}")
        .parse::<tonic::transport::Endpoint>()
        .expect("endpoint");
    let channel = endpoint.connect().await.expect("connect channel");
    engram_protocol::grpc_client::GrpcHostClient::new(channel)
}

/// Client → host: bytes the caller writes to `tunnel.outbound` arrive on
/// the host-side `outbound_rx` verbatim and in order — the first item is
/// the caller's data, never the `Open` sentinel. And `(sandbox_id, port)`
/// round-trip out-of-band via the Open variant.
#[tokio::test]
async fn proxy_port_forwards_client_bytes_to_host_and_carries_open() {
    let host = Arc::new(FakeHost::default());
    let addr = boot_grpc_server(host.clone()).await;
    let client = connect_grpc_client(addr).await;

    let sandbox_id = SandboxId::new();
    let tunnel = client
        .proxy_port(sandbox_id, 3000)
        .await
        .expect("client proxy_port");

    tunnel
        .outbound
        .send(Bytes::from_static(b"GET / HTTP/1.1\r\n\r\n"))
        .await
        .expect("push bytes");

    let ends = host
        .tunnel_ends
        .lock()
        .take()
        .expect("server didn't call inner proxy_port");
    let PortTunnelEnds {
        mut outbound_rx, ..
    } = ends;

    let first = tokio::time::timeout(Duration::from_secs(2), outbound_rx.recv())
        .await
        .expect("no bytes reached the host within 2s")
        .expect("host-side tunnel closed without delivering");
    assert_eq!(&first[..], b"GET / HTTP/1.1\r\n\r\n");

    // sandbox_id + port round-tripped via the Open variant, out-of-band.
    let captured = host
        .captured
        .lock()
        .expect("server didn't extract Open(sandbox_id, port)");
    assert_eq!(captured, (sandbox_id, 3000));
}

/// Host → client: bytes the guest socket produces (pushed into the
/// host-side `inbound_tx`) arrive on the client `tunnel.inbound`
/// verbatim.
#[tokio::test]
async fn proxy_port_forwards_host_bytes_to_client() {
    let host = Arc::new(FakeHost::default());
    let addr = boot_grpc_server(host.clone()).await;
    let client = connect_grpc_client(addr).await;

    let sandbox_id = SandboxId::new();
    let mut tunnel = client
        .proxy_port(sandbox_id, 8080)
        .await
        .expect("client proxy_port");

    let ends = host
        .tunnel_ends
        .lock()
        .take()
        .expect("server didn't call inner proxy_port");
    let PortTunnelEnds { inbound_tx, .. } = ends;

    inbound_tx
        .send(Bytes::from_static(b"HTTP/1.1 200 OK\r\n\r\nhi"))
        .await
        .expect("host push bytes");

    let got = tokio::time::timeout(Duration::from_secs(2), tunnel.inbound.recv())
        .await
        .expect("no bytes reached the client within 2s")
        .expect("client-side tunnel closed without delivering");
    assert_eq!(&got[..], b"HTTP/1.1 200 OK\r\n\r\nhi");
}
