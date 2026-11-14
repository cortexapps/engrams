//! Real gRPC lifecycle, cancellation, message size, and wire compatibility tests.
// tests drive a live system; wall clock/OS entropy here is input, not a decision source (ADR 0098 D1)
#![allow(clippy::disallowed_methods)]

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use engram_core::error::SandboxError;
use engram_core::traits::sandbox::{HarnessDial, HarnessSink};
use engram_core::traits::{HostClient, SessionFence};
use engram_core::types::egress::SessionEgressPolicy;
use engram_core::types::sandbox::{AgentSpec, ExecRequest, ExecStream, SandboxSpec};
use engram_core::types::shell::ShellTunnel;
use engram_core::types::snapshot::SnapshotMetadata;
use engram_core::{SandboxId, SessionId};
use parking_lot::Mutex;

/// Records lifecycle calls and supplies a stalled file source for cancellation.
#[derive(Default)]
struct FakeHost {
    calls: Mutex<Vec<(&'static str, SandboxId)>>,
    starts: Mutex<Vec<StartCall>>,
    file_drop: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    prompt_bytes: Mutex<Vec<usize>>,
}

struct StartCall {
    sandbox: SandboxId,
    agent: AgentSpec,
    policy: SessionEgressPolicy,
    fence: SessionFence,
}

#[async_trait]
impl HostClient for FakeHost {
    async fn pause(&self, id: SandboxId, _: SessionFence) -> Result<(), SandboxError> {
        self.calls.lock().push(("pause", id));
        Ok(())
    }
    async fn resume(&self, id: SandboxId, _: SessionFence) -> Result<(), SandboxError> {
        self.calls.lock().push(("resume", id));
        Ok(())
    }

    async fn read_file(
        &self,
        _: SandboxId,
        path: String,
    ) -> Result<
        (
            engram_core::types::sandbox::SessionFileMetadata,
            engram_core::types::sandbox::SessionFileStream,
        ),
        SandboxError,
    > {
        Ok((
            engram_core::types::sandbox::SessionFileMetadata {
                path,
                size_bytes: 1,
                sha256: "a".repeat(64),
            },
            Box::pin(StalledFile(self.file_drop.lock().take())),
        ))
    }

    async fn create(&self, _: SandboxSpec) -> Result<SandboxId, SandboxError> {
        unreachable!("pause/resume test path doesn't call create")
    }
    async fn destroy(&self, _: SandboxId, _: SessionFence) -> Result<(), SandboxError> {
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
    async fn snapshot(
        &self,
        _: SandboxId,
        _: SessionFence,
    ) -> Result<SnapshotMetadata, SandboxError> {
        unreachable!()
    }
    async fn restore(
        &self,
        _: SnapshotMetadata,
        _: SessionFence,
    ) -> Result<SandboxId, SandboxError> {
        unreachable!()
    }
    async fn start_agent(
        &self,
        sandbox: SandboxId,
        agent: AgentSpec,
        policy: SessionEgressPolicy,
        fence: SessionFence,
    ) -> Result<(), SandboxError> {
        self.starts.lock().push(StartCall {
            sandbox,
            agent,
            policy,
            fence,
        });
        Ok(())
    }
    async fn guest_ip(&self, _: SandboxId) -> Option<std::net::Ipv4Addr> {
        None
    }
    async fn bind_session(
        &self,
        _: SessionId,
        _: SandboxId,
        _: u64,
    ) -> Result<(), engram_core::SandboxError> {
        Ok(())
    }
    async fn unbind_session(&self, _: SessionId) {}
    async fn send_prompt(
        &self,
        _: SandboxId,
        _: String,
        text: String,
        _: Option<String>,
    ) -> Result<(), SandboxError> {
        self.prompt_bytes.lock().push(text.len());
        Ok(())
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
        let _ = engram_host_agent::grpc_server::boot(
            addr,
            host_dyn,
            engram_host_agent::session_epochs::ephemeral(),
            None,
        )
        .await;
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
    // ADR 0079: epoch threaded by the op executor; 0 until the verb migrates.
    let fence = SessionFence::unfenced();
    client.pause(sandbox_id, fence).await.expect("client pause");
    client
        .resume(sandbox_id, fence)
        .await
        .expect("client resume");

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

struct StalledFile(Option<tokio::sync::oneshot::Sender<()>>);
impl futures::Stream for StalledFile {
    type Item = Result<bytes::Bytes, SandboxError>;
    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        std::task::Poll::Pending
    }
}
impl Drop for StalledFile {
    fn drop(&mut self) {
        if let Some(signal) = self.0.take() {
            let _ = signal.send(());
        }
    }
}

#[tokio::test]
async fn dropping_stalled_file_response_drops_host_source() {
    use engram_protocol::grpc::host_service_server::HostService;
    use futures::StreamExt;
    let (signal, dropped) = tokio::sync::oneshot::channel();
    let host = Arc::new(FakeHost::default());
    *host.file_drop.lock() = Some(signal);
    let service = engram_host_agent::grpc_server::HostServiceImpl::new(
        host,
        engram_host_agent::session_epochs::ephemeral(),
    );
    let mut response = service
        .read_file(tonic::Request::new(
            engram_protocol::grpc::HostReadFileRequest {
                sandbox_id: SandboxId::new().as_uuid().as_bytes().to_vec(),
                path: "/tmp/stalled.bin".into(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(matches!(
        response.next().await.unwrap().unwrap().frame,
        Some(engram_protocol::grpc::host_read_file_response::Frame::Metadata(_))
    ));
    tokio::task::yield_now().await;
    drop(response);
    tokio::time::timeout(Duration::from_millis(100), dropped)
        .await
        .expect("downstream cancellation must drop the stalled host source")
        .unwrap();
}

// These bytes come directly from ec526661, before managed authority was added.
const OLD_POLICY: &[u8] = include_bytes!(
    "../../engram-protocol/tests/golden/ordinary_v38/session_egress_policy_minted.bin"
);
const OLD_AGENT: &[u8] =
    include_bytes!("../../engram-protocol/tests/golden/ordinary_v38/agent_spec.bin");

#[tokio::test]
async fn predecessor_38_caller_reaches_new_host_with_exact_policy_and_fence() {
    use engram_protocol::grpc::{host_service_client::HostServiceClient, StartAgentRequest};
    let host = Arc::new(FakeHost::default());
    let addr = boot_grpc_server(host.clone()).await;
    let mut client = HostServiceClient::connect(format!("http://{addr}"))
        .await
        .unwrap();
    let expected: SessionEgressPolicy = bincode::deserialize(include_bytes!(
        "../../engram-protocol/tests/golden/session_egress_policy_minted.bin"
    ))
    .unwrap();
    let message = StartAgentRequest {
        sandbox_id: expected.sandbox_id.as_uuid().as_bytes().to_vec(),
        agent_bincode: OLD_AGENT.to_vec(),
        policy_bincode: OLD_POLICY.to_vec(),
        session_id: expected.session_id.as_uuid().as_bytes().to_vec(),
        fencing_epoch: 7,
    };
    let request = |message: StartAgentRequest, version: &str| {
        let mut req = tonic::Request::new(message);
        req.set_timeout(Duration::from_secs(3));
        req.metadata_mut()
            .insert("x-engram-wire-version", version.parse().unwrap());
        req
    };
    client
        .start_agent(request(message.clone(), "38"))
        .await
        .unwrap();
    {
        let starts = host.starts.lock();
        assert_eq!(starts.len(), 1);
        assert_eq!(starts[0].sandbox, expected.sandbox_id);
        assert_eq!(starts[0].policy, expected);
        assert_eq!(bincode::serialize(&starts[0].agent).unwrap(), OLD_AGENT);
        assert_eq!(starts[0].fence.session_id, expected.session_id);
        assert_eq!(starts[0].fence.epoch, 7);
    }
    for version in ["0", "43", "unknown"] {
        assert_eq!(
            client
                .start_agent(request(message.clone(), version))
                .await
                .unwrap_err()
                .code(),
            tonic::Code::FailedPrecondition
        );
    }
    for bad_bytes in [
        OLD_POLICY[..OLD_POLICY.len() - 1].to_vec(),
        [OLD_POLICY, &[0]].concat(),
        include_bytes!("../../engram-protocol/tests/golden/session_egress_policy_managed.bin")
            .to_vec(),
    ] {
        let mut bad = message.clone();
        bad.policy_bincode = bad_bytes;
        bad.fencing_epoch = 8;
        assert_eq!(
            client
                .start_agent(request(bad, "38"))
                .await
                .unwrap_err()
                .code(),
            tonic::Code::InvalidArgument
        );
    }
    let mut stale = message;
    stale.fencing_epoch = 6;
    assert_eq!(
        client
            .start_agent(request(stale, "38"))
            .await
            .unwrap_err()
            .code(),
        tonic::Code::FailedPrecondition
    );
    assert_eq!(
        host.starts.lock().len(),
        1,
        "refusals must not reach the backend"
    );
    let mut pause = tonic::Request::new(engram_protocol::grpc::FencedSandboxRequest {
        uuid: expected.sandbox_id.as_uuid().as_bytes().to_vec(),
        session_id: expected.session_id.as_uuid().as_bytes().to_vec(),
        fencing_epoch: 7,
    });
    pause.set_timeout(Duration::from_secs(3));
    pause
        .metadata_mut()
        .insert("x-engram-wire-version", "38".parse().unwrap());
    client.pause_sandbox(pause).await.unwrap();
    assert_eq!(*host.calls.lock(), vec![("pause", expected.sandbox_id)]);
}

/// The host accepts requests larger than tonic's 4 MiB default decode limit.
#[tokio::test]
async fn the_host_accepts_a_request_above_the_tonic_default_limit() {
    let host = Arc::new(FakeHost::default());
    let addr = boot_grpc_server(host.clone()).await;
    let client = connect_grpc_client(addr).await;

    let text = "x".repeat(6 * 1024 * 1024);
    client
        .send_prompt(SandboxId::new(), "p1".into(), text.clone(), None)
        .await
        .expect("a 6 MiB request reaches the host");

    assert_eq!(*host.prompt_bytes.lock(), vec![text.len()]);
}
