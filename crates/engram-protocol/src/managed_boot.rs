//! Bounded exact managed commands. Decoding cannot grant host authority.
use engram_core::SandboxError;
use serde::{de::DeserializeOwned, Serialize};
const MAX_BYTES: usize = 8 * 1024 * 1024;
pub fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>, SandboxError> {
    let bytes = serde_json::to_vec(value).map_err(|_| invalid())?;
    if bytes.len() > MAX_BYTES {
        return Err(invalid());
    }
    Ok(bytes)
}
pub fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, SandboxError> {
    if bytes.len() > MAX_BYTES {
        return Err(invalid());
    }
    serde_json::from_slice(bytes).map_err(|_| invalid())
}
fn invalid() -> SandboxError {
    SandboxError::InvalidSpec("invalid managed boot command".into())
}
#[cfg(test)]
mod tests {
    use super::*;
    use engram_core::types::managed_boot::ManagedBootHostState;
    #[test]
    fn managed_boot_decode_is_bounded_and_rejects_unknown_state() {
        assert!(decode::<ManagedBootHostState>(br#"{"state":"warm"}"#).is_err());
        assert!(decode::<ManagedBootHostState>(&vec![b' '; MAX_BYTES + 1]).is_err());
        assert!(decode::<ManagedBootHostState>(b"null").is_err());
    }
    proptest::proptest! {
        #[test]
        fn managed_boot_decode_never_panics(bytes in proptest::collection::vec(proptest::num::u8::ANY,0..8192)) {
            let _=decode::<ManagedBootHostState>(&bytes);
        }
    }
}
