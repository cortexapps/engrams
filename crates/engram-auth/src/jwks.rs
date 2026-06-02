//! Shared JWT verification: fetch a JWKS, resolve the signing key by `kid`,
//! and validate registered + custom claims. Used by both the forward-auth
//! verifier and the OIDC ID-token check.

use std::collections::HashMap;

use jsonwebtoken::jwk::JwkSet;
use jsonwebtoken::{decode, decode_header, Algorithm, DecodingKey, Validation};
use serde_json::Value;

use crate::error::AuthError;
use crate::verify::VerifiedEmail;

/// Fetch a JWKS document.
pub async fn fetch_jwks(client: &reqwest::Client, url: &str) -> Result<JwkSet, AuthError> {
    client
        .get(url)
        .send()
        .await
        .map_err(|e| AuthError::Http(format!("jwks fetch {url}: {e}")))?
        .error_for_status()
        .map_err(|e| AuthError::Http(format!("jwks fetch {url}: {e}")))?
        .json::<JwkSet>()
        .await
        .map_err(|e| AuthError::Http(format!("jwks decode {url}: {e}")))
}

/// Resolve the decoding key for a token from a JWKS via the token's `kid`.
pub fn resolve_key(jwks: &JwkSet, token: &str) -> Result<(DecodingKey, Algorithm), AuthError> {
    let header = decode_header(token).map_err(|e| AuthError::Verify(format!("jwt header: {e}")))?;
    let kid = header
        .kid
        .ok_or_else(|| AuthError::Verify("jwt has no kid".into()))?;
    let jwk = jwks
        .find(&kid)
        .ok_or_else(|| AuthError::Verify(format!("no jwk for kid {kid}")))?;
    let key = DecodingKey::from_jwk(jwk).map_err(|e| AuthError::Verify(format!("bad jwk: {e}")))?;
    Ok((key, header.alg))
}

/// Validate a token's signature + registered claims (exp always; iss/aud when
/// supplied), check an optional `nonce`, and extract the email + display name.
pub fn validate_claims(
    token: &str,
    key: &DecodingKey,
    alg: Algorithm,
    issuer: Option<&str>,
    audience: Option<&str>,
    email_claim: &str,
    expected_nonce: Option<&str>,
) -> Result<VerifiedEmail, AuthError> {
    let mut validation = Validation::new(alg);
    if let Some(iss) = issuer {
        validation.set_issuer(&[iss]);
    }
    match audience {
        Some(aud) => validation.set_audience(&[aud]),
        // No audience configured → don't require an `aud` claim (jsonwebtoken
        // validates aud by default).
        None => validation.validate_aud = false,
    }

    let data = decode::<HashMap<String, Value>>(token, key, &validation)
        .map_err(|e| AuthError::Verify(format!("jwt validation: {e}")))?;
    let claims = data.claims;

    if let Some(expected) = expected_nonce {
        let got = claims.get("nonce").and_then(Value::as_str);
        if got != Some(expected) {
            return Err(AuthError::Verify("id-token nonce mismatch".into()));
        }
    }

    let email = claims
        .get(email_claim)
        .and_then(Value::as_str)
        .ok_or_else(|| AuthError::Verify(format!("token missing `{email_claim}` claim")))?
        .to_string();
    let display_name = claims
        .get("name")
        .and_then(Value::as_str)
        .map(str::to_string);
    Ok(VerifiedEmail {
        email,
        display_name,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use jsonwebtoken::{encode, EncodingKey, Header};
    use serde_json::json;

    const SECRET: &[u8] = b"test-hs256-secret";

    fn sign(claims: &Value) -> String {
        encode(
            &Header::new(Algorithm::HS256),
            claims,
            &EncodingKey::from_secret(SECRET),
        )
        .unwrap()
    }

    fn key() -> DecodingKey {
        DecodingKey::from_secret(SECRET)
    }

    fn future() -> i64 {
        // Far-future exp so the test never flakes on clock; chrono avoided in
        // unit code per project convention, but tests may use a fixed offset.
        4_102_444_800 // 2100-01-01
    }

    #[test]
    fn valid_token_extracts_email_and_name() {
        let tok = sign(&json!({
            "email": "ada@example.com", "name": "Ada", "iss": "https://idp", "aud": "client-1", "exp": future(), "nonce": "n1"
        }));
        let v = validate_claims(
            &tok,
            &key(),
            Algorithm::HS256,
            Some("https://idp"),
            Some("client-1"),
            "email",
            Some("n1"),
        )
        .unwrap();
        assert_eq!(v.email, "ada@example.com");
        assert_eq!(v.display_name.as_deref(), Some("Ada"));
    }

    #[test]
    fn expired_token_is_rejected() {
        let tok = sign(&json!({ "email": "a@b.com", "exp": 1_000_000_000i64 }));
        let err =
            validate_claims(&tok, &key(), Algorithm::HS256, None, None, "email", None).unwrap_err();
        assert!(matches!(err, AuthError::Verify(_)));
    }

    #[test]
    fn wrong_audience_is_rejected() {
        let tok = sign(&json!({ "email": "a@b.com", "aud": "other", "exp": future() }));
        let err = validate_claims(
            &tok,
            &key(),
            Algorithm::HS256,
            None,
            Some("client-1"),
            "email",
            None,
        )
        .unwrap_err();
        assert!(matches!(err, AuthError::Verify(_)));
    }

    #[test]
    fn missing_email_claim_is_rejected() {
        let tok = sign(&json!({ "sub": "u1", "exp": future() }));
        let err =
            validate_claims(&tok, &key(), Algorithm::HS256, None, None, "email", None).unwrap_err();
        assert!(matches!(err, AuthError::Verify(m) if m.contains("email")));
    }

    #[test]
    fn nonce_mismatch_is_rejected() {
        let tok = sign(&json!({ "email": "a@b.com", "exp": future(), "nonce": "real" }));
        let err = validate_claims(
            &tok,
            &key(),
            Algorithm::HS256,
            None,
            None,
            "email",
            Some("expected"),
        )
        .unwrap_err();
        assert!(matches!(err, AuthError::Verify(m) if m.contains("nonce")));
    }

    #[test]
    fn configurable_email_claim() {
        // GCP IAP forwards the address in `email`; some IdPs use a different
        // claim — verify the claim name is honoured.
        let tok = sign(&json!({ "preferred_username": "x@y.z", "exp": future() }));
        let v = validate_claims(
            &tok,
            &key(),
            Algorithm::HS256,
            None,
            None,
            "preferred_username",
            None,
        )
        .unwrap();
        assert_eq!(v.email, "x@y.z");
    }
}
