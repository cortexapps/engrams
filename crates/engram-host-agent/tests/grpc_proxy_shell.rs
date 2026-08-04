//! gRPC-layer integration test for `ProxyShell`.
//!
//! Closes the test gap that let the prod session 8725648d bug ship:
//! `crates/engram-host-agent/tests/e2e_shell.rs` calls
//! `proxy_shell::open_shell_tunnel_at` directly, bypassing the gRPC
//! tunnel entirely. The bug lived in the gRPC server's
//! `proxy_shell` handler — specifically a "defensively forward the
//! first frame's payload" branch that turned the sandbox_id sentinel
//! into an empty `Binary` WS frame to ttyd, which 1.7.8 TCP-RSTs on.
//! This test stands up the real gRPC server with a stub HostClient
//! and asserts the structural invariant the new schema enforces:
//!
//! **The host-side ShellTunnel.outbound_rx receives the caller's
//! frames verbatim and in order. No spurious frames are injected.**
//!
//! With the new `ProxyShellMessage { oneof body { Open | Text | ... }}`
//! schema, the Open variant is statically distinct from the data
//! variants — the gRPC server cannot mistakenly forward it as data.
//! This test is a regression guard so a future refactor doesn't
//! reintroduce the same class of bug under a different shape.
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
use engram_core::types::shell::{ShellFrame, ShellTunnel, ShellTunnelEnds};
use engram_core::types::snapshot::SnapshotMetadata;
use engram_core::{SandboxId, SessionId};
use parking_lot::Mutex;

/// Stub HostClient whose only meaningful method is `proxy_shell`.
/// Every other trait method panics — we want a loud failure if the
/// test starts touching surface area it doesn't intend to cover.
/// `proxy_shell` opens a fresh ShellTunnel and stashes the host-side
/// ends (which the test inspects) plus records the sandbox_id it
/// was asked to open.
#[derive(Default)]
struct FakeHost {
    captured_open: Mutex<Option<SandboxId>>,
    tunnel_ends: Mutex<Option<ShellTunnelEnds>>,
}

#[async_trait]
impl HostClient for FakeHost {
    async fn create(&self, _: SandboxSpec) -> Result<SandboxId, SandboxError> {
        unreachable!("proxy_shell test path doesn't call create")
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
        _: SandboxId,
        _: AgentSpec,
        _: SessionEgressPolicy,
        _: SessionFence,
    ) -> Result<(), SandboxError> {
        unreachable!()
    }
    async fn guest_ip(&self, _: SandboxId) -> Option<std::net::Ipv4Addr> {
        None
    }
    async fn bind_session(&self, _: SessionId, _: SandboxId, _: u64) {}
    async fn unbind_session(&self, _: SessionId) {}
    async fn send_prompt(
        &self,
        _: SandboxId,
        _: String,
        _: String,
        _: Option<String>,
    ) -> Result<(), SandboxError> {
        unreachable!()
    }
    async fn proxy_shell(&self, sandbox_id: SandboxId) -> Result<ShellTunnel, SandboxError> {
        *self.captured_open.lock() = Some(sandbox_id);
        let (tunnel, ends) = ShellTunnel::pair();
        *self.tunnel_ends.lock() = Some(ends);
        Ok(tunnel)
    }
    fn harness_dial(&self) -> HarnessDial {
        HarnessDial::Vsock
    }
    fn set_harness_sink(&self, _: HarnessSink) {}
}

/// Bind a random localhost port, return the addr, and free the port.
/// tonic's `Server::serve` rebinds; the race window is acceptable for
/// in-proc tests.
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
        // Ignore the result — the runtime collects the task when the
        // test ends and tonic surfaces shutdown as Err which is
        // expected.
        let _ = engram_host_agent::grpc_server::boot(
            addr,
            host_dyn,
            None,
            engram_host_agent::session_epochs::ephemeral(),
            None,
        )
        .await;
    });
    // Spin until the port accepts; tonic binds asynchronously.
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

/// The regression guard: when the coord sends a Text frame through
/// the gRPC tunnel, the host-side ShellTunnel.outbound_rx must
/// receive *that* Text frame as its very first item — never a
/// synthetic Binary(empty) or any other variant injected by the
/// gRPC handshake.
///
/// Before the oneof migration, the gRPC server's "defensively
/// forward first frame's payload" branch caused an empty Binary
/// frame to land here before the caller's Text frame.
#[tokio::test]
async fn proxy_shell_forwards_only_caller_frames_no_open_sentinel_leak() {
    let host = Arc::new(FakeHost::default());
    let addr = boot_grpc_server(host.clone()).await;
    let client = connect_grpc_client(addr).await;

    let sandbox_id = SandboxId::new();
    let tunnel = client
        .proxy_shell(sandbox_id)
        .await
        .expect("client proxy_shell");

    // Push the SAME frame shape the browser would push first (a
    // Text frame carrying the AuthToken JSON).
    tunnel
        .outbound
        .send(ShellFrame::Text("{\"AuthToken\":\"\"}".into()))
        .await
        .expect("push text");

    // The host's proxy_shell impl captured the tunnel ends — pull
    // the first frame the gRPC server would have forwarded to ttyd.
    let ends = host
        .tunnel_ends
        .lock()
        .take()
        .expect("server didn't call inner proxy_shell");
    let ShellTunnelEnds {
        mut outbound_rx, ..
    } = ends;

    let first = tokio::time::timeout(Duration::from_secs(2), outbound_rx.recv())
        .await
        .expect("no frame reached the host within 2s")
        .expect("host-side tunnel closed without delivering");

    match &first {
        ShellFrame::Text(t) => {
            assert_eq!(t, "{\"AuthToken\":\"\"}");
        }
        ShellFrame::Binary(b) if b.is_empty() => {
            panic!(
                "REGRESSION: gRPC server forwarded an empty Binary frame \
                 before the caller's frame. This is the prod session \
                 8725648d bug — ttyd 1.7.8 TCP-RSTs on this. The \
                 ProxyShellMessage oneof was supposed to make this \
                 structurally impossible."
            );
        }
        other => panic!("unexpected first frame: {other:?}"),
    }

    // sandbox_id round-tripped correctly via the Open variant.
    let captured = host
        .captured_open
        .lock()
        .expect("server didn't extract sandbox_id");
    assert_eq!(captured, sandbox_id);
}

/// Binary frames round-trip (and the Binary discriminator is correct
/// — different from the old `kind: BINARY, data: empty` pattern that
/// looked identical to a sentinel before the oneof split).
#[tokio::test]
async fn proxy_shell_binary_frame_round_trips_through_grpc_layer() {
    let host = Arc::new(FakeHost::default());
    let addr = boot_grpc_server(host.clone()).await;
    let client = connect_grpc_client(addr).await;

    let sandbox_id = SandboxId::new();
    let tunnel = client.proxy_shell(sandbox_id).await.expect("proxy_shell");

    tunnel
        .outbound
        .send(ShellFrame::Binary(bytes::Bytes::from_static(
            b"\x00\x01\x02",
        )))
        .await
        .expect("push binary");

    let ends = host
        .tunnel_ends
        .lock()
        .take()
        .expect("inner proxy_shell not called");
    let ShellTunnelEnds {
        mut outbound_rx, ..
    } = ends;

    let first = tokio::time::timeout(Duration::from_secs(2), outbound_rx.recv())
        .await
        .expect("no frame within 2s")
        .expect("tunnel closed");
    match first {
        ShellFrame::Binary(b) => assert_eq!(&b[..], b"\x00\x01\x02"),
        other => panic!("expected Binary, got {other:?}"),
    }
}

/// Close-with-code from the caller flows through the gRPC tunnel
/// unchanged (the old `kind/data/close_code` field-bag could lose
/// the code in translation; the oneof makes it a typed variant).
#[tokio::test]
async fn proxy_shell_close_with_code_round_trips() {
    use engram_core::types::shell::ShellClose;
    let host = Arc::new(FakeHost::default());
    let addr = boot_grpc_server(host.clone()).await;
    let client = connect_grpc_client(addr).await;

    let sandbox_id = SandboxId::new();
    let tunnel = client.proxy_shell(sandbox_id).await.expect("proxy_shell");

    tunnel
        .outbound
        .send(ShellFrame::Close(Some(ShellClose {
            code: 1001,
            reason: "going away".into(),
        })))
        .await
        .expect("push close");

    let ends = host
        .tunnel_ends
        .lock()
        .take()
        .expect("inner proxy_shell not called");
    let ShellTunnelEnds {
        mut outbound_rx, ..
    } = ends;

    let first = tokio::time::timeout(Duration::from_secs(2), outbound_rx.recv())
        .await
        .expect("no frame within 2s")
        .expect("tunnel closed");
    match first {
        ShellFrame::Close(Some(c)) => {
            assert_eq!(c.code, 1001);
            assert_eq!(c.reason, "going away");
        }
        other => panic!("expected Close(Some(1001)), got {other:?}"),
    }
}
