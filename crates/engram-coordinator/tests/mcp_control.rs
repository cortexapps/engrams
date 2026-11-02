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
                wire::credential_control_service_client::CredentialControlServiceClient::new(
                    channel
                )
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
        assert_eq!(
            wire::operator_publication_service_client::OperatorPublicationServiceClient::new(
                channel.clone()
            )
            .stage(wire::OperatorPublicationServiceStageRequest {
                protocol: 1,
                expected_generation: None,
                configured_candidate_identity: String::new()
            })
            .await
            .unwrap_err()
            .code(),
            tonic::Code::Unimplemented
        );
        let mut policy_client =
            wire::session_policy_service_client::SessionPolicyServiceClient::new(channel.clone());
        let sid = meta
            .create_session(engram_core::types::SessionSpec {
                image: "test.invalid/policy:latest".into(),
                mode: Default::default(),
            })
            .await
            .unwrap();
        let policy = policy_client
            .read_write_policy(wire::ReadWritePolicyRequest {
                protocol: 1,
                session_id: sid.to_string(),
                plan_tool_call_id: None,
            })
            .await;
        if pinned {
            assert_eq!(
                policy
                    .unwrap()
                    .into_inner()
                    .state
                    .unwrap()
                    .policy
                    .unwrap()
                    .write_eligibility,
                "unknown"
            );
            assert_eq!(
                policy_client
                    .read_write_policy(wire::ReadWritePolicyRequest {
                        protocol: 0,
                        session_id: sid.to_string(),
                        plan_tool_call_id: None
                    })
                    .await
                    .unwrap_err()
                    .code(),
                tonic::Code::InvalidArgument
            );
        } else {
            assert_eq!(policy.unwrap_err().code(), tonic::Code::PermissionDenied);
        }
        // Exact native input is available only to the pinned policy service.
        // This fixture exercises persistence and transport, never backend execution.
        use engram_core::types::{managed_create::*, mcp_policy::*, runtime_spec::RuntimeSpec};
        use prost::Message;
        let native_sid =
            engram_core::SessionId(uuid::Uuid::from_u128(if pinned { 1400 } else { 1401 }));
        let policy = McpPolicyCreation {
            actor_id: "owner-native".into(),
            input_id: format!("create:{native_sid}"),
            parent_session_id: None,
            eligibility: WriteEligibility::ReadOnly,
        };
        let mut create = engram_protocol::app::CreateSessionRequest {
            requested_session_id: Some(native_sid.to_string()),
            harness: Some("claude".into()),
            harness_mode: Some("plan".into()),
            ..Default::default()
        };
        create
            .harness_env
            .insert("ENGRAM_TOOLS".into(), " [ ] ".into());
        let mut reserve = wire::ReserveManagedCreateRequest {
            protocol: 1,
            task_id: "task-native".into(),
            native_version: 1,
            native_digest: digest(b" [ ] "),
            creation_request: wire::CreatePolicySessionRequest {
                protocol: 1,
                actor_id: policy.actor_id.clone(),
                input_id: policy.input_id.clone(),
                create: Some(create),
                parent_session_id: None,
            }
            .encode_to_vec(),
            input_digest: String::new(),
        };
        reserve.input_digest = wire::managed_create_input_digest(&reserve);
        assert_eq!(
            policy_client
                .reserve_managed_create(reserve.clone())
                .await
                .unwrap_err()
                .code(),
            if pinned {
                tonic::Code::Unavailable
            } else {
                tonic::Code::PermissionDenied
            }
        );
        assert!(meta.get_session(native_sid).await.is_err());
        let identity = ManagedCreateIdentity {
            session_id: native_sid,
            task_id: reserve.task_id.clone(),
            policy: policy.clone(),
            input_digest: reserve.input_digest.clone(),
        };
        assert_eq!(
            policy_client
                .read_published_operator(wire::ReadPublishedOperatorRequest { protocol: 1 })
                .await
                .unwrap_err()
                .code(),
            if pinned {
                tonic::Code::Unavailable
            } else {
                tonic::Code::PermissionDenied
            }
        );
        engram_testkit::operator::publish_empty(meta.as_ref()).await;
        let projection = policy_client
            .read_published_operator(wire::ReadPublishedOperatorRequest { protocol: 1 })
            .await;
        if pinned {
            let projection: engram_core::types::managed_operator::OperatorPublication =
                serde_json::from_slice(&projection.unwrap().into_inner().publication_json).unwrap();
            assert!(projection.admission_open);
            assert_eq!(
                projection.published.unwrap().candidate,
                engram_testkit::operator::candidate()
            );
        } else {
            assert_eq!(
                projection.unwrap_err().code(),
                tonic::Code::PermissionDenied
            );
        }

        meta.reserve_and_persist_create(
            engram_core::traits::SessionCreateWriteSet {
                managed_initial_prompt: None,
                managed_creation: Some(ManagedCreation {
                    identity: identity.clone(),
                    native: FrozenNativeManifest {
                        version: 1,
                        bytes: b" [ ] ".to_vec(),
                        digest: reserve.native_digest.clone(),
                    },
                }),
                mcp_policy: Some(policy.clone()),
                nbd_slot_need: 1,
                session_id: native_sid,
                spec: engram_core::types::SessionSpec {
                    image: "test.invalid/native:latest".into(),
                    mode: Default::default(),
                },
                mem_budget_mib: 512,
                cpu_budget_vcpus: 1,
                sealed_secrets: None,
                capabilities: Vec::new(),
                integration_policy_json: None,
                runtime_spec: RuntimeSpec::new(Vec::new(), Some("claude".into()), None, Vec::new()),
                oauth_binding: None,
            },
            &[],
            0,
        )
        .await
        .unwrap();
        let native_identity = wire::ManagedCreateIdentity {
            session_id: native_sid.to_string(),
            task_id: identity.task_id,
            input_id: policy.input_id,
            actor_id: policy.actor_id,
            parent_session_id: None,
            write_eligibility: "read_only".into(),
            input_digest: identity.input_digest,
        };
        let boot = policy_client
            .read_managed_boot(wire::ReadManagedBootRequest {
                protocol: 1,
                identity: Some(native_identity.clone()),
            })
            .await;
        if pinned {
            assert!(boot.unwrap().into_inner().boot_json.is_none());
        } else {
            assert_eq!(boot.unwrap_err().code(), tonic::Code::PermissionDenied);
        }
        for protocol in [0, 1] {
            let code = policy_client
                .finalize_managed_boot(wire::FinalizeManagedBootRequest {
                    protocol,
                    input_json: b"{}".to_vec(),
                })
                .await
                .unwrap_err()
                .code();
            assert_eq!(
                code,
                if pinned {
                    tonic::Code::InvalidArgument
                } else {
                    tonic::Code::PermissionDenied
                }
            );
        }
        // A valid one-MiB manifest expands when encoded as JSON bytes. The
        // service envelope must reach the bounded typed decoder above four MiB.
        let large = policy_client
            .finalize_managed_boot(wire::FinalizeManagedBootRequest {
                protocol: 1,
                input_json: vec![b' '; 5 * 1024 * 1024],
            })
            .await
            .unwrap_err()
            .code();
        assert_eq!(
            large,
            if pinned {
                tonic::Code::InvalidArgument
            } else {
                tonic::Code::PermissionDenied
            }
        );
        let request = wire::ReadNativeManifestRequest {
            protocol: 1,
            identity: Some(native_identity.clone()),
        };
        let result = policy_client.read_native_manifest(request.clone()).await;
        if pinned {
            let native = result.unwrap().into_inner();
            assert_eq!(native.native_bytes, b" [ ] ");
            assert_eq!(native.native_digest, reserve.native_digest);
            assert_eq!(native.identity, Some(native_identity));
            assert_eq!(native.native_version, 1);
            let mut wrong = request.clone();
            wrong.identity.as_mut().unwrap().task_id = "another-task".into();
            assert_eq!(
                policy_client
                    .read_native_manifest(wrong)
                    .await
                    .unwrap_err()
                    .code(),
                tonic::Code::FailedPrecondition
            );
            let mut wrong = request.clone();
            wrong.identity.as_mut().unwrap().session_id = sid.to_string();
            assert_eq!(
                policy_client
                    .read_native_manifest(wrong)
                    .await
                    .unwrap_err()
                    .code(),
                tonic::Code::FailedPrecondition
            );
            let mut wrong = request;
            wrong.protocol = 0;
            assert_eq!(
                policy_client
                    .read_native_manifest(wrong)
                    .await
                    .unwrap_err()
                    .code(),
                tonic::Code::InvalidArgument
            );
        } else {
            assert_eq!(result.unwrap_err().code(), tonic::Code::PermissionDenied);
        }
        let mut client =
            wire::credential_control_service_client::CredentialControlServiceClient::new(channel);
        if pinned {
            use engram_core::types::mcp_policy::{McpPolicyCreation, WriteEligibility};
            let policy_sid = engram_core::SessionId(uuid::Uuid::from_u128(1200));
            meta.reserve_and_persist_create(
                engram_core::traits::SessionCreateWriteSet {
                    managed_initial_prompt: None,
                    managed_creation: None,
                    mcp_policy: Some(McpPolicyCreation {
                        actor_id: "owner-a".into(),
                        input_id: "create-a".into(),
                        parent_session_id: None,
                        eligibility: WriteEligibility::ReadOnly,
                    }),
                    nbd_slot_need: 1,
                    session_id: policy_sid,
                    spec: engram_core::types::SessionSpec {
                        image: "test.invalid/policy:latest".into(),
                        mode: Default::default(),
                    },
                    mem_budget_mib: 512,
                    cpu_budget_vcpus: 1,
                    sealed_secrets: None,
                    capabilities: Vec::new(),
                    integration_policy_json: None,
                    runtime_spec: engram_core::types::runtime_spec::RuntimeSpec::new(
                        Vec::new(),
                        Some("claude".into()),
                        None,
                        Vec::new(),
                    ),
                    oauth_binding: None,
                },
                &[],
                0,
            )
            .await
            .unwrap();
            meta.append_session_event(policy_sid,"tool_call_requested",serde_json::json!({"name":"exit_plan_mode","tool_call_id":"plan-a","run_id":"run-a","args_json":"{}"})).await.unwrap();
            let reviewed = policy_client
                .read_write_policy(wire::ReadWritePolicyRequest {
                    protocol: 1,
                    session_id: policy_sid.to_string(),
                    plan_tool_call_id: Some("plan-a".into()),
                })
                .await
                .unwrap()
                .into_inner()
                .state
                .unwrap()
                .plan_review
                .unwrap();
            let approved = policy_client
                .accept_plan_exit(wire::AcceptPlanExitRequest {
                    protocol: 1,
                    expected: Some(reviewed.clone()),
                    actor_id: "owner-a".into(),
                    result_json: "{\"decision\":\"approve\"}".into(),
                })
                .await
                .unwrap()
                .into_inner()
                .policy
                .unwrap();
            assert_eq!(approved.write_eligibility, "write_eligible");
            assert_eq!(approved.authenticated_actor_id.as_deref(), Some("owner-a"));
            let outbox = meta
                .outbox_get(&engram_core::types::outbox::tool_result_outbox_id(
                    policy_sid, "plan-a",
                ))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(outbox.payload["result_json"], "{\"decision\":\"approve\"}");
            for state in [
                engram_core::SessionState::Pending,
                engram_core::SessionState::Created,
                engram_core::SessionState::Active,
            ] {
                meta.transition_session(policy_sid, state, engram_core::BindingDisposition::Retain)
                    .await
                    .unwrap();
            }
            meta.append_session_event(
                policy_sid,
                "run_started",
                serde_json::json!({"run_id":"run-a"}),
            )
            .await
            .unwrap();
            meta.append_session_event(policy_sid, "tool_call_requested", serde_json::json!({"name":"mcp_docs_search", "tool_call_id":"read-a", "run_id":"run-a", "args_json":"{ \"x\": 1 }"})).await.unwrap();
            let exact = wire::ValidateExecutionIdentityRequest {
                protocol: 1,
                session_id: policy_sid.to_string(),
                run_id: "run-a".into(),
                tool_call_id: "read-a".into(),
                accepted_policy_ref: approved.r#ref.clone(),
                accepted_write_eligibility: "write_eligible".into(),
                ancestor_policy_refs: Vec::new(),
                model_tool_name: "mcp_docs_search".into(),
                original_arguments: "{ \"x\": 1 }".into(),
            };
            assert_eq!(
                policy_client
                    .validate_execution_identity(exact.clone())
                    .await
                    .unwrap()
                    .into_inner()
                    .status,
                "current"
            );
            assert_eq!(
                policy_client
                    .validate_execution_identity(wire::ValidateExecutionIdentityRequest {
                        original_arguments: "{\"x\":1}".into(),
                        ..exact.clone()
                    })
                    .await
                    .unwrap()
                    .into_inner()
                    .status,
                "stale"
            );
            assert_eq!(
                policy_client
                    .validate_dispatch_fence(wire::ValidateDispatchFenceRequest {
                        protocol: exact.protocol,
                        session_id: exact.session_id,
                        run_id: exact.run_id,
                        tool_call_id: exact.tool_call_id,
                        accepted_policy_ref: exact.accepted_policy_ref,
                        accepted_write_eligibility: exact.accepted_write_eligibility,
                        ancestor_policy_refs: exact.ancestor_policy_refs,
                        model_tool_name: "wrong-name".into(),
                        original_arguments: exact.original_arguments,
                    })
                    .await
                    .unwrap()
                    .into_inner()
                    .status,
                "stale"
            );
            let prior = policy_client
                .validate_dispatch_fence(wire::ValidateDispatchFenceRequest {
                    protocol: 1,
                    session_id: policy_sid.to_string(),
                    run_id: "run-a".into(),
                    tool_call_id: "call-a".into(),
                    accepted_policy_ref: reviewed.policy_ref,
                    accepted_write_eligibility: "read_only".into(),
                    ancestor_policy_refs: Vec::new(),
                    model_tool_name: "exit_plan_mode".into(),
                    original_arguments: "{}".into(),
                })
                .await
                .unwrap()
                .into_inner();
            assert_ne!(prior.status, "current");
            assert_eq!(
                policy_client
                    .create_policy_session(wire::CreatePolicySessionRequest {
                        protocol: 1,
                        ..Default::default()
                    })
                    .await
                    .unwrap_err()
                    .code(),
                tonic::Code::InvalidArgument
            );
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
                wire::credential_control_service_client::CredentialControlServiceClient::new(
                    channel
                )
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
