use engram_coordinator::mcp::worker::{canonical, digest};
use serde_json::json;

#[test]
fn fixed_worker_wire_matches_node_utf16_and_integer_encoding() {
    let value = json!({"\u{e000}":"last", "\u{10000}":"first", "line":"\n\u{000f}","n":9007199254740991i64});
    assert_eq!(
        canonical(&value).unwrap(),
        "{\"line\":\"\\n\\u000f\",\"n\":9007199254740991,\"𐀀\":\"first\",\"\":\"last\"}"
    );
    assert_eq!(
        digest(&json!({"kind":"metadata","endpoint":"https://auth.example/metadata"})).unwrap(),
        "e776800d6d90b7a995cb193fbc359e5b248ed21f8c78519749d290599cef2b7d"
    );
    assert!(canonical(&json!(9007199254740992i64)).is_err());
    assert!(canonical(&json!(1.1)).is_err());
}

#[tokio::test]
async fn actual_worker_mtls_identity_and_fixed_rpc() {
    use engram_coordinator::mcp::worker::*;
    use tokio::io::{AsyncBufReadExt, BufReader};
    let mut child = tokio::process::Command::new("node")
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
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut line)
        .await
        .unwrap();
    let fixture: serde_json::Value = serde_json::from_str(&line).unwrap();
    let url = format!("https://localhost:{}", fixture["port"]);
    let cert = fixture["cert"].as_str().unwrap();
    let pem = format!("{}{}", cert, fixture["key"].as_str().unwrap());
    let client = WorkerClient::new(
        &url,
        fixture["ca"].as_str().unwrap().as_bytes(),
        pem.as_bytes(),
    )
    .unwrap();
    let identity = client.identity().await.unwrap();
    assert_eq!(identity.zone_id, "public");
    let no_cert = engram_tls::client_builder()
        .tls_certs_only([reqwest::Certificate::from_pem(
            fixture["ca"].as_str().unwrap().as_bytes(),
        )
        .unwrap()])
        .no_proxy()
        .build()
        .unwrap();
    assert!(no_cert.get(format!("{url}/identity")).send().await.is_err());
    assert!(WorkerClient::new("http://localhost", cert.as_bytes(), pem.as_bytes()).is_err());
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let operation = WorkerOperation::ValidateDestination {
        endpoint: "https://auth.example/metadata".into(),
    };
    let intent = WorkerIntent {
        id: "intent-a".into(),
        boot_id: identity.boot_id.clone(),
        zone_id: identity.zone_id.clone(),
        policy_digest: identity.policy_digest.clone(),
        connection_id: "connection-a".into(),
        connection_revision: 1,
        credential_identity: "authority-a".into(),
        scope: WorkerScope::Administration {
            request_id: "request-a".into(),
            actor_id: "actor-a".into(),
        },
        operation: "validateDestination".into(),
        destination: "https://auth.example/metadata".into(),
        request_digest: "a".repeat(64),
        operation_digest: digest(&serde_json::to_value(&operation).unwrap()).unwrap(),
        expires_at: now + 10000,
        lease: WorkerLease {
            id: "lease-a".into(),
            deployment_id: "deployment-a".into(),
            expires_at: now + 10000,
        },
    };
    client.rpc(&identity, &intent, &operation).await.unwrap();
    assert!(client.rpc(&identity, &intent, &operation).await.is_err());
    let mut wrong = identity.clone();
    wrong.boot_id = "wrong".into();
    assert!(client.check_identity(&wrong).await.is_err());
}

mod authority {
    use engram_coordinator::{
        mcp::{model::*, worker::*, *},
        oauth::OAuthManager,
    };
    use engram_core::{
        traits::{Clock, StaticSecretStore},
        types::org_secret::McpSecretRole,
    };
    use engram_crypto::EnvVarKeyProvider;
    use engram_sim::{ManualClock, SimEntropy, SimMetadataStore};
    use std::{
        collections::{BTreeMap, BTreeSet, HashMap},
        sync::{
            atomic::{AtomicU8, Ordering},
            Arc, Mutex,
        },
    };
    struct DeterministicSecurity(AtomicU8);
    impl SecurityEntropy for DeterministicSecurity {
        fn bytes(&self) -> Result<[u8; 32]> {
            Ok([self.0.fetch_add(1, Ordering::SeqCst); 32])
        }
    }
    struct FakeWorker(Mutex<WorkerIdentity>);
    #[async_trait::async_trait]
    impl WorkerTransport for FakeWorker {
        async fn identity(&self) -> Result<WorkerIdentity> {
            Ok(self.0.lock().unwrap().clone())
        }
        async fn rpc(
            &self,
            _: &WorkerIdentity,
            _: &WorkerIntent,
            _: &WorkerOperation,
        ) -> Result<serde_json::Value> {
            Err(McpError::Unavailable)
        }
    }
    fn setup() -> (
        McpAuthority,
        Arc<ManualClock>,
        WorkerIdentity,
        Arc<FakeWorker>,
    ) {
        let clock = ManualClock::new();
        let entropy = Arc::new(SimEntropy::seeded(9));
        let meta = SimMetadataStore::new(clock.clone(), entropy.clone());
        let manager = OAuthManager::new(
            meta,
            Arc::new(EnvVarKeyProvider::from_bytes([8; 32], "test")),
            clock.clone(),
            entropy,
            Arc::new(StaticSecretStore::new(HashMap::new())),
        );
        let identity = WorkerIdentity {
            protocol: 1,
            boot_id: "boot-a".into(),
            zone_id: "public".into(),
            policy_digest: "a".repeat(64),
            policy_revision: "1".into(),
        };
        let registry = Registry {
            enabled: true,
            deployment_id: "deployment-a".into(),
            zones: BTreeMap::from([(
                "public".into(),
                Zone {
                    policy_digest: identity.policy_digest.clone(),
                    policy_revision: "1".into(),
                    endpoints: BTreeSet::from(["https://resource.example/mcp".into()]),
                },
            )]),
            registrations: BTreeMap::new(),
        };
        let worker = Arc::new(FakeWorker(Mutex::new(identity.clone())));
        (
            McpAuthority::new(
                manager,
                Arc::new(DeterministicSecurity(AtomicU8::new(1))),
                registry,
                BTreeMap::from([("public".into(), worker.clone() as Arc<dyn WorkerTransport>)]),
            )
            .unwrap(),
            clock,
            identity,
            worker,
        )
    }
    #[tokio::test]
    async fn api_rotation_fences_old_lease_even_before_orchestrator_publication() {
        let (authority, clock, identity, worker) = setup();
        let change = ChangeContext {
            connection_id: "connection-a".into(),
            config_revision: 1,
            previous_authorization_generation: 0,
            resource: Destination {
                zone_id: "public".into(),
                endpoint: "https://resource.example/mcp".into(),
            },
        };
        let first = authority
            .put_secret(
                &change,
                McpSecretRole::ApiKey,
                None,
                "x-api-key",
                "first-secret",
            )
            .await
            .unwrap();
        let lease = ResourceLease {
            context: first.context.clone(),
            reference: first.reference.clone(),
            worker: identity,
            intent_id: "intent-a".into(),
            request_digest: "a".repeat(64),
            operation: "discover".into(),
            scope: WorkerScope::Administration {
                request_id: "request-a".into(),
                actor_id: "actor-a".into(),
            },
            expires_at: clock.now_utc().timestamp_millis() + 10000,
            admission: WorkerLease {
                id: "lease-a".into(),
                deployment_id: "deployment-a".into(),
                expires_at: clock.now_utc().timestamp_millis() + 10000,
            },
        };
        assert_eq!(authority.lease(&lease).await.unwrap().value, "first-secret");
        let mut next = change;
        next.previous_authorization_generation = first.context.authorization_generation;
        let second = authority
            .put_secret(
                &next,
                McpSecretRole::ApiKey,
                Some(&first.reference),
                "x-api-key",
                "second-secret",
            )
            .await
            .unwrap();
        assert_ne!(
            first.context.credential_identity,
            second.context.credential_identity
        );
        assert!(authority.lease(&lease).await.is_err());
        let mut fresh = lease;
        fresh.context = second.context;
        fresh.reference = second.reference;
        assert_eq!(
            authority.lease(&fresh).await.unwrap().value,
            "second-secret"
        );
        let mut wrong = fresh.clone();
        wrong.context.config_revision += 1;
        assert!(authority.lease(&wrong).await.is_err());
        wrong = fresh.clone();
        wrong.worker.boot_id = "old-boot".into();
        assert!(authority.lease(&wrong).await.is_err());
        // A new boot fences old intents, but preserves durable credential authority.
        worker.0.lock().unwrap().boot_id = "boot-b".into();
        assert!(authority.lease(&fresh).await.is_err());
        fresh.worker.boot_id = "boot-b".into();
        assert_eq!(
            authority.lease(&fresh).await.unwrap().value,
            "second-secret"
        );
        let current = authority
            .status("connection-a", &fresh.reference)
            .await
            .unwrap();
        assert!(authority.revoke(&first).await.is_err());
        let revoked = authority.revoke(&current).await.unwrap();
        assert_eq!(revoked.status, "revoked");
        assert!(authority.lease(&fresh).await.is_err());
        clock.advance(std::time::Duration::from_secs(10));
        assert!(authority.lease(&fresh).await.is_err());
    }
}
