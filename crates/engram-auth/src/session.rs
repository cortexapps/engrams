//! Opaque web-session token: mint a random token for the cookie, store only
//! its SHA-256 hash. A DB read can't reconstruct a valid cookie, and
//! revocation is a row delete.

use base64::Engine;
use rand::rngs::OsRng;
use rand::RngCore;
use sha2::{Digest, Sha256};

/// Mint a fresh 256-bit opaque session token, URL-safe base64 (no padding).
/// Goes into the cookie verbatim; only [`hash_token`] of it is persisted.
pub fn mint_session_token() -> String {
    // OsRng (not thread_rng) so the value is `Send` across awaits, matching
    // engram-crypto's rationale.
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// SHA-256 of the cookie token — the `web_sessions.token_hash` primary key.
pub fn hash_token(token: &str) -> Vec<u8> {
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    hasher.finalize().to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minted_tokens_are_unique_and_hash_deterministically() {
        let a = mint_session_token();
        let b = mint_session_token();
        assert_ne!(a, b, "each token must be random");
        assert_eq!(hash_token(&a), hash_token(&a), "hash is deterministic");
        assert_ne!(hash_token(&a), hash_token(&b));
        assert_eq!(hash_token(&a).len(), 32, "sha-256 → 32 bytes");
    }
}
