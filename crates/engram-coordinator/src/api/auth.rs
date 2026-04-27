//! Bearer-token middleware.
//!
//! Wraps every protected route. The check is constant-time-ish (linear
//! scan over a small token list, byte-by-byte comparison) — at v1 we
//! don't need a separate auth service or per-token rotation.
//!
//! The empty-list short-circuit (`AuthState::accepts_anything`) is
//! load-bearing: dev `just dev` and the in-process integration tests
//! both run without `ENGRAM_AUTH_TOKENS` set, and we don't want every
//! test to grow an `Authorization` header. Production deployments set
//! the env var.

use std::sync::Arc;

use axum::body::Body;
use axum::extract::Request;
use axum::http::{header, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

/// Token allow-list snapshot. Built once at router-construction time
/// and shared across all handlers. We use `Arc<[String]>` rather than
/// reaching into `AppState.cfg` so the middleware doesn't depend on
/// the full state struct (smaller test surface, cheaper to clone).
#[derive(Clone, Debug)]
pub struct AuthState {
    tokens: Arc<[String]>,
}

impl AuthState {
    pub fn new(tokens: Vec<String>) -> Self {
        Self {
            tokens: tokens.into(),
        }
    }

    fn accepts_anything(&self) -> bool {
        self.tokens.is_empty()
    }

    fn accepts(&self, candidate: &str) -> bool {
        // Linear scan + constant-time byte compare per entry. Tokens
        // are short and the list is small (single-digit), so a hash
        // table would be overkill and would defeat constant-time
        // comparison anyway.
        self.tokens
            .iter()
            .any(|t| ct_eq(t.as_bytes(), candidate.as_bytes()))
    }
}

/// Constant-time byte comparison. We don't pull `subtle` as a dep —
/// this is one tight loop, dwarfed by the network round-trip, and the
/// `forbid(unsafe_code)` rule rules out the SIMD shortcut anyway.
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut acc = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        acc |= x ^ y;
    }
    acc == 0
}

/// Axum middleware. Lets the request through if:
/// 1. Auth is disabled (empty token list), OR
/// 2. The request carries `Authorization: Bearer <t>` with `<t>` in
///    the allow-list.
///
/// Otherwise returns the standard JSON error envelope with 401.
pub async fn require_bearer(
    axum::extract::State(state): axum::extract::State<AuthState>,
    req: Request<Body>,
    next: Next,
) -> Response {
    if state.accepts_anything() {
        return next.run(req).await;
    }

    let header_value = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());

    let token = match header_value.and_then(|h| h.strip_prefix("Bearer ")) {
        Some(t) => t.trim(),
        None => return unauthorized("missing or malformed Authorization header"),
    };

    if !state.accepts(token) {
        return unauthorized("invalid bearer token");
    }

    next.run(req).await
}

fn unauthorized(msg: &str) -> Response {
    // Reuse ApiError's JSON envelope so clients can match on `error`
    // the same way they do for the rest of the surface. We don't have
    // an `Unauthorized` variant in ApiError; status comes from a custom
    // pair instead of leaking a 500.
    let body = axum::Json(serde_json::json!({
        "error": "unauthorized",
        "message": msg,
    }));
    (StatusCode::UNAUTHORIZED, body).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_list_signals_dev_bypass_via_accepts_anything() {
        // The bypass is a separate signal from `accepts`. The middleware
        // checks `accepts_anything()` first and short-circuits;
        // `accepts(t)` honestly answers "is `t` in the allow-list?",
        // which for an empty list is always `false`. Splitting the two
        // means a refactor that drops the bypass would fail loud
        // instead of silently letting traffic through.
        let s = AuthState::new(vec![]);
        assert!(s.accepts_anything());
        assert!(!s.accepts("anything"));
    }

    #[test]
    fn populated_list_rejects_outsiders() {
        let s = AuthState::new(vec!["alpha".into(), "beta".into()]);
        assert!(!s.accepts_anything());
        assert!(s.accepts("alpha"));
        assert!(s.accepts("beta"));
        assert!(!s.accepts("gamma"));
        assert!(!s.accepts(""), "empty token must not be accepted");
    }

    #[test]
    fn ct_eq_distinguishes_lengths_and_contents() {
        assert!(ct_eq(b"abc", b"abc"));
        assert!(!ct_eq(b"abc", b"abcd"));
        assert!(!ct_eq(b"abc", b"abd"));
        assert!(ct_eq(b"", b""));
        assert!(!ct_eq(b"", b"x"));
    }

    #[test]
    fn token_list_is_case_sensitive() {
        // Bearer tokens are opaque: `Foo` and `foo` are different
        // values, just like JWT secrets. This locks that behaviour
        // in so a casing change in a future refactor would have to
        // be deliberate.
        let s = AuthState::new(vec!["Foo".into()]);
        assert!(s.accepts("Foo"));
        assert!(!s.accepts("foo"));
    }
}
