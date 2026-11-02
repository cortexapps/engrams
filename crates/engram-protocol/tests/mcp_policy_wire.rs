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
        let _=mcp::ReadWritePolicyRequest::decode(bytes.as_slice());
        let _=mcp::AcceptPlanExitRequest::decode(bytes.as_slice());
        let _=mcp::ValidateDispatchFenceRequest::decode(bytes.as_slice());
        let _=app::CompleteToolCallRequest::decode(bytes.as_slice());
        let _=app::McpPlanReview::decode(bytes.as_slice());
        let _=mcp::PutSecretResponse::decode(bytes.as_slice());
        let _=mcp::CompleteAuthorizationResponse::decode(bytes.as_slice());
        let _=mcp::GetStatusResponse::decode(bytes.as_slice());
        let _=mcp::RevokeResponse::decode(bytes.as_slice());
    }
}
