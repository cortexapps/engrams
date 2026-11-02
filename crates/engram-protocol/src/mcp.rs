//! Internal MCP credential control protocol 1; no cleartext app registration.
include!(concat!(env!("OUT_DIR"), "/mcp/engram.mcp.v1.rs"));
pub const PROTOCOL: u32 = 1;
macro_rules! redacted_debug {
    ($($ty:ty),*) => {$ (impl std::fmt::Debug for $ty {
        fn fmt(&self,f:&mut std::fmt::Formatter<'_>)->std::fmt::Result {f.write_str("McpControlMessage([redacted])")}
    })*};
}
redacted_debug!(
    PutSecretRequest,
    ReserveManagedCreateRequest,
    CreatePolicySessionRequest,
    CompleteAuthorizationRequest,
    BeginAuthorizationResponse,
    LeaseResponse
);

/// SHA-256 of exact creation wire bytes, task context, and native descriptor
/// version/digest. Length prefixes prevent ambiguous concatenation. Callers
/// retain the original wire bytes; they must not re-encode maps for a retry.
pub fn managed_create_input_digest(request: &ReserveManagedCreateRequest) -> String {
    use sha2::{Digest, Sha256};
    let mut hash = Sha256::new();
    hash.update(b"engram-managed-create-v1\0");
    hash.update(request.protocol.to_be_bytes());
    hash.update(request.native_version.to_be_bytes());
    for bytes in [
        request.task_id.as_bytes(),
        request.native_digest.as_bytes(),
        request.creation_request.as_slice(),
    ] {
        hash.update((bytes.len() as u64).to_be_bytes());
        hash.update(bytes);
    }
    format!("sha256:{}", hex::encode(hash.finalize()))
}

impl ReserveManagedCreateRequest {
    pub fn decode_creation(&self) -> Result<CreatePolicySessionRequest, &'static str> {
        use prost::Message;
        if self.creation_request.len() > 3 * 1024 * 1024 {
            return Err("creation request exceeds limit");
        }
        CreatePolicySessionRequest::decode(self.creation_request.as_slice())
            .map_err(|_| "invalid creation request")
    }
}

#[cfg(test)]
mod managed_creation_tests {
    use super::*;
    use prost::Message;
    #[test]
    fn exact_input_digest_and_wire_contract() {
        let request = ReserveManagedCreateRequest {
            protocol: 1,
            creation_request: vec![1, 2, 3],
            task_id: "task-a".into(),
            native_version: 1,
            native_digest: format!("sha256:{}", "a".repeat(64)),
            input_digest: String::new(),
        };
        assert_eq!(
            managed_create_input_digest(&request),
            "sha256:fa03d5903a5d81b1127d47cc331d0d833eb68f5fe42012397d86958bf5b995b5"
        );
        assert_eq!(
            ReserveManagedCreateRequest::decode(request.encode_to_vec().as_slice()).unwrap(),
            request
        );
        assert!(request.decode_creation().is_err());
        let response = ReadNativeManifestResponse {
            protocol: 1,
            identity: Some(ManagedCreateIdentity {
                session_id: "s-a".into(),
                task_id: "task-a".into(),
                input_id: "create-a".into(),
                actor_id: "owner".into(),
                parent_session_id: Some("parent".into()),
                write_eligibility: "read_only".into(),
                input_digest: "digest".into(),
            }),
            native_version: 1,
            native_bytes: b" [ ] ".to_vec(),
            native_digest: "digest".into(),
        };
        assert_eq!(
            ReadNativeManifestResponse::decode(response.encode_to_vec().as_slice()).unwrap(),
            response
        );
    }
}
