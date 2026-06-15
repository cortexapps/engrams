//! Machine auth for the app-gRPC surface (ADR 0039 §5): one trusted
//! caller (the orchestrator), one static bearer, constant-time compare.
//! Fail closed: no configured tokens = reject everything.
//!
//! Deliberately a *separate credential* from the host-agents'
//! `ENGRAM_AUTH_TOKENS` (`api/auth.rs`): different caller, different
//! blast radius, independently rotatable. And deliberately the
//! *opposite* dev posture from `api/auth.rs`'s `accepts_anything()`
//! empty-list bypass — this surface is born strict, so a deployment
//! that forgets `ENGRAM_APP_GRPC_TOKENS` serves `unauthenticated`
//! instead of an open admin plane. Boot is unaffected either way.

/// Static bearer allow-list for the app-gRPC services. Built once at
/// server construction from `CoordinatorConfig::app_grpc_tokens` and
/// shared (`Arc`) across the five service structs.
pub struct BearerAuth {
    /// >1 only during rotation overlap.
    tokens: Vec<String>,
}

impl BearerAuth {
    /// Drops empty-string tokens at construction: an empty token can
    /// never be valid credentials (and would otherwise match an empty
    /// presented bearer). The CLI layer in `main.rs` already filters
    /// these out; this is defense in depth for other construction sites
    /// (tests, future callers) that don't.
    pub fn new(tokens: Vec<String>) -> Self {
        Self {
            tokens: tokens.into_iter().filter(|t| !t.is_empty()).collect(),
        }
    }

    /// Per-RPC check, called at the top of every stub. Sync on purpose
    /// — a metadata lookup plus a constant-time compare doesn't earn a
    /// tower layer.
    //
    // `clippy::result_large_err`: `tonic::Status` is ~176 bytes, but it's
    // the error type tonic mandates on every RPC return anyway, so the
    // `?` at each call site flows straight into a `Result<_, Status>` —
    // boxing here would just force an unbox at every call. The lint
    // doesn't fire on the trait-impl RPCs (you can't change a trait
    // method's signature); it fires here only because `check` is an
    // inherent method with the same unavoidable error type.
    #[allow(clippy::result_large_err)]
    pub fn check<T>(&self, req: &tonic::Request<T>) -> Result<(), tonic::Status> {
        let presented = req
            .metadata()
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            // Byte-exact, case-sensitive scheme match: RFC 9110 makes the
            // `Bearer` scheme token case-insensitive, but we deliberately
            // don't honor that — one in-house machine caller we control,
            // which always sends exactly `Bearer `, so a stricter match is
            // a smaller surface with zero real-world cost.
            .and_then(|v| v.strip_prefix("Bearer "))
            .ok_or_else(|| tonic::Status::unauthenticated("missing bearer token"))?;
        // Linear scan + constant-time byte compare per entry, the same
        // shape as `api::auth`'s `ct_eq` (an empty `tokens` list never
        // matches, which is exactly the fail-closed posture we want).
        if self
            .tokens
            .iter()
            .any(|t| ct_eq(t.as_bytes(), presented.as_bytes()))
        {
            Ok(())
        } else {
            Err(tonic::Status::unauthenticated("invalid bearer token"))
        }
    }
}

/// Constant-time byte comparison; mirrors `api::auth::ct_eq` (private
/// there, and that file is frozen — it guards host ingest). One tight
/// loop, dwarfed by the network round-trip; not worth a `subtle` dep.
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

#[cfg(test)]
mod tests {
    use super::*;

    fn req_with_bearer(token: &str) -> tonic::Request<()> {
        let mut req = tonic::Request::new(());
        req.metadata_mut().insert(
            "authorization",
            format!("Bearer {token}").parse().expect("ascii header"),
        );
        req
    }

    #[test]
    fn right_token_passes() {
        let auth = BearerAuth::new(vec!["s3cret".into()]);
        assert!(auth.check(&req_with_bearer("s3cret")).is_ok());
    }

    #[test]
    fn rotation_overlap_accepts_either_token() {
        let auth = BearerAuth::new(vec!["old".into(), "new".into()]);
        assert!(auth.check(&req_with_bearer("old")).is_ok());
        assert!(auth.check(&req_with_bearer("new")).is_ok());
    }

    #[test]
    fn wrong_token_is_unauthenticated() {
        let auth = BearerAuth::new(vec!["s3cret".into()]);
        let err = auth.check(&req_with_bearer("nope")).unwrap_err();
        assert_eq!(err.code(), tonic::Code::Unauthenticated);
        assert_eq!(err.message(), "invalid bearer token");
    }

    #[test]
    fn missing_header_is_unauthenticated() {
        let auth = BearerAuth::new(vec!["s3cret".into()]);
        let err = auth.check(&tonic::Request::new(())).unwrap_err();
        assert_eq!(err.code(), tonic::Code::Unauthenticated);
        assert_eq!(err.message(), "missing bearer token");
    }

    #[test]
    fn malformed_scheme_is_unauthenticated() {
        // `Basic` (or a bare token) is not a bearer; same error as
        // missing so the response doesn't leak which part was wrong.
        let auth = BearerAuth::new(vec!["s3cret".into()]);
        let mut req = tonic::Request::new(());
        req.metadata_mut()
            .insert("authorization", "Basic s3cret".parse().unwrap());
        let err = auth.check(&req).unwrap_err();
        assert_eq!(err.code(), tonic::Code::Unauthenticated);
        assert_eq!(err.message(), "missing bearer token");
    }

    #[test]
    fn empty_token_set_fails_closed() {
        // The opposite of `api/auth.rs`'s `accepts_anything()` dev
        // bypass, on purpose: no configured tokens means NOTHING is
        // accepted on this surface — not even an empty bearer.
        let auth = BearerAuth::new(vec![]);
        let err = auth.check(&req_with_bearer("anything")).unwrap_err();
        assert_eq!(err.code(), tonic::Code::Unauthenticated);
        let err = auth.check(&req_with_bearer("")).unwrap_err();
        assert_eq!(err.code(), tonic::Code::Unauthenticated);
        let err = auth.check(&tonic::Request::new(())).unwrap_err();
        assert_eq!(err.code(), tonic::Code::Unauthenticated);
    }

    #[test]
    fn tokens_are_case_sensitive_and_exact() {
        let auth = BearerAuth::new(vec!["Token".into()]);
        assert!(auth.check(&req_with_bearer("Token")).is_ok());
        assert!(auth.check(&req_with_bearer("token")).is_err());
        assert!(auth.check(&req_with_bearer("Token ")).is_err());
        assert!(auth.check(&req_with_bearer("Toke")).is_err());
    }
}
