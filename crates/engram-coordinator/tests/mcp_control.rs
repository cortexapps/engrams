use engram_coordinator::{
    mcp::{control::*, model::Registry, *},
    oauth::OAuthManager,
};
use engram_core::traits::StaticSecretStore;
use engram_crypto::EnvVarKeyProvider;
use engram_protocol::mcp as wire;
use engram_sim::{ManualClock, SimEntropy, SimMetadataStore};
use std::{
    collections::{BTreeMap, HashMap},
    sync::Arc,
};
use tokio::io::{AsyncBufReadExt, BufReader};

#[tokio::test]
async fn control_listener_requires_tls_client_certificate_pin_and_protocol_one() {
    let mut fixture = tokio::process::Command::new("node")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/mcp-worker.mjs"
        ))
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(fixture.stdout.take().unwrap())
        .read_line(&mut line)
        .await
        .unwrap();
    let f: serde_json::Value = serde_json::from_str(&line).unwrap();
    let clock = ManualClock::new();
    let entropy = Arc::new(SimEntropy::seeded(1));
    let meta = SimMetadataStore::new(clock.clone(), entropy.clone());
    let manager = OAuthManager::new(
        meta,
        Arc::new(EnvVarKeyProvider::from_bytes([7; 32], "test")),
        clock,
        entropy,
        Arc::new(StaticSecretStore::new(HashMap::new())),
    );
    let (_, shutdown) = tokio::sync::watch::channel(false);
    assert!(
        engram_coordinator::mcp::startup::start(None, manager.clone(), shutdown.clone())
            .await
            .unwrap()
            .is_none()
    );
    assert!(engram_coordinator::mcp::startup::start(
        Some(std::path::Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../deploy/mcp-control.example.json"
        ))),
        manager.clone(),
        shutdown
    )
    .await
    .unwrap()
    .is_none());
    let authority = Arc::new(
        McpAuthority::new(
            manager,
            Arc::new(SystemSecurityEntropy),
            Registry {
                enabled: true,
                deployment_id: "deployment-a".into(),
                ..Default::default()
            },
            BTreeMap::new(),
        )
        .unwrap(),
    );
    for pinned in [true, false] {
        let tls = ControlTls {
            certificate: f["cert"].as_str().unwrap().as_bytes().to_vec(),
            private_key: f["key"].as_str().unwrap().as_bytes().to_vec(),
            client_ca: f["ca"].as_str().unwrap().as_bytes().to_vec(),
            orchestrator_fingerprints: vec![if pinned {
                f["fingerprint"].as_str().unwrap().into()
            } else {
                "00:".repeat(31) + "00"
            }],
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (stop, done) = tokio::sync::oneshot::channel::<()>();
        let task = tokio::spawn(
            server(authority.clone(), tls)
                .unwrap()
                .serve_with_incoming_shutdown(
                    tonic::transport::server::TcpIncoming::from(listener),
                    async {
                        let _ = done.await;
                    },
                ),
        );
        let endpoint = tonic::transport::Endpoint::from_shared(format!(
            "https://localhost:{}",
            address.port()
        ))
        .unwrap();
        let tls = tonic::transport::ClientTlsConfig::new()
            .ca_certificate(tonic::transport::Certificate::from_pem(
                f["ca"].as_str().unwrap(),
            ))
            .domain_name("localhost");
        let no_cert = endpoint
            .clone()
            .tls_config(tls.clone())
            .unwrap()
            .connect()
            .await;
        if let Ok(channel) = no_cert {
            assert!(
                wire::credential_control_client::CredentialControlClient::new(channel)
                    .get_status(wire::GetStatusRequest::default())
                    .await
                    .is_err()
            );
        }
        let channel = endpoint
            .tls_config(tls.identity(tonic::transport::Identity::from_pem(
                f["cert"].as_str().unwrap(),
                f["key"].as_str().unwrap(),
            )))
            .unwrap()
            .connect()
            .await
            .unwrap();
        let mut client = wire::credential_control_client::CredentialControlClient::new(channel);
        if pinned {
            let missing = client
                .get_status(wire::GetStatusRequest {
                    protocol: 1,
                    connection_id: "connection-a".into(),
                    reference: Some(wire::CredentialReference {
                        kind: wire::CredentialKind::Oauth as i32,
                        registration_id: "absent".into(),
                        ..Default::default()
                    }),
                })
                .await
                .unwrap_err();
            assert_eq!(missing.code(), tonic::Code::NotFound);
        }
        let error = client
            .get_status(wire::GetStatusRequest {
                protocol: 2,
                ..Default::default()
            })
            .await
            .unwrap_err();
        assert_eq!(
            error.code(),
            if pinned {
                tonic::Code::InvalidArgument
            } else {
                tonic::Code::PermissionDenied
            }
        );
        let plaintext =
            tonic::transport::Endpoint::from_shared(format!("http://localhost:{}", address.port()))
                .unwrap()
                .connect_timeout(std::time::Duration::from_secs(1))
                .connect()
                .await;
        if let Ok(channel) = plaintext {
            assert!(
                wire::credential_control_client::CredentialControlClient::new(channel)
                    .get_status(wire::GetStatusRequest::default())
                    .await
                    .is_err()
            );
        }
        let _ = stop.send(());
        task.await.unwrap().unwrap();
    }
    assert!(!Registry::default().enabled);
    assert!(!format!(
        "{:?}",
        wire::PutSecretRequest {
            value: "private-value".into(),
            ..Default::default()
        }
    )
    .contains("private-value"));
}
