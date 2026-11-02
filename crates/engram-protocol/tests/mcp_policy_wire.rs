use engram_protocol::{app, mcp};
use proptest::{prelude::*, test_runner::FileFailurePersistence};
use prost::Message;

#[test]
fn app_reflection_excludes_internal_policy_and_credential_services() {
    let bytes = app::FILE_DESCRIPTOR_SET;
    for private in [
        "engram/mcp/v1/session_policy.proto",
        "SessionPolicyService",
        "CredentialControlService",
        "OperatorPublicationService",
        "engram/mcp/v1/operator.proto",
    ] {
        assert!(!bytes
            .windows(private.len())
            .any(|w| w == private.as_bytes()));
    }
}
proptest! {
    #![proptest_config(ProptestConfig { cases: 64, failure_persistence: Some(Box::new(FileFailurePersistence::Direct("proptest-regressions/mcp_policy_wire.txt"))), ..ProptestConfig::default() })]
    #[test]
    fn policy_decode_never_panics(bytes in proptest::collection::vec(any::<u8>(),0..=4096)) {
        let _=mcp::CreatePolicySessionRequest::decode(bytes.as_slice());
        let _=mcp::ReadManagedPolicyResponse::decode(bytes.as_slice());
        let _=mcp::ManagedRetainedDesired::decode(bytes.as_slice());
        let _=mcp::ReadWritePolicyRequest::decode(bytes.as_slice());
        let _=mcp::AcceptPlanExitRequest::decode(bytes.as_slice());
        let _=mcp::ValidateDispatchFenceRequest::decode(bytes.as_slice());
        let _=app::CompleteToolCallRequest::decode(bytes.as_slice());
        let _=app::McpPlanReview::decode(bytes.as_slice());
        let _=mcp::PutSecretResponse::decode(bytes.as_slice());
        let _=mcp::CompleteAuthorizationResponse::decode(bytes.as_slice());
        let _=mcp::GetStatusResponse::decode(bytes.as_slice());
        let _=mcp::RevokeResponse::decode(bytes.as_slice());
        let _=mcp::ReadManagedBootRequest::decode(bytes.as_slice());
        let _=mcp::ReadManagedBootResponse::decode(bytes.as_slice());
        let _=mcp::FinalizeManagedBootRequest::decode(bytes.as_slice());
        let _=mcp::OperatorPublicationServiceStageRequest::decode(bytes.as_slice());
        let _=mcp::OperatorPublicationServiceReadRequest::decode(bytes.as_slice());
        let _=mcp::OperatorPublicationServiceReadResponse::decode(bytes.as_slice());
        let _=mcp::OperatorPublicationServiceStageResponse::decode(bytes.as_slice());
        let _=mcp::ReadPublishedOperatorRequest::decode(bytes.as_slice());
        let _=mcp::ReadPublishedOperatorResponse::decode(bytes.as_slice());
        let _=serde_json::from_slice::<engram_core::types::managed_operator::OperatorCandidate>(&bytes);
        let _=serde_json::from_slice::<engram_core::types::managed_operator::OperatorPublication>(&bytes);
    }
}

#[test]
fn managed_read_requires_explicit_canonical_retention_and_exact_target() {
    use engram_core::{types::managed_deny::*, HostId, SandboxId, SessionId};
    let target = ManagedPolicyTarget {
        session_id: SessionId::new(),
        sandbox_id: SandboxId::new(),
        host_id: HostId::new(),
        binding_epoch: 1,
    };
    let mut response = mcp::ReadManagedPolicyResponse {
        protocol: mcp::PROTOCOL,
        applied: None,
        retained_desired: None,
    };
    assert!(
        response.validate_for(&target).is_err(),
        "old server omission must fail closed"
    );
    response.retained_desired = Some(mcp::ManagedRetainedDesired {
        target: Some(target.clone().into()),
        policy: None,
    });
    response.validate_for(&target).unwrap(); // Explicit absence, with no fabricated ACK.
    let policy = ManagedDenyPolicy::new(3, ["orphan.example".into()]).unwrap();
    response.retained_desired.as_mut().unwrap().policy = Some(policy.clone().into());
    let roundtrip =
        mcp::ReadManagedPolicyResponse::decode(response.encode_to_vec().as_slice()).unwrap();
    roundtrip.validate_for(&target).unwrap();
    assert!(roundtrip.applied.is_none());
    let mut wrong = target.clone();
    wrong.binding_epoch += 1;
    assert!(response.validate_for(&wrong).is_err());
    response
        .retained_desired
        .as_mut()
        .unwrap()
        .policy
        .as_mut()
        .unwrap()
        .hosts
        .push("UPPER.EXAMPLE".into());
    assert!(
        response.validate_for(&target).is_err(),
        "noncanonical retained bytes rejected"
    );
    response.retained_desired.as_mut().unwrap().policy = Some(policy.clone().into());
    let context = ManagedPolicyContext {
        target: wrong,
        operator_generation: 1,
        operator_identity: format!("sha256:{}", "a".repeat(64)),
        binding_hash: None,
    };
    response.applied = Some(
        ManagedAppliedState::new(context, policy, "host".into(), "proxy".into(), None)
            .unwrap()
            .into(),
    );
    assert!(
        response.validate_for(&target).is_err(),
        "applied target must agree independently"
    );
}

#[test]
fn managed_boot_pinned_wire_tags_are_stable() {
    let request = mcp::FinalizeManagedBootRequest {
        protocol: 1,
        input_json: b"{}".to_vec(),
    };
    assert_eq!(request.encode_to_vec(), [8, 1, 18, 2, 123, 125]);
    let response = mcp::ReadManagedBootResponse {
        protocol: 1,
        boot_json: Some(b"{}".to_vec()),
    };
    assert_eq!(response.encode_to_vec(), [8, 1, 18, 2, 123, 125]);
    assert_eq!(
        mcp::ReadManagedBootResponse::decode([8u8, 1].as_slice())
            .unwrap()
            .boot_json,
        None
    );
}
#[test]
fn managed_capture_json_golden_roundtrip() {
    use engram_core::types::managed_boot::ManagedCaptureRequest;
    let bytes = include_bytes!("golden/managed_capture.json");
    let request: ManagedCaptureRequest = engram_protocol::managed_boot::decode(bytes).unwrap();
    request.key.validate().unwrap();
    assert_eq!(
        engram_protocol::managed_boot::encode(&request).unwrap(),
        bytes
    );
    assert_eq!(
        request.disk_manifest().manifest_id,
        request.capture_id.as_uuid()
    );
    assert_eq!(request.disk_manifest().version, 1);
}
