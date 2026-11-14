//! Worker protocol 1. Operations are fixed; no provider HTTP proxy is exposed.
use super::{McpError, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::time::Duration;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkerIdentity {
    pub protocol: u32,
    pub boot_id: String,
    pub zone_id: String,
    pub policy_digest: String,
    pub policy_revision: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
pub enum WorkerScope {
    #[serde(rename_all = "camelCase")]
    Administration {
        request_id: String,
        actor_id: String,
    },
    #[serde(rename_all = "camelCase")]
    Execution {
        session_id: String,
        run_id: String,
        tool_call_id: String,
        binding_hash: String,
    },
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkerLease {
    pub id: String,
    pub deployment_id: String,
    pub expires_at: i64,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkerIntent {
    pub id: String,
    pub boot_id: String,
    pub zone_id: String,
    pub policy_digest: String,
    pub connection_id: String,
    pub connection_revision: i64,
    pub credential_identity: String,
    pub scope: WorkerScope,
    pub operation: String,
    pub destination: String,
    pub request_digest: String,
    pub operation_digest: String,
    pub expires_at: i64,
    pub lease: WorkerLease,
}
// These types intentionally have no Debug implementation: token fields are private.
#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum WorkerOperation {
    Metadata { endpoint: String },
    ValidateDestination { endpoint: String },
    Token { endpoint: String, grant: TokenGrant },
}
#[derive(Serialize)]
#[serde(tag = "grant_type")]
pub enum TokenGrant {
    #[serde(rename = "authorization_code")]
    AuthorizationCode {
        code: String,
        redirect_uri: String,
        code_verifier: String,
        client_id: String,
        resource: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        client_secret: Option<String>,
    },
    #[serde(rename = "refresh_token")]
    RefreshToken {
        refresh_token: String,
        client_id: String,
        resource: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        client_secret: Option<String>,
    },
}
impl WorkerOperation {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Metadata { .. } => "metadata",
            Self::ValidateDestination { .. } => "validateDestination",
            Self::Token { .. } => "token",
        }
    }
    pub fn endpoint(&self) -> &str {
        match self {
            Self::Metadata { endpoint }
            | Self::ValidateDestination { endpoint }
            | Self::Token { endpoint, .. } => endpoint,
        }
    }
}

/// This encoder is deliberately limited to the fixed coordinator wire: strings,
/// booleans, null and safe integers. No floating point enters these requests.
/// Keys follow JavaScript UTF-16 ordering, including non-BMP names in vectors.
pub fn canonical(value: &Value) -> Result<String> {
    Ok(match value {
        Value::Null => "null".into(),
        Value::Bool(v) => v.to_string(),
        Value::String(v) => serde_json::to_string(v).map_err(|_| McpError::Invalid)?,
        Value::Number(v) => {
            let n = v.as_i64().ok_or(McpError::Invalid)?;
            if !(-9_007_199_254_740_991..=9_007_199_254_740_991).contains(&n) {
                return Err(McpError::Invalid);
            }
            n.to_string()
        }
        Value::Array(v) => format!(
            "[{}]",
            v.iter()
                .map(canonical)
                .collect::<Result<Vec<_>>>()?
                .join(",")
        ),
        Value::Object(v) => {
            let mut keys: Vec<_> = v.keys().collect();
            keys.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
            let fields = keys
                .into_iter()
                .map(|k| {
                    Ok(format!(
                        "{}:{}",
                        serde_json::to_string(k).map_err(|_| McpError::Invalid)?,
                        canonical(&v[k])?
                    ))
                })
                .collect::<Result<Vec<_>>>()?;
            format!("{{{}}}", fields.join(","))
        }
    })
}
pub fn digest(value: &Value) -> Result<String> {
    Ok(hex::encode(Sha256::digest(canonical(value)?.as_bytes())))
}

#[async_trait]
pub trait WorkerTransport: Send + Sync {
    async fn identity(&self) -> Result<WorkerIdentity>;
    async fn rpc(
        &self,
        identity: &WorkerIdentity,
        intent: &WorkerIntent,
        operation: &WorkerOperation,
    ) -> Result<Value>;
    async fn check_identity(&self, expected: &WorkerIdentity) -> Result<()> {
        if &self.identity().await? != expected {
            return Err(McpError::Stale);
        }
        Ok(())
    }
}
pub struct WorkerClient {
    client: reqwest::Client,
    base: String,
}
impl WorkerClient {
    pub fn new(base: &str, ca: &[u8], identity: &[u8]) -> Result<Self> {
        let url = url::Url::parse(base).map_err(|_| McpError::Invalid)?;
        if url.scheme() != "https"
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || url.path() != "/"
        {
            return Err(McpError::Invalid);
        }
        let certs = reqwest::Certificate::from_pem_bundle(ca).map_err(|_| McpError::Invalid)?;
        if certs.is_empty() {
            return Err(McpError::Invalid);
        }
        let client = engram_tls::client_builder()
            .tls_certs_only(certs)
            .identity(reqwest::Identity::from_pem(identity).map_err(|_| McpError::Invalid)?)
            .tls_version_min(reqwest::tls::Version::TLS_1_3)
            .https_only(true)
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(10))
            .tls_sslkeylogfile(false)
            .build()
            .map_err(|_| McpError::Transport)?;
        Ok(Self {
            client,
            base: base.trim_end_matches('/').into(),
        })
    }
    async fn json(response: reqwest::Response) -> Result<Value> {
        if !response.status().is_success() {
            return Err(McpError::Transport);
        }
        let mut response = response;
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| McpError::Transport)? {
            if bytes.len() + chunk.len() > 2 * 1024 * 1024 {
                return Err(McpError::Invalid);
            }
            bytes.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&bytes).map_err(|_| McpError::Invalid)
    }
}
#[async_trait]
impl WorkerTransport for WorkerClient {
    async fn identity(&self) -> Result<WorkerIdentity> {
        let v = Self::json(
            self.client
                .get(format!("{}/identity", self.base))
                .send()
                .await
                .map_err(|_| McpError::Transport)?,
        )
        .await?;
        let identity: WorkerIdentity = serde_json::from_value(v).map_err(|_| McpError::Invalid)?;
        if identity.protocol != 1 || identity.boot_id.is_empty() {
            return Err(McpError::Denied);
        }
        Ok(identity)
    }
    async fn rpc(
        &self,
        identity: &WorkerIdentity,
        intent: &WorkerIntent,
        operation: &WorkerOperation,
    ) -> Result<Value> {
        let op = serde_json::to_value(operation).map_err(|_| McpError::Invalid)?;
        if intent.boot_id != identity.boot_id
            || intent.zone_id != identity.zone_id
            || intent.policy_digest != identity.policy_digest
            || intent.operation != operation.kind()
            || intent.destination != operation.endpoint()
            || intent.operation_digest != digest(&op)?
        {
            return Err(McpError::Denied);
        }
        let body = serde_json::json!({"protocol":1,"intent":intent,"operation":op});
        let response = Self::json(
            self.client
                .post(format!("{}/rpc", self.base))
                .json(&body)
                .send()
                .await
                .map_err(|_| McpError::Transport)?,
        )
        .await?;
        if response["ok"] != true
            || response["identity"]
                != serde_json::to_value(identity).map_err(|_| McpError::Invalid)?
            || response["intent"] != body["intent"]
            || response["intentId"] != intent.id
            || response["requestDigest"] != intent.request_digest
            || response["operationDigest"] != intent.operation_digest
            || response["leaseId"] != intent.lease.id
        {
            return Err(McpError::Denied);
        }
        response.get("result").cloned().ok_or(McpError::Invalid)
    }
}
