//! Shared per-session broker-token auth for the in-guest capability
//! seams.
//!
//! ADR 0023 minted a per-session opaque broker token to authorize the
//! forge bridge (git credential + PR). ADR 0026 generalizes it: the
//! same token also authorizes the artifact-upload bridge, and the check
//! no longer requires a forge to be configured (uploads aren't
//! git-gated). The token still lives in `state.git_broker_tokens` (the
//! name is historical); think of it as the session's broker token.

use axum::http::HeaderMap;

use engram_core::types::ids::SessionId;

use crate::state::SharedState;

/// Verify the per-session broker token in constant time against the
/// in-memory map. Returns `true` iff the session has a token and it
/// matches. Does NOT require `state.forge` — the forge seam layers its
/// own forge-configured check on top of this.
pub(crate) fn authorize_broker_token(state: &SharedState, session: SessionId, token: &str) -> bool {
    match state.git_broker_tokens.get(&session) {
        Some(expected) => constant_time_eq(token.as_bytes(), expected.value().as_bytes()),
        None => false,
    }
}

/// Extract a `Bearer <token>` value from the `Authorization` header.
pub(crate) fn bearer(headers: &HeaderMap) -> Option<String> {
    let v = headers
        .get(axum::http::header::AUTHORIZATION)?
        .to_str()
        .ok()?;
    v.strip_prefix("Bearer ").map(str::to_string)
}

/// Length-independent byte comparison so a forged token can't be
/// recovered by timing the compare.
pub(crate) fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{header::AUTHORIZATION, HeaderValue};

    #[test]
    fn bearer_extracts_only_bearer_scheme() {
        let mut h = HeaderMap::new();
        h.insert(AUTHORIZATION, HeaderValue::from_static("Bearer tok123"));
        assert_eq!(bearer(&h).as_deref(), Some("tok123"));
        assert!(bearer(&HeaderMap::new()).is_none());
        let mut basic = HeaderMap::new();
        basic.insert(AUTHORIZATION, HeaderValue::from_static("Basic tok123"));
        assert!(bearer(&basic).is_none());
    }

    #[test]
    fn constant_time_eq_compares_exactly() {
        assert!(constant_time_eq(b"secret", b"secret"));
        assert!(constant_time_eq(b"", b""));
        assert!(!constant_time_eq(b"secret", b"secres"));
        assert!(!constant_time_eq(b"secret", b"secret-longer"));
    }
}
