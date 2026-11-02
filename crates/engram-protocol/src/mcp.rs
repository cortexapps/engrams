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
    CreatePolicySessionRequest,
    CompleteAuthorizationRequest,
    BeginAuthorizationResponse,
    LeaseResponse
);
