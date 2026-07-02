//! gRPC-layer wiring test for ADR 0045 Phase F pause / resume.
//!
//! Pause/resume are thin passthroughs (proto `PauseSandbox`/`ResumeSandbox`
//! → host gRPC server → `HostClient::pause`/`resume` → the backend). The
//! risk in that plumbing is a wiring mismatch — the RPC bound to the wrong
//! handler, or pause/resume swapped. This stands up the real gRPC server
//! with a stub host that records which method it was asked to run for which
//! sandbox, drives it through the real `GrpcHostClient`, and asserts each
//! verb reaches the host with the right sandbox id, in order.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use engram_core::error::SandboxError;
use engram_core::traits::sandbox::{HarnessDial, HarnessSink};
use engram_core::traits::HostClient;
use engram_core::types::egress::SessionEgressPolicy;
use engram_core::types::sandbox::{AgentSpec, ExecRequest, ExecStream, SandboxSpec};
use engram_core::types::shell::ShellTunnel;
use engram_core::types::snapshot::SnapshotMetadata;
use engram_core::{SandboxId, SessionId};
use parking_lot::Mutex;

/// Records the (verb, sandbox_id) of every pause/resume the gRPC server
/// forwarded. Every other trait method is `unreachable!()` — a loud
/// failure if the test starts touching surface it doesn't intend to cover.
#[derive(Default)]
struct FakeHost {
    calls: Mutex<Vec<(&'static str, SandboxId)>>,
}

#[async_trait]
impl HostClient for FakeHost {
    async fn pause(&self, id: SandboxId) -> Result<(), SandboxError> {
        self.calls.lock().push(("pause", id));
        Ok(())
    }
    async fn resume(&self, id: SandboxId) -> Result<(), SandboxError> {
        self.calls.lock().push(("resume", id));
        Ok(())
    }

    async fn create(&self, _: SandboxSpec) -> Result<SandboxId, SandboxError> {
        unreachable!("pause/resume test path doesn't call create")
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
        unreachable!()
    }
    async fn release_shell(&self, _: SandboxId) -> Result<(), SandboxError> {
        unreachable!()
    }
    async fn proxy_shell(&self, _: SandboxId) -> Result<ShellTunnel, SandboxError> {
        unreachable!()
    }
    fn harness_dial(&self) -> HarnessDial {
        HarnessDial::Vsock
    }
    fn set_harness_sink(&self, _: HarnessSink) {}
}

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

#[tokio::test]
async fn pause_then_resume_round_trip_to_the_host_with_the_right_sandbox() {
    let host = Arc::new(FakeHost::default());
    let addr = boot_grpc_server(host.clone()).await;
    let client = connect_grpc_client(addr).await;

    let sandbox_id = SandboxId::new();

    // Drive both verbs through the real proto → server → trait path.
    client.pause(sandbox_id).await.expect("client pause");
    client.resume(sandbox_id).await.expect("client resume");

    // Each verb reached the host once, for this sandbox, in order — proving
    // PauseSandbox/ResumeSandbox are bound to the right handlers and not
    // swapped.
    let calls = host.calls.lock().clone();
    assert_eq!(
        calls,
        vec![("pause", sandbox_id), ("resume", sandbox_id)],
        "pause/resume did not round-trip in order with the right sandbox id",
    );
}
