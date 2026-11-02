//! Local TLS process proof; no guest or deployment is involved.
use engram_protocol::{
    grpc_client::GrpcHostClient,
    host_tls::{HostClientTls, HostPeer},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};
fn openssl(dir: &std::path::Path, args: &[&str]) {
    let output = std::process::Command::new("openssl")
        .current_dir(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(output.status.success(), "openssl fixture generation failed");
}
#[tokio::test]
async fn managed_host_requires_pinned_mutual_tls() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path();
    openssl(
        dir,
        &[
            "req",
            "-x509",
            "-newkey",
            "rsa:2048",
            "-nodes",
            "-keyout",
            "ca.key",
            "-out",
            "ca.pem",
            "-days",
            "1",
            "-subj",
            "/CN=test-ca",
        ],
    );
    std::fs::write(
        dir.join("extensions"),
        "subjectAltName=DNS:localhost\nextendedKeyUsage=serverAuth,clientAuth\n",
    )
    .unwrap();
    for name in ["host", "coord", "other"] {
        openssl(
            dir,
            &[
                "req",
                "-newkey",
                "rsa:2048",
                "-nodes",
                "-keyout",
                &format!("{name}.key"),
                "-out",
                &format!("{name}.csr"),
                "-subj",
                "/CN=localhost",
            ],
        );
        openssl(
            dir,
            &[
                "x509",
                "-req",
                "-in",
                &format!("{name}.csr"),
                "-CA",
                "ca.pem",
                "-CAkey",
                "ca.key",
                "-CAcreateserial",
                "-out",
                &format!("{name}.pem"),
                "-days",
                "1",
                "-extfile",
                "extensions",
            ],
        );
        openssl(
            dir,
            &[
                "x509",
                "-in",
                &format!("{name}.pem"),
                "-outform",
                "DER",
                "-out",
                &format!("{name}.der"),
            ],
        );
    }
    let host_id = engram_core::HostId::new();
    use engram_core::{traits::HostClient, types::managed_deny::*};
    let ca = engram_egress_proxy::Ca::load_or_generate(&dir.join("proxy-ca")).unwrap();
    let mut args = engram_egress_proxyd::DaemonArgs::new(
        dir.to_path_buf(),
        ca.cert_pem.clone(),
        ca.key_pair.serialize_pem(),
        "http://127.0.0.1:1".into(),
        None,
        host_id,
    );
    [args.proxy_port, args.dns_port, args.gateway_port] = engram_testkit::net::free_tcp_udp_ports();
    let daemon = tokio::spawn(engram_egress_proxyd::run(args));
    let handle = Arc::new(engram_host_agent::proxyd_client::ProxydHandle::new(
        dir.join(engram_egress_proto::CONTROL_SOCK_NAME),
    ));
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if handle.hello().await.is_ok() {
                break;
            }
            assert!(!daemon.is_finished());
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let egress = Arc::new(engram_host_agent::egress::HostEgress::new(
        handle,
        ca.cert_pem,
        None,
    ));
    let backend = Arc::new(EndpointFixture(egress.clone()));
    let bindings = engram_host_agent::bindings::BindingStore::open(dir.join("bindings")).unwrap();
    let target = ManagedPolicyTarget {
        session_id: engram_core::SessionId::new(),
        sandbox_id: engram_core::SandboxId::new(),
        host_id,
        binding_epoch: 1,
    };
    bindings
        .bind(target.session_id, target.sandbox_id, target.binding_epoch)
        .unwrap();
    let owner =
        engram_host_agent::managed_policy::ManagedOwnership::for_mode(&dir.join("bindings"), true)
            .unwrap()
            .unwrap();
    let managed = Arc::new(
        engram_host_agent::managed_policy::ManagedPolicyHost::new(
            host_id,
            "tls-test-host".into(),
            bindings.clone(),
            backend.clone(),
            egress,
            owner,
        )
        .unwrap(),
    );
    let hub = Arc::new(engram_host_agent::harness::HarnessHub::new(
        engram_host_agent::harness::event_sink_to(|_, _, _, _| async { Ok(()) }),
        bindings,
    ));
    let local = Arc::new(
        engram_host_agent::host_client::LocalHostClient::new(backend, hub)
            .with_managed_policy(managed),
    );
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let pins = |name: &str| {
        BTreeSet::from([engram_protocol::host_tls::certificate_pin(
            &std::fs::read(dir.join(format!("{name}.der"))).unwrap(),
        )])
    };
    let server = tokio::spawn(engram_host_agent::grpc_server::boot_with_managed_tls(
        addr,
        local.clone(),
        None,
        engram_host_agent::session_epochs::SessionEpochStore::open(dir.join("epochs")).unwrap(),
        None,
        Some(engram_host_agent::managed_tls::ManagedHostTls {
            certificate: dir.join("host.pem"),
            key: dir.join("host.key"),
            client_ca: dir.join("ca.pem"),
            coordinator_pins: pins("coord"),
        }),
    ));
    let address = format!("https://{addr}");
    let mut client = HostClientTls {
        client_certificate: dir.join("coord.pem"),
        client_key: dir.join("coord.key"),
        hosts: BTreeMap::from([(
            host_id,
            HostPeer {
                endpoint: address.clone(),
                server_name: "localhost".into(),
                server_ca: dir.join("ca.pem"),
                server_pins: pins("host"),
            },
        )]),
    };
    let connect = |config: &HostClientTls| {
        config
            .endpoint(
                host_id,
                &address,
                tonic::transport::Endpoint::from_shared(address.clone()).unwrap(),
            )
            .unwrap()
            .connect_lazy()
    };
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if GrpcHostClient::new(connect(&client)).ping().await.is_ok() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let remote = GrpcHostClient::new(connect(&client));
    assert!(remote
        .read_managed_policy(target.clone())
        .await
        .unwrap()
        .is_none());
    let context = ManagedPolicyContext {
        target: target.clone(),
        operator_generation: 1,
        operator_identity: format!("sha256:{}", "a".repeat(64)),
        binding_hash: None,
    };
    let policy = engram_core::types::egress::SessionEgressPolicy {
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
        secret_mode: engram_core::types::image::SecretMode::Literal,
        apps: vec![],
        managed_deny: Some(ManagedDenyPolicy::new(1, ["mcp.example".into()]).unwrap()),
    };
    let apply = ManagedPolicyApply {
        context,
        expected: None,
        egress: policy,
    };
    let ack = remote.apply_managed_policy(apply.clone()).await.unwrap();
    assert_eq!(
        remote.read_managed_policy(target.clone()).await.unwrap(),
        Some(ack.clone())
    );
    assert_eq!(remote.apply_managed_policy(apply).await.unwrap(), ack);
    assert!(
        ack.context.binding_hash.is_none() && ack.network_proof.is_none(),
        "bootstrap is not final network proof"
    );
    let snapshot: engram_core::types::snapshot::SnapshotMetadata =
        serde_json::from_value(serde_json::json!({
            "id": engram_core::SnapshotId::new(), "size_bytes": 0,
            "created_at": "2026-01-01T00:00:00Z", "image_version": "fixture",
        }))
        .unwrap();
    let managed_fence = engram_core::traits::SessionFence::new(target.session_id, 1);
    let ordinary_fence = engram_core::traits::SessionFence::new(engram_core::SessionId::new(), 1);
    for host in [&*local as &dyn HostClient, &remote as &dyn HostClient] {
        assert!(host.restore(snapshot.clone(), ordinary_fence).await.is_ok());
        assert!(host
            .restore_base_for_session(snapshot.clone(), Default::default(), vec![], ordinary_fence)
            .await
            .is_ok());
        assert!(host
            .resume(engram_core::SandboxId::new(), ordinary_fence)
            .await
            .is_ok());
        for error in [
            host.resume(target.sandbox_id, ordinary_fence)
                .await
                .unwrap_err(),
            host.restore(snapshot.clone(), managed_fence)
                .await
                .unwrap_err(),
            host.restore_base_for_session(
                snapshot.clone(),
                Default::default(),
                vec![],
                managed_fence,
            )
            .await
            .unwrap_err(),
            host.resume(target.sandbox_id, managed_fence)
                .await
                .unwrap_err(),
        ] {
            assert!(
                error
                    .to_string()
                    .contains("managed restore quarantine unavailable"),
                "{error}"
            );
        }
    }
    // Missing desired state and a corrupt marker still identify a known managed session.
    std::fs::remove_file(
        dir.join("bindings")
            .join(format!("{}.managed", target.session_id)),
    )
    .unwrap();
    std::fs::write(
        dir.join("bindings")
            .join(format!("{}.managed-required", target.session_id)),
        b"corrupt",
    )
    .unwrap();
    assert!(remote
        .restore(snapshot, managed_fence)
        .await
        .unwrap_err()
        .to_string()
        .contains("managed restore quarantine unavailable"));
    client.client_certificate = dir.join("other.pem");
    client.client_key = dir.join("other.key");
    assert!(
        GrpcHostClient::new(connect(&client)).ping().await.is_err(),
        "same CA does not replace caller pins"
    );
    client.client_certificate = dir.join("coord.pem");
    client.client_key = dir.join("coord.key");
    client.hosts.get_mut(&host_id).unwrap().server_pins = pins("other");
    assert!(
        GrpcHostClient::new(connect(&client)).ping().await.is_err(),
        "server pin required as well as CA/name"
    );
    assert!(client
        .endpoint(
            host_id,
            &format!("http://{addr}"),
            tonic::transport::Endpoint::from_shared(format!("http://{addr}")).unwrap()
        )
        .is_err());
    let plaintext = tonic::transport::Endpoint::from_shared(format!("http://{addr}"))
        .unwrap()
        .connect_lazy();
    assert!(GrpcHostClient::new(plaintext).ping().await.is_err());
    client.hosts.get_mut(&host_id).unwrap().server_pins = pins("host");
    client.hosts.get_mut(&host_id).unwrap().server_name = "wrong.example".into();
    assert!(GrpcHostClient::new(connect(&client)).ping().await.is_err());
    client.hosts.get_mut(&host_id).unwrap().server_name = "localhost".into();
    client.hosts.get_mut(&host_id).unwrap().server_ca = dir.join("coord.pem");
    assert!(GrpcHostClient::new(connect(&client)).ping().await.is_err());
    client.hosts.get_mut(&host_id).unwrap().server_ca = dir.join("ca.pem");
    let mut raw =
        engram_protocol::grpc::host_service_client::HostServiceClient::new(connect(&client));
    assert!(
        raw.ping(engram_protocol::grpc::Empty {}).await.is_err(),
        "missing protocol version rejected"
    );
    let mut old = tonic::Request::new(engram_protocol::grpc::Empty {});
    old.metadata_mut().insert(
        engram_protocol::wire::WIRE_VERSION_METADATA_KEY,
        "1".parse().unwrap(),
    );
    assert!(raw.ping(old).await.is_err(), "old protocol rejected");
    let no_identity = tonic::transport::Endpoint::from_shared(address.clone())
        .unwrap()
        .tls_config(
            tonic::transport::ClientTlsConfig::new()
                .domain_name("localhost")
                .ca_certificate(tonic::transport::Certificate::from_pem(
                    std::fs::read(dir.join("ca.pem")).unwrap(),
                )),
        )
        .unwrap()
        .connect_lazy();
    assert!(GrpcHostClient::new(no_identity).ping().await.is_err());
    daemon.abort();
    let _ = daemon.await;
    server.abort();
    let _ = server.await;
}

struct EndpointFixture(Arc<engram_host_agent::egress::HostEgress>);
#[async_trait::async_trait]
impl engram_core::traits::SandboxBackend for EndpointFixture {
    async fn create(
        &self,
        _: engram_core::types::sandbox::SandboxSpec,
    ) -> Result<engram_core::SandboxId, engram_core::SandboxError> {
        unimplemented!("fixture does not create guests")
    }
    async fn exec_stream(
        &self,
        _: engram_core::SandboxId,
        _: engram_core::types::sandbox::ExecRequest,
    ) -> Result<engram_core::types::sandbox::ExecStream, engram_core::SandboxError> {
        unimplemented!("fixture does not execute")
    }
    async fn snapshot(
        &self,
        _: engram_core::SandboxId,
    ) -> Result<engram_core::types::snapshot::SnapshotMetadata, engram_core::SandboxError> {
        unimplemented!("fixture does not snapshot")
    }
    fn snapshot_path_for(&self, _: engram_core::SnapshotId) -> std::path::PathBuf {
        std::path::PathBuf::new()
    }
    async fn restore(
        &self,
        _: engram_core::types::snapshot::SnapshotMetadata,
    ) -> Result<engram_core::SandboxId, engram_core::SandboxError> {
        Ok(engram_core::SandboxId::new())
    }
    async fn restore_base_for_session(
        &self,
        _: engram_core::types::snapshot::SnapshotMetadata,
        _: std::collections::HashMap<String, String>,
        _: Vec<engram_core::types::sandbox::AuxRoDrive>,
    ) -> Result<engram_core::SandboxId, engram_core::SandboxError> {
        Ok(engram_core::SandboxId::new())
    }
    async fn resume(&self, _: engram_core::SandboxId) -> Result<(), engram_core::SandboxError> {
        Ok(())
    }
    async fn destroy(&self, _: engram_core::SandboxId) -> Result<(), engram_core::SandboxError> {
        Ok(())
    }
    async fn list(&self) -> Result<Vec<engram_core::SandboxId>, engram_core::SandboxError> {
        Ok(vec![])
    }
    async fn guest_endpoints(
        &self,
        _: engram_core::SandboxId,
    ) -> Option<engram_core::types::endpoints::GuestEndpoints> {
        Some(engram_core::types::endpoints::GuestEndpoints {
            egress_identity: "10.200.0.42".parse().unwrap(),
            dial_ip: "10.200.0.42".parse().unwrap(),
            netns: None,
            vsock_uds: None,
        })
    }
    async fn notify_session_policy(
        &self,
        policy: engram_core::types::egress::SessionEgressPolicy,
    ) -> Result<(), engram_core::SandboxError> {
        self.0
            .apply_policy(policy)
            .await
            .map_err(|e| engram_core::SandboxError::InvalidSpec(e.to_string()))
    }
}
