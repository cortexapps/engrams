use engram_coordinator::{
    mcp::{control::*, model::Registry, *},
    oauth::OAuthManager,
};
use engram_core::{
    traits::{Clock, MetadataStore, StaticSecretStore},
    types::{mcp::McpSealedState, oauth::*},
};
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
        meta.clone(),
        Arc::new(EnvVarKeyProvider::from_bytes([7; 32], "test")),
        clock.clone(),
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
            let id = uuid::Uuid::from_u128(999);
            let now = clock.now_utc();
            meta.create_mcp_oauth_flow(
                OAuthFlow {
                    id,
                    key: OAuthCredentialKey {
                        subject_kind: OAuthSubjectKind::Mcp,
                        subject_id: "connection-a".into(),
                        provider: "registered-a".into(),
                    },
                    owner_replica: "replica-a".into(),
                    lease_expires_at: now + chrono::Duration::minutes(10),
                    expires_at: now + chrono::Duration::minutes(10),
                    status: OAuthFlowStatus::Pending,
                    error_code: None,
                    created_at: now,
                    updated_at: now,
                },
                McpSealedState {
                    wrapped_dek: vec![],
                    nonce: vec![],
                    ciphertext: vec![],
                    key_id: "test".into(),
                },
            )
            .await
            .unwrap();
            let cancel = wire::CancelAuthorizationRequest {
                protocol: 1,
                flow_id: id.to_string(),
                connection_id: "connection-a".into(),
                registration_id: "registered-a".into(),
                actor_id: "admin".into(),
            };
            for _ in 0..2 {
                let receipt = client
                    .cancel_authorization(cancel.clone())
                    .await
                    .unwrap()
                    .into_inner();
                assert_eq!(receipt.flow_id, cancel.flow_id);
                assert_eq!(receipt.connection_id, cancel.connection_id);
                assert_eq!(receipt.registration_id, cancel.registration_id);
                assert_eq!(receipt.protocol, 1);
            }
            assert_eq!(
                meta.get_oauth_flow(id).await.unwrap().unwrap().status,
                OAuthFlowStatus::Cancelled
            );
            let mut wrong = cancel;
            wrong.connection_id = "unrelated".into();
            assert!(client.cancel_authorization(wrong).await.is_err());
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
