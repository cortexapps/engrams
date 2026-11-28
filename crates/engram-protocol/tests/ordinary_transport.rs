//! Real HTTP/2 exchange with the strict ordinary wire 38 source contract.
use engram_core::{traits::SessionFence, types::egress::SessionEgressPolicy};
use engram_protocol::{grpc, grpc_client::GrpcHostClient};
use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use tonic::{Request, Response, Status};

const POLICY: &[u8] = include_bytes!("golden/ordinary_v38/session_egress_policy_minted.bin");
const AGENT: &[u8] = include_bytes!("golden/ordinary_v38/agent_spec.bin");
const SNAPSHOT: &[u8] = include_bytes!("golden/ordinary_v38/snapshot_metadata.bin");
type Reply<T> =
    std::pin::Pin<Box<dyn std::future::Future<Output = Result<Response<T>, Status>> + Send>>;

#[derive(Clone)]
struct Predecessor(Arc<AtomicUsize>);
impl tonic::server::NamedService for Predecessor {
    const NAME: &'static str = "engram.host.v1.HostService";
}
impl tonic::server::UnaryService<grpc::StartAgentRequest> for Predecessor {
    type Response = grpc::Empty;
    type Future = Reply<Self::Response>;
    fn call(&mut self, request: Request<grpc::StartAgentRequest>) -> Self::Future {
        assert!(request.metadata().contains_key("grpc-timeout"));
        let body = request.into_inner();
        assert_eq!(body.policy_bincode, POLICY);
        assert_eq!(body.agent_bincode, AGENT);
        assert_eq!(body.session_id, [0x30; 16]);
        assert_eq!(body.sandbox_id, [0x31; 16]);
        assert_eq!(body.fencing_epoch, 7);
        self.0.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(Response::new(grpc::Empty {})) })
    }
}
impl tonic::server::UnaryService<grpc::FencedSandboxRequest> for Predecessor {
    type Response = grpc::SnapshotResponse;
    type Future = Reply<Self::Response>;
    fn call(&mut self, request: Request<grpc::FencedSandboxRequest>) -> Self::Future {
        let body = request.into_inner();
        assert_eq!(body.uuid, [0x31; 16]);
        assert_eq!(body.session_id, [0x30; 16]);
        assert_eq!(body.fencing_epoch, 7);
        self.0.fetch_add(1, Ordering::SeqCst);
        Box::pin(async {
            Ok(Response::new(grpc::SnapshotResponse {
                metadata_bincode: SNAPSHOT.to_vec(),
            }))
        })
    }
}
impl tonic::codegen::Service<tonic::codegen::http::Request<tonic::body::Body>> for Predecessor {
    type Response = tonic::codegen::http::Response<tonic::body::Body>;
    type Error = std::convert::Infallible;
    type Future = std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Self::Response, Self::Error>> + Send>,
    >;
    fn poll_ready(
        &mut self,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        std::task::Poll::Ready(Ok(()))
    }
    fn call(&mut self, request: tonic::codegen::http::Request<tonic::body::Body>) -> Self::Future {
        let service = self.clone();
        Box::pin(async move {
            if request
                .headers()
                .get("x-engram-wire-version")
                .and_then(|v| v.to_str().ok())
                != Some("38")
            {
                return Ok(Status::failed_precondition("ordinary wire 38 required").into_http());
            }
            assert!(!request.headers().contains_key("x-engram-managed-protocol"));
            match request.uri().path() {
                "/engram.host.v1.HostService/StartAgent" => {
                    Ok(tonic::server::Grpc::new(tonic_prost::ProstCodec::<
                        grpc::Empty,
                        grpc::StartAgentRequest,
                    >::default())
                    .unary(service, request)
                    .await)
                }
                "/engram.host.v1.HostService/Snapshot" => {
                    Ok(tonic::server::Grpc::new(tonic_prost::ProstCodec::<
                        grpc::SnapshotResponse,
                        grpc::FencedSandboxRequest,
                    >::default())
                    .unary(service, request)
                    .await)
                }
                _ => Ok(Status::unimplemented("predecessor method").into_http()),
            }
        })
    }
}

#[tokio::test]
async fn new_client_sends_predecessor_policy_and_reads_predecessor_snapshot() {
    use futures::FutureExt;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let incoming = futures::stream::unfold(listener, |listener| async move {
        Some((listener.accept().await.map(|(socket, _)| socket), listener))
    });
    let calls = Arc::new(AtomicUsize::new(0));
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let mut server = tokio::spawn(
        tonic::transport::Server::builder()
            .add_service(Predecessor(calls.clone()))
            .serve_with_incoming_shutdown(incoming, async {
                let _ = stopped.await;
            }),
    );
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        std::panic::AssertUnwindSafe(async {
            let channel = tonic::transport::Endpoint::from_shared(format!("http://{address}"))
                .unwrap()
                .connect()
                .await
                .unwrap();
            let client = GrpcHostClient::new(channel);
            let policy: SessionEgressPolicy =
                bincode::deserialize(include_bytes!("golden/session_egress_policy_minted.bin"))
                    .unwrap();
            let id = policy.sandbox_id;
            let fence = SessionFence::new(policy.session_id, 7);
            client
                .start_agent(id, bincode::deserialize(AGENT).unwrap(), policy, fence)
                .await
                .unwrap();
            let snapshot = client.snapshot(id, fence).await.unwrap();
            assert_eq!(bincode::serialize(&snapshot).unwrap(), SNAPSHOT);
            assert_eq!(calls.load(Ordering::SeqCst), 2);
        })
        .catch_unwind(),
    )
    .await;
    let _ = stop.send(());
    if tokio::time::timeout(Duration::from_secs(2), &mut server)
        .await
        .is_err()
    {
        server.abort();
        let _ = server.await;
    }
    result.unwrap().unwrap();
}
