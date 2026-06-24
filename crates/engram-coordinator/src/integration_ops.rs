//! Server-side integration invocation (the "IntegrationOp" seam) — the logic
//! behind `grpc_app::integration_op`. Sessionless: a connector's credential is
//! resolved coordinator-side (KEK-unseal / mint) and either used to make the call
//! here (`run_integration_op`, Mode A — secret never leaves the coordinator) or
//! handed back to the orchestrator (`resolve_integration_credential`, Mode B — for
//! an off-the-shelf SDK). Mirrors `grpc_app::mint::run_connector_test`, generalized
//! from a benign GET to an arbitrary request.
//!
//! Kept out of `grpc_app/` on purpose: the auth-convention test there counts one
//! `self.auth.check` per `async fn`, so the helper async fns live here and the RPC
//! methods stay thin (check + delegate).

use std::collections::HashMap;
use std::time::Duration;

use engram_core::traits::{CredentialHint, ScopedCredential, SecretContext};
use engram_core::types::SecretSchema;
use engram_protocol::app;

use crate::state::SharedState;

/// Response-body cap: an integration op returns control-plane-sized JSON, not a
/// stream. Anything larger is truncated and flagged.
const MAX_BODY: usize = 1024 * 1024;

/// Make one authenticated request against the connector's host and return the
/// upstream status + (capped) body. Redirects are NOT followed — a cross-host 30x
/// would leak the injected credential.
pub async fn run_integration_op(
    state: &SharedState,
    req: app::RunIntegrationOpRequest,
) -> Result<app::RunIntegrationOpResponse, String> {
    let http = reqwest::Client::builder()
        .user_agent("engram-integration-op")
        .timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| format!("http client: {e}"))?;

    let method = reqwest::Method::from_bytes(req.method.to_uppercase().as_bytes())
        .map_err(|_| format!("invalid HTTP method {:?}", req.method))?;
    let url = format!("https://{}{}", req.host, req.path);
    let mut builder = http.request(method, &url);

    let spec = req.credential.unwrap_or_default();
    builder = apply_credential(state, &spec, builder).await?;

    if !req.body.is_empty() {
        let ct = if req.content_type.is_empty() {
            "application/json"
        } else {
            req.content_type.as_str()
        };
        builder = builder
            .header(reqwest::header::CONTENT_TYPE, ct)
            .body(req.body);
    }

    let resp = builder
        .send()
        .await
        .map_err(|e| format!("request to {} failed: {e}", req.host))?;
    let status = u32::from(resp.status().as_u16());
    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let full = resp
        .bytes()
        .await
        .map_err(|e| format!("reading response body from {} failed: {e}", req.host))?;
    let truncated = full.len() > MAX_BODY;
    let body = if truncated {
        full.slice(0..MAX_BODY).to_vec()
    } else {
        full.to_vec()
    };

    Ok(app::RunIntegrationOpResponse {
        status,
        body,
        content_type,
        truncated,
    })
}

/// Resolve a connector's credential to its raw material for the orchestrator (Mode
/// B). The token crosses to the orchestrator tier — a trusted first-party backend —
/// but never to a guest VM. Audit-logged: a server-side credential read is
/// security-relevant.
pub async fn resolve_integration_credential(
    state: &SharedState,
    req: app::ResolveIntegrationCredentialRequest,
) -> Result<app::ResolvedCredential, String> {
    tracing::info!(
        provider = %req.provider,
        "integration credential resolved to the orchestrator (Mode B)"
    );
    let spec = req.credential.unwrap_or_default();

    if spec.source == "mint" {
        let scoped = mint_scoped(state, &spec.mint_provider).await?;
        return Ok(scoped_to_resolved(scoped));
    }

    // inject: a single secret yields a bearer (the raw token an SDK wants);
    // multiple secrets yield a header-name → raw-value map (e.g. Datadog's two keys).
    if spec.injects.is_empty() {
        return Err("connector has no inject credential".to_string());
    }
    if spec.injects.len() == 1 {
        let token = resolve_secret(state, &spec.injects[0].secret_ref).await?;
        return Ok(app::ResolvedCredential {
            cred: Some(app::resolved_credential::Cred::Bearer(app::BearerCred {
                token,
            })),
            expires_at: String::new(),
        });
    }
    let mut values = HashMap::new();
    for inj in &spec.injects {
        let value = resolve_secret(state, &inj.secret_ref).await?;
        let name = if inj.header.is_empty() {
            "Authorization".to_string()
        } else {
            inj.header.clone()
        };
        values.insert(name, value);
    }
    Ok(app::ResolvedCredential {
        cred: Some(app::resolved_credential::Cred::Headers(app::HeadersCred {
            values,
        })),
        expires_at: String::new(),
    })
}

/// Attach the resolved credential to an outbound request builder. For inject, apply
/// every declared header (templated); for mint, attach the provider's own header.
async fn apply_credential(
    state: &SharedState,
    spec: &app::CredentialSpec,
    builder: reqwest::RequestBuilder,
) -> Result<reqwest::RequestBuilder, String> {
    if spec.source == "mint" {
        let engine = state
            .integrations
            .resolve(&spec.mint_provider, &state.services.secrets)
            .await
            .ok_or_else(|| "mint credentials are not configured".to_string())?;
        let hint = CredentialHint {
            served_host: None,
            owner: None,
        };
        let scoped = engine
            .mint_credential(&[], &hint)
            .await
            .map_err(|e| format!("could not mint a token: {e}"))?;
        let header = engine.inject_header(&scoped).ok_or_else(|| {
            "credential is not header-injectable (e.g. AWS SigV4 needs request signing)".to_string()
        })?;
        return Ok(builder.header(header.name, header.value));
    }
    if spec.injects.is_empty() {
        return Err("connector has no inject credential".to_string());
    }
    let mut b = builder;
    for inj in &spec.injects {
        let value = resolve_secret(state, &inj.secret_ref).await?;
        let header_name = if inj.header.is_empty() {
            "Authorization"
        } else {
            inj.header.as_str()
        };
        let template = if inj.template.is_empty() {
            "{}"
        } else {
            inj.template.as_str()
        };
        b = b.header(header_name, template.replace("{}", &value));
    }
    Ok(b)
}

/// Mint a default-scoped credential for a mint provider (empty caps, like the
/// connector-test probe — a sessionless op carries no session capabilities).
async fn mint_scoped(state: &SharedState, provider: &str) -> Result<ScopedCredential, String> {
    let engine = state
        .integrations
        .resolve(provider, &state.services.secrets)
        .await
        .ok_or_else(|| "mint credentials are not configured".to_string())?;
    let hint = CredentialHint {
        served_host: None,
        owner: None,
    };
    engine
        .mint_credential(&[], &hint)
        .await
        .map_err(|e| format!("could not mint a token: {e}"))
}

/// Resolve one org secret by name (KEK-unseal via the composed SecretStore).
async fn resolve_secret(state: &SharedState, name: &str) -> Result<String, String> {
    let ctx = SecretContext {
        repo: "",
        image_tag: "",
    };
    let schema = SecretSchema {
        required: true,
        ..Default::default()
    };
    state
        .services
        .secrets
        .get(&ctx, name, &schema)
        .await
        .map_err(|e| format!("could not resolve org secret: {e}"))?
        .ok_or_else(|| format!("org secret \"{name}\" is not set"))
}

/// Map the domain `ScopedCredential` onto the wire `ResolvedCredential`.
fn scoped_to_resolved(s: ScopedCredential) -> app::ResolvedCredential {
    match s {
        ScopedCredential::Bearer { token, expires_at } => app::ResolvedCredential {
            cred: Some(app::resolved_credential::Cred::Bearer(app::BearerCred {
                token,
            })),
            expires_at: expires_at.to_rfc3339(),
        },
        ScopedCredential::Basic {
            username,
            password,
            expires_at,
        } => app::ResolvedCredential {
            cred: Some(app::resolved_credential::Cred::Basic(app::BasicCred {
                username,
                password,
            })),
            expires_at: expires_at.to_rfc3339(),
        },
        ScopedCredential::AwsSts {
            access_key_id,
            secret_access_key,
            session_token,
            expires_at,
        } => app::ResolvedCredential {
            cred: Some(app::resolved_credential::Cred::AwsSts(app::AwsStsCred {
                access_key_id,
                secret_access_key,
                session_token,
            })),
            expires_at: expires_at.to_rfc3339(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn fixed_exp() -> chrono::DateTime<chrono::Utc> {
        chrono::Utc.with_ymd_and_hms(2030, 1, 1, 0, 0, 0).unwrap()
    }

    #[test]
    fn bearer_maps_to_bearer_with_rfc3339_expiry() {
        let r = scoped_to_resolved(ScopedCredential::Bearer {
            token: "xoxb-tok".into(),
            expires_at: fixed_exp(),
        });
        match r.cred {
            Some(app::resolved_credential::Cred::Bearer(b)) => assert_eq!(b.token, "xoxb-tok"),
            other => panic!("expected bearer, got {other:?}"),
        }
        assert_eq!(r.expires_at, fixed_exp().to_rfc3339());
    }

    #[test]
    fn basic_maps_to_basic() {
        let r = scoped_to_resolved(ScopedCredential::Basic {
            username: "x-access-token".into(),
            password: "ghs_pw".into(),
            expires_at: fixed_exp(),
        });
        match r.cred {
            Some(app::resolved_credential::Cred::Basic(b)) => {
                assert_eq!(b.username, "x-access-token");
                assert_eq!(b.password, "ghs_pw");
            }
            other => panic!("expected basic, got {other:?}"),
        }
    }

    #[test]
    fn aws_sts_maps_to_aws_sts() {
        let r = scoped_to_resolved(ScopedCredential::AwsSts {
            access_key_id: "AKIA".into(),
            secret_access_key: "sk".into(),
            session_token: "st".into(),
            expires_at: fixed_exp(),
        });
        match r.cred {
            Some(app::resolved_credential::Cred::AwsSts(s)) => {
                assert_eq!(s.access_key_id, "AKIA");
                assert_eq!(s.secret_access_key, "sk");
                assert_eq!(s.session_token, "st");
            }
            other => panic!("expected aws_sts, got {other:?}"),
        }
    }
}
