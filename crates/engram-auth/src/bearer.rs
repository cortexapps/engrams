//! `ServiceBearer` — the existing deployment bearer token, now one link in
//! the verifier chain. A valid token resolves to an admin-equivalent service
//! principal (host-agent / CLI / machine callers, and automation hitting
//! admin endpoints). The same `--auth-tokens` value the coordinator already
//! used for `require_bearer`.

use async_trait::async_trait;
use engram_core::types::user::{Principal, Role};

use crate::error::AuthError;
use crate::verify::{IdentityVerifier, Verified, VerifyInput};
use crate::SERVICE_USER_ID;

pub struct ServiceBearer {
    tokens: Vec<String>,
    principal: Principal,
}

impl ServiceBearer {
    /// `service_email` is the identity stamped for machine-created sessions /
    /// git attribution.
    pub fn new(tokens: Vec<String>, service_email: impl Into<String>) -> Self {
        Self {
            tokens,
            principal: Principal {
                user_id: SERVICE_USER_ID.into(),
                display_name: Some("Engram Service".to_string()),
                email: service_email.into(),
                role: Role::Admin,
                active: true,
            },
        }
    }
}

/// Constant-time equality so token comparison doesn't leak length-prefix
/// matches via timing. Same posture as the coordinator's existing bearer check.
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[async_trait]
impl IdentityVerifier for ServiceBearer {
    async fn verify(&self, input: &VerifyInput) -> Result<Option<Verified>, AuthError> {
        let Some(presented) = input.bearer() else {
            return Ok(None);
        };
        let matched = self
            .tokens
            .iter()
            .any(|t| ct_eq(t.as_bytes(), presented.as_bytes()));
        if matched {
            Ok(Some(Verified::Principal(self.principal.clone())))
        } else {
            // A bearer token was presented but didn't match. Fall through
            // (Ok(None)) rather than hard-failing: the same Authorization
            // header could in principle be meant for another verifier, and
            // the chain's terminal miss is what produces the 401. Returning
            // Err here would be equivalent for the 401, but None keeps the
            // chain composable.
            Ok(None)
        }
    }

    fn name(&self) -> &'static str {
        "service-bearer"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input_with_bearer(tok: &str) -> VerifyInput {
        let mut i = VerifyInput::default();
        i.headers
            .insert("authorization".into(), format!("Bearer {tok}"));
        i
    }

    #[tokio::test]
    async fn valid_token_resolves_admin_service_principal() {
        let v = ServiceBearer::new(vec!["s3cret".into()], "svc@engram.local");
        let got = v.verify(&input_with_bearer("s3cret")).await.unwrap();
        match got {
            Some(Verified::Principal(p)) => {
                assert!(p.is_admin());
                assert_eq!(p.email, "svc@engram.local");
            }
            other => panic!("expected service principal, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn wrong_or_absent_token_falls_through() {
        let v = ServiceBearer::new(vec!["s3cret".into()], "svc@engram.local");
        assert!(v.verify(&input_with_bearer("nope")).await.unwrap().is_none());
        assert!(v.verify(&VerifyInput::default()).await.unwrap().is_none());
    }
}
