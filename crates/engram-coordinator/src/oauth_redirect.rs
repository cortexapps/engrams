//! ADR 0106 addendum: the authorization-code (redirect) flow family for
//! connector OAuth.
//!
//! One spec-driven driver serves every provider: the connector's `oauth`
//! facet arrives over the wire as a [`RedirectOauthSpec`] (the orchestrator
//! enforced host containment at the connector parse boundary; this module
//! re-checks shape only), and the coordinator — the sole tier that reads
//! org secrets — resolves the BYO client credentials, assembles the
//! authorize URL, and runs the code→token exchange.
//!
//! Unlike the device-code family, redirect flows hold no in-memory handle
//! and no owner-lease loop: the durable `oauth_flows` row IS the CSRF state
//! (`state` = flow id, 122 bits of injected entropy), begun on one replica
//! and completable on any other. The pending→terminal transition fences
//! racing callbacks; the provider's single-use authorization code fences
//! double exchanges.

use std::collections::BTreeMap;
use std::time::Duration;

use base64::Engine as _;
use engram_core::traits::SecretContext;
use engram_core::types::connector_oauth::{
    normalize_scope, ConnectorOAuthBundle, ConnectorOAuthKind, ConnectorOAuthRefreshSpec,
    CONNECTOR_OAUTH_BUNDLE_VERSION,
};
use engram_core::types::oauth::{
    OAuthAccountMetadata, OAuthCredentialKey, OAuthFlow, OAuthFlowStatus, SealedOAuthCredential,
};
use engram_core::types::SecretSchema;
// digest 0.11 moved `new_from_slice` off `Mac` and onto `KeyInit`, so the
// keyed constructor needs that trait in scope as well as `Mac` itself.
use hmac::{Hmac, KeyInit, Mac};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::oauth::{
    validate_key, OAuthDriverError, OAuthManager, OAuthServiceError, ValidatedOAuthBundle,
};

/// Consent flows finish in minutes; a lapsed attempt is auto-cancelled by
/// the next begin, so a short TTL never strands the admin.
const REDIRECT_FLOW_TTL: chrono::Duration = chrono::Duration::minutes(15);
const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_PROBE_RESPONSE: usize = 256 * 1024;
const MAX_SCOPES: usize = 50;
const MAX_EXTRA_PARAMS: usize = 16;
const MAX_PARAM_LEN: usize = 200;
const MAX_PROBE_REQUEST_BODY: usize = 4096;
const MAX_DOT_SEGMENTS: usize = 8;

/// Parameters the flow machinery owns; a facet may not override them.
const RESERVED_AUTHORIZE_PARAMS: &[&str] = &[
    "client_id",
    "client_secret",
    "redirect_uri",
    "state",
    "scope",
    "response_type",
    "grant_type",
    "code",
    "code_challenge",
    "code_challenge_method",
    "code_verifier",
];

pub const META_ACCOUNT_ID: &str = "account_id";
pub const META_DISPLAY_NAME: &str = "display_name";
pub const META_WORKSPACE_ID: &str = "workspace_id";
pub const META_WORKSPACE_NAME: &str = "workspace_name";
const METADATA_FIELDS: &[&str] = &[
    META_ACCOUNT_ID,
    META_DISPLAY_NAME,
    META_WORKSPACE_ID,
    META_WORKSPACE_NAME,
];

/// Wire-supplied, orchestrator-validated redirect spec (the connector's
/// oauth facet in coordinator terms).
#[derive(Clone, Debug)]
pub struct RedirectOauthSpec {
    pub authorize_url: String,
    pub token_url: String,
    pub scopes: Vec<String>,
    /// Empty means comma (Slack, Linear).
    pub scope_delimiter: String,
    pub extra_authorize_params: BTreeMap<String, String>,
    pub client_id_ref: String,
    pub client_secret_ref: String,
    pub pkce: bool,
    pub metadata: RedirectMetadataSpec,
    /// ADR 0115: the authorize query parameter that carries the joined
    /// scopes. Empty means the RFC name, `scope`. Slack's user-subject flow
    /// sends `user_scope` so the provider mints a USER token, not a bot.
    pub scopes_param: String,
    /// ADR 0115: dot-path to the grant object inside the token response.
    /// Empty means the root. Slack's user token lives under `authed_user`
    /// (`access_token`/`token_type`/`scope` are read from that object;
    /// metadata dot-paths still see the full response).
    pub grant_path: String,
}

#[derive(Clone, Debug, Default)]
pub struct RedirectMetadataSpec {
    /// Metadata field → dot-path into the token response JSON.
    pub from_token_response: BTreeMap<String, String>,
    pub probe: Option<RedirectMetadataProbe>,
}

/// One bounded "who am I" request against a connector host, authenticated
/// with the new access token.
#[derive(Clone, Debug)]
pub struct RedirectMetadataProbe {
    pub method: String,
    pub host: String,
    pub path: String,
    pub body: String,
    /// Metadata field → dot-path into the probe response JSON.
    pub map: BTreeMap<String, String>,
}

#[derive(Clone, Debug)]
pub struct BegunRedirectFlow {
    pub flow: OAuthFlow,
    /// Fully-assembled provider URL; the orchestrator 302s the admin to it.
    pub authorize_url: String,
}

impl RedirectOauthSpec {
    pub fn delimiter(&self) -> &str {
        if self.scope_delimiter.is_empty() {
            ","
        } else {
            &self.scope_delimiter
        }
    }

    fn scopes_param(&self) -> &str {
        if self.scopes_param.is_empty() {
            "scope"
        } else {
            &self.scopes_param
        }
    }

    fn validate(&self) -> Result<(), String> {
        validate_https_url(&self.authorize_url, "authorize_url")?;
        validate_https_url(&self.token_url, "token_url")?;
        if self.scopes.len() > MAX_SCOPES {
            return Err(format!("too many scopes (max {MAX_SCOPES})"));
        }
        if self
            .scopes
            .iter()
            .any(|s| s.is_empty() || s.chars().any(char::is_whitespace))
        {
            return Err("scopes must be non-empty and whitespace-free".into());
        }
        if !matches!(self.scope_delimiter.as_str(), "" | "," | " ") {
            return Err("scope_delimiter must be \",\" or \" \"".into());
        }
        if self.extra_authorize_params.len() > MAX_EXTRA_PARAMS {
            return Err(format!(
                "too many extra authorize params (max {MAX_EXTRA_PARAMS})"
            ));
        }
        for (k, v) in &self.extra_authorize_params {
            if RESERVED_AUTHORIZE_PARAMS.contains(&k.as_str()) {
                return Err(format!("authorize param {k:?} is reserved"));
            }
            if k.is_empty()
                || k.len() > MAX_PARAM_LEN
                || v.len() > MAX_PARAM_LEN
                || k.chars().any(char::is_control)
                || v.chars().any(char::is_control)
            {
                return Err("extra authorize params must be short, control-free strings".into());
            }
        }
        if self.client_id_ref.trim().is_empty() || self.client_secret_ref.trim().is_empty() {
            return Err("client id/secret refs are required".into());
        }
        if !self.scopes_param.is_empty()
            && (self.scopes_param.len() > 32
                || !self
                    .scopes_param
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c == '_'))
        {
            return Err("scopes_param must be a short lowercase identifier".into());
        }
        if !self.grant_path.is_empty()
            && (self.grant_path.len() > 64
                || self.grant_path.split('.').count() > 4
                || self.grant_path.split('.').any(|seg| {
                    seg.is_empty() || !seg.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                }))
        {
            return Err("grant_path must be a short dot-path".into());
        }
        for (field, path) in &self.metadata.from_token_response {
            validate_metadata_mapping(field, path)?;
        }
        if let Some(probe) = &self.metadata.probe {
            if !matches!(probe.method.as_str(), "GET" | "POST") {
                return Err("probe method must be GET or POST".into());
            }
            validate_host(&probe.host)?;
            if !probe.path.starts_with('/') || probe.path.len() > MAX_PARAM_LEN {
                return Err("probe path must be a short absolute path".into());
            }
            if probe.body.len() > MAX_PROBE_REQUEST_BODY {
                return Err("probe body is too large".into());
            }
            if probe.map.is_empty() {
                return Err("a probe must map at least one metadata field".into());
            }
            for (field, path) in &probe.map {
                validate_metadata_mapping(field, path)?;
            }
        }
        Ok(())
    }
}

fn validate_https_url(raw: &str, what: &str) -> Result<(), String> {
    let url = reqwest::Url::parse(raw).map_err(|e| format!("invalid {what}: {e}"))?;
    let Some(host) = url.host_str() else {
        return Err(format!("{what} must be an absolute URL"));
    };
    // Plain http is allowed only toward loopback (dev stacks and the fake
    // providers in tests). The orchestrator's connector parse boundary
    // additionally requires https for real facets.
    let loopback = matches!(host, "localhost" | "127.0.0.1" | "[::1]");
    if !(url.scheme() == "https" || (url.scheme() == "http" && loopback)) {
        return Err(format!("{what} must be an absolute https URL"));
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(format!("{what} must not carry a query or fragment"));
    }
    Ok(())
}

fn validate_host(host: &str) -> Result<(), String> {
    let (name, port) = match host.split_once(':') {
        Some((name, port)) => (name, Some(port)),
        None => (host, None),
    };
    if name.is_empty()
        || host.len() > 253
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
        || port.is_some_and(|p| p.is_empty() || p.parse::<u16>().is_err())
    {
        return Err("probe host must be a bare hostname".into());
    }
    Ok(())
}

/// Loopback probes go over plain http (dev stacks, test fakes); everything
/// else is https.
fn probe_url(host: &str, path: &str) -> String {
    let name = host.split(':').next().unwrap_or(host);
    let scheme = if matches!(name, "localhost" | "127.0.0.1") {
        "http"
    } else {
        "https"
    };
    format!("{scheme}://{host}{path}")
}

fn validate_metadata_mapping(field: &str, path: &str) -> Result<(), String> {
    if !METADATA_FIELDS.contains(&field) {
        return Err(format!("unknown metadata field {field:?}"));
    }
    let segments: Vec<&str> = path.split('.').collect();
    if segments.is_empty() || segments.len() > MAX_DOT_SEGMENTS {
        return Err(format!("metadata path {path:?} has too many segments"));
    }
    for seg in segments {
        if seg.is_empty()
            || seg.len() > 64
            || !seg.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            return Err(format!("metadata path {path:?} has an invalid segment"));
        }
    }
    Ok(())
}

/// Walk a bounded dot-path through JSON objects. No array indexing — the
/// metadata surfaces this feeds are scalar identity fields.
fn dot_path<'v>(json: &'v Value, path: &str) -> Option<&'v Value> {
    let mut cursor = json;
    for seg in path.split('.') {
        cursor = cursor.as_object()?.get(seg)?;
    }
    Some(cursor)
}

/// Scalar-to-string for identity fields; numeric ids are common (GitHub,
/// Discord) so numbers stringify.
fn value_to_string(value: &Value) -> Option<String> {
    match value {
        Value::String(s) if !s.trim().is_empty() => Some(s.trim().to_owned()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn extract_mapped(
    map: &BTreeMap<String, String>,
    json: &Value,
    out: &mut BTreeMap<String, String>,
) {
    for (field, path) in map {
        if let Some(value) = dot_path(json, path).and_then(value_to_string) {
            out.insert(field.clone(), value);
        }
    }
}

/// The parsed, provider-agnostic result of a token grant. Shared with the
/// refresh machinery, which runs the same parse over `refresh_token` grants.
pub(crate) struct TokenGrant {
    pub(crate) access_token: String,
    pub(crate) token_type: String,
    pub(crate) refresh_token: Option<String>,
    pub(crate) scope: Option<String>,
    pub(crate) expires_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Raw JSON, for metadata extraction.
    pub(crate) json: Value,
}

fn driver_err(code: &'static str, detail: impl Into<String>) -> OAuthServiceError {
    OAuthServiceError::Driver(OAuthDriverError {
        code,
        detail: detail.into(),
    })
}

fn base64url(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// PKCE verifier, derived rather than persisted: recomputable on any
/// replica from the client secret + flow id, so `oauth_flows` keeps its
/// no-codes-at-rest invariant.
fn derive_pkce_verifier(client_secret: &str, flow_id: uuid::Uuid) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(client_secret.as_bytes())
        .expect("HMAC accepts any key length");
    mac.update(b"engram-connector-pkce-v1:");
    mac.update(flow_id.as_bytes());
    base64url(&mac.finalize().into_bytes())
}

fn pkce_challenge(verifier: &str) -> String {
    base64url(&Sha256::digest(verifier.as_bytes()))
}

impl OAuthManager {
    async fn resolve_org_secret(&self, name: &str) -> Result<String, OAuthServiceError> {
        let ctx = SecretContext {
            repo: "",
            image_tag: "",
        };
        let schema = SecretSchema {
            required: true,
            ..Default::default()
        };
        self.secrets
            .get(&ctx, name, &schema)
            .await
            .map_err(|e| {
                OAuthServiceError::BadRequest(format!("could not resolve org secret: {e}"))
            })?
            .ok_or_else(|| OAuthServiceError::BadRequest(format!("org secret {name:?} is not set")))
    }

    /// Start a redirect flow: durable flow row (the CSRF state) + the fully
    /// assembled provider authorize URL. A stale pending attempt for the
    /// same subject+provider is cancelled — starting over always works.
    pub async fn begin_redirect(
        &self,
        key: OAuthCredentialKey,
        spec: &RedirectOauthSpec,
        redirect_uri: &str,
    ) -> Result<BegunRedirectFlow, OAuthServiceError> {
        validate_key(&key)?;
        spec.validate().map_err(OAuthServiceError::BadRequest)?;
        if redirect_uri.trim().is_empty() {
            return Err(OAuthServiceError::BadRequest(
                "redirect_uri is required".into(),
            ));
        }
        let client_id = self.resolve_org_secret(&spec.client_id_ref).await?;

        if let Some(stale) = self.meta.get_pending_oauth_flow(&key).await? {
            // Best-effort: a concurrent completion may win the transition,
            // in which case create below conflicts and the caller retries.
            let _ = self
                .meta
                .finish_oauth_flow_unowned(stale.id, OAuthFlowStatus::Cancelled, Some("superseded"))
                .await;
        }

        let flow_id = self.entropy.uuid();
        let now = self.clock.now_utc();
        let flow = OAuthFlow {
            id: flow_id,
            key: key.clone(),
            owner_replica: self.replica.clone(),
            // No renewal loop runs for redirect flows: the lease must span
            // the whole TTL or the cleanup sweeper would mark the flow
            // owner-lost mid-consent.
            lease_expires_at: now + REDIRECT_FLOW_TTL,
            expires_at: now + REDIRECT_FLOW_TTL,
            status: OAuthFlowStatus::Pending,
            error_code: None,
            created_at: now,
            updated_at: now,
        };
        self.meta.create_oauth_flow(flow.clone()).await?;

        let pkce = if spec.pkce {
            let client_secret = self.resolve_org_secret(&spec.client_secret_ref).await?;
            Some(pkce_challenge(&derive_pkce_verifier(
                &client_secret,
                flow_id,
            )))
        } else {
            None
        };
        let mut url = reqwest::Url::parse(&spec.authorize_url)
            .map_err(|e| OAuthServiceError::BadRequest(format!("invalid authorize_url: {e}")))?;
        {
            let mut q = url.query_pairs_mut();
            q.append_pair("client_id", &client_id);
            q.append_pair("redirect_uri", redirect_uri);
            q.append_pair("response_type", "code");
            q.append_pair("state", &flow_id.to_string());
            if !spec.scopes.is_empty() {
                q.append_pair(spec.scopes_param(), &spec.scopes.join(spec.delimiter()));
            }
            for (k, v) in &spec.extra_authorize_params {
                q.append_pair(k, v);
            }
            if let Some(challenge) = &pkce {
                q.append_pair("code_challenge", challenge);
                q.append_pair("code_challenge_method", "S256");
            }
        }
        Ok(BegunRedirectFlow {
            flow,
            authorize_url: url.into(),
        })
    }

    /// Complete a redirect flow on any replica: validate the flow row,
    /// exchange the code, extract declarative metadata, seal + publish the
    /// bundle, and finish the flow. The token never returns to the caller.
    pub async fn complete_redirect(
        &self,
        key: OAuthCredentialKey,
        flow_id: uuid::Uuid,
        code: &str,
        redirect_uri: &str,
        spec: &RedirectOauthSpec,
    ) -> Result<SealedOAuthCredential, OAuthServiceError> {
        validate_key(&key)?;
        spec.validate().map_err(OAuthServiceError::BadRequest)?;
        if code.trim().is_empty() {
            return Err(OAuthServiceError::BadRequest(
                "authorization code is required".into(),
            ));
        }
        let flow = self
            .meta
            .get_oauth_flow(flow_id)
            .await?
            .ok_or(OAuthServiceError::NotFound)?;
        if flow.key != key {
            return Err(OAuthServiceError::NotFound);
        }
        let now = self.clock.now_utc();
        if flow.status.is_terminal() || flow.expires_at <= now {
            return Err(OAuthServiceError::BadRequest(
                "the connection attempt expired or was already completed; start over".into(),
            ));
        }

        let outcome = self
            .exchange_and_publish(&key, flow_id, code, redirect_uri, spec)
            .await;
        match &outcome {
            Ok(_) => {
                if let Err(error) = self
                    .meta
                    .finish_oauth_flow_unowned(flow_id, OAuthFlowStatus::Succeeded, None)
                    .await
                {
                    // The credential is published; a lost finish race only
                    // affects flow-status reporting.
                    tracing::warn!(flow_id = %flow_id, error = %error, "redirect flow finish lost a race");
                }
            }
            Err(error) => {
                let _ = self
                    .meta
                    .finish_oauth_flow_unowned(flow_id, OAuthFlowStatus::Failed, Some(error.code()))
                    .await;
            }
        }
        outcome
    }

    async fn exchange_and_publish(
        &self,
        key: &OAuthCredentialKey,
        flow_id: uuid::Uuid,
        code: &str,
        redirect_uri: &str,
        spec: &RedirectOauthSpec,
    ) -> Result<SealedOAuthCredential, OAuthServiceError> {
        let client_id = self.resolve_org_secret(&spec.client_id_ref).await?;
        let client_secret = self.resolve_org_secret(&spec.client_secret_ref).await?;

        let mut form: Vec<(&str, String)> = vec![
            ("grant_type", "authorization_code".into()),
            ("code", code.into()),
            ("redirect_uri", redirect_uri.into()),
            ("client_id", client_id),
            ("client_secret", client_secret.clone()),
        ];
        if spec.pkce {
            form.push((
                "code_verifier",
                derive_pkce_verifier(&client_secret, flow_id),
            ));
        }
        let grant = self
            .token_grant(&spec.token_url, &form, spec.delimiter(), &spec.grant_path)
            .await?;

        let mut fields = BTreeMap::new();
        extract_mapped(&spec.metadata.from_token_response, &grant.json, &mut fields);
        if let Some(probe) = &spec.metadata.probe {
            let json = self.run_probe(probe, &grant.access_token).await?;
            extract_mapped(&probe.map, &json, &mut fields);
        }
        let metadata = OAuthAccountMetadata {
            // Unmapped account id degrades account-switch detection to the
            // subject itself; shipped facets always map it.
            account_id: fields
                .get(META_ACCOUNT_ID)
                .cloned()
                .unwrap_or_else(|| key.subject_id.clone()),
            display_name: fields.get(META_DISPLAY_NAME).cloned(),
            plan_type: None,
            workspace_id: fields.get(META_WORKSPACE_ID).cloned(),
            workspace_name: fields.get(META_WORKSPACE_NAME).cloned(),
        };

        let refresh = grant
            .refresh_token
            .as_ref()
            .map(|_| ConnectorOAuthRefreshSpec {
                token_url: spec.token_url.clone(),
                client_id_ref: spec.client_id_ref.clone(),
                client_secret_ref: spec.client_secret_ref.clone(),
            });
        let bundle = ConnectorOAuthBundle {
            v: CONNECTOR_OAUTH_BUNDLE_VERSION,
            kind: ConnectorOAuthKind::Oauth2AuthorizationCode,
            access_token: grant.access_token,
            token_type: grant.token_type,
            refresh_token: grant.refresh_token,
            scope: grant.scope,
            obtained_at: self.clock.now_utc(),
            expires_at: grant.expires_at,
            refresh,
        };
        let payload = bundle
            .to_json()
            .map_err(|_| OAuthServiceError::InvalidBundle)?;
        self.publish_bundle(
            key,
            ValidatedOAuthBundle {
                payload,
                metadata,
                expires_at: bundle.expires_at,
            },
        )
        .await?;
        self.meta
            .get_oauth_credential(key)
            .await?
            .ok_or(OAuthServiceError::NotFound)
    }

    /// POST a form grant to the token endpoint and parse the RFC 6749
    /// response shape, tolerating the common deviations: HTTP-200 error
    /// bodies (Slack `{"ok":false}` — caught by the missing token), and
    /// array `scope` (older Linear apps).
    /// `grant_path` selects the object the grant fields are read from
    /// (ADR 0115: Slack's user token lives under `authed_user`); empty means
    /// the response root. Metadata dot-paths always see the full response.
    pub(crate) async fn token_grant(
        &self,
        token_url: &str,
        form: &[(&str, String)],
        scope_delimiter: &str,
        grant_path: &str,
    ) -> Result<TokenGrant, OAuthServiceError> {
        let http = reqwest::Client::builder()
            .user_agent("engram-connector-oauth")
            .timeout(EXCHANGE_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| driver_err("http_client", e.to_string()))?;
        let resp = http
            .post(token_url)
            .form(form)
            .send()
            .await
            .map_err(|e| driver_err("exchange_failed", format!("token request failed: {e}")))?;
        let status = resp.status().as_u16();
        let json: Value = resp.json().await.map_err(|_| {
            driver_err(
                "exchange_failed",
                format!("token response was not JSON (HTTP {status})"),
            )
        })?;
        // The grant object: the root, or the spec's dot-path into it. A
        // missing path reads as "no token", so the error below names it.
        let grant = if grant_path.is_empty() {
            Some(&json)
        } else {
            grant_path
                .split('.')
                .try_fold(&json, |value, seg| value.get(seg))
        };
        let access_token = grant
            .and_then(|g| g.get("access_token"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        if access_token.is_empty() {
            let err = json
                .get("error")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
                .unwrap_or_else(|| {
                    if grant_path.is_empty() {
                        "no access token in response".to_owned()
                    } else {
                        format!("no access token under {grant_path:?}")
                    }
                });
            let code = if err == "invalid_grant" {
                "invalid_grant"
            } else {
                "exchange_failed"
            };
            return Err(driver_err(
                code,
                format!("token grant rejected (HTTP {status}): {err}"),
            ));
        }
        let grant = grant.expect("grant object exists when a token was read");
        let token_type = grant
            .get("token_type")
            .and_then(Value::as_str)
            .unwrap_or("bearer")
            .to_ascii_lowercase();
        let refresh_token = grant
            .get("refresh_token")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(ToOwned::to_owned);
        let scope = normalize_scope(grant.get("scope"), scope_delimiter);
        let expires_at = grant
            .get("expires_in")
            .and_then(|v| {
                v.as_i64()
                    .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
            })
            .filter(|secs| *secs > 0)
            .map(|secs| self.clock.now_utc() + chrono::Duration::seconds(secs));
        Ok(TokenGrant {
            access_token,
            token_type,
            refresh_token,
            scope,
            expires_at,
            json,
        })
    }

    /// One bounded identity probe with the fresh token. A failing probe
    /// fails the completion: if the provider cannot answer "who am I", the
    /// token is not usable and retrying the flow is cheap.
    async fn run_probe(
        &self,
        probe: &RedirectMetadataProbe,
        access_token: &str,
    ) -> Result<Value, OAuthServiceError> {
        let http = reqwest::Client::builder()
            .user_agent("engram-connector-oauth")
            .timeout(EXCHANGE_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| driver_err("http_client", e.to_string()))?;
        let url = probe_url(&probe.host, &probe.path);
        let mut builder = match probe.method.as_str() {
            "POST" => http.post(&url),
            _ => http.get(&url),
        };
        builder = builder.header("Authorization", format!("Bearer {access_token}"));
        if probe.method == "POST" && !probe.body.is_empty() {
            builder = builder
                .header("Content-Type", "application/json")
                .body(probe.body.clone());
        }
        let resp = builder
            .send()
            .await
            .map_err(|e| driver_err("probe_failed", format!("metadata probe failed: {e}")))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(driver_err(
                "probe_failed",
                format!("metadata probe returned HTTP {status}"),
            ));
        }
        let body = resp
            .bytes()
            .await
            .map_err(|e| driver_err("probe_failed", format!("metadata probe read failed: {e}")))?;
        if body.len() > MAX_PROBE_RESPONSE {
            return Err(driver_err(
                "probe_failed",
                "metadata probe response too large",
            ));
        }
        serde_json::from_slice(&body)
            .map_err(|_| driver_err("probe_failed", "metadata probe response was not JSON"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_validation_rejects_bad_shapes() {
        let base = RedirectOauthSpec {
            authorize_url: "https://linear.app/oauth/authorize".into(),
            token_url: "https://api.linear.app/oauth/token".into(),
            scopes: vec!["read".into(), "write".into()],
            scope_delimiter: String::new(),
            extra_authorize_params: BTreeMap::from([("actor".to_string(), "app".to_string())]),
            client_id_ref: "linear.client_id".into(),
            client_secret_ref: "linear.client_secret".into(),
            pkce: false,
            metadata: RedirectMetadataSpec::default(),
            scopes_param: String::new(),
            grant_path: String::new(),
        };
        assert!(base.validate().is_ok());

        let mut user_scope = base.clone();
        user_scope.scopes_param = "user_scope".into();
        user_scope.grant_path = "authed_user".into();
        assert!(user_scope.validate().is_ok());

        let mut bad_param = base.clone();
        bad_param.scopes_param = "User Scope".into();
        assert!(bad_param.validate().is_err());

        let mut bad_path = base.clone();
        bad_path.grant_path = "authed user.token".into();
        assert!(bad_path.validate().is_err());

        let mut http_url = base.clone();
        http_url.authorize_url = "http://linear.app/oauth/authorize".into();
        assert!(http_url.validate().is_err());

        let mut query_url = base.clone();
        query_url.authorize_url = "https://linear.app/oauth/authorize?x=1".into();
        assert!(query_url.validate().is_err());

        let mut reserved = base.clone();
        reserved
            .extra_authorize_params
            .insert("redirect_uri".into(), "https://evil".into());
        assert!(reserved.validate().is_err());

        let mut spacey_scope = base.clone();
        spacey_scope.scopes = vec!["read write".into()];
        assert!(spacey_scope.validate().is_err());

        let mut bad_field = base.clone();
        bad_field
            .metadata
            .from_token_response
            .insert("proto".into(), "team.id".into());
        assert!(bad_field.validate().is_err());

        let mut proto_path = base.clone();
        proto_path
            .metadata
            .from_token_response
            .insert(META_ACCOUNT_ID.into(), "__proto__.x".into());
        // `__proto__` is charset-legal for a JSON key; the walker treats it
        // as data (serde_json has no prototype chain). It must simply parse.
        assert!(proto_path.validate().is_ok());

        let mut deep_path = base.clone();
        deep_path
            .metadata
            .from_token_response
            .insert(META_ACCOUNT_ID.into(), "a.b.c.d.e.f.g.h.i".into());
        assert!(deep_path.validate().is_err());

        let mut bad_probe = base.clone();
        bad_probe.metadata.probe = Some(RedirectMetadataProbe {
            method: "PUT".into(),
            host: "api.linear.app".into(),
            path: "/graphql".into(),
            body: String::new(),
            map: BTreeMap::from([(META_ACCOUNT_ID.to_string(), "data.viewer.id".to_string())]),
        });
        assert!(bad_probe.validate().is_err());
    }

    #[test]
    fn dot_path_walks_objects_only() {
        let json = serde_json::json!({"team": {"id": "T1", "num": 7}, "arr": [1]});
        assert_eq!(
            dot_path(&json, "team.id")
                .and_then(value_to_string)
                .as_deref(),
            Some("T1")
        );
        assert_eq!(
            dot_path(&json, "team.num")
                .and_then(value_to_string)
                .as_deref(),
            Some("7")
        );
        assert!(dot_path(&json, "arr.0").is_none());
        assert!(dot_path(&json, "missing.x").is_none());
    }

    #[test]
    fn pkce_derivation_is_stable_and_secret_dependent() {
        let flow = uuid::Uuid::parse_str("10600000-0000-4000-8000-000000000021").unwrap();
        let a = derive_pkce_verifier("secret-a", flow);
        assert_eq!(a, derive_pkce_verifier("secret-a", flow));
        assert_ne!(a, derive_pkce_verifier("secret-b", flow));
        // RFC 7636: verifier must be 43-128 chars of the unreserved set.
        assert!((43..=128).contains(&a.len()), "{}", a.len());
        assert!(!pkce_challenge(&a).contains('='));
    }
}
