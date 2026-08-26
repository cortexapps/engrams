//! The daemon's coordinator-facing glue (ADR 0121).
//!
//! The three coord routes the egress request path depends on — inject
//! refresh, observed-asset forwarding, connection-credential minting —
//! moved here from host-agent so a guest request that needs one never
//! depends on pod liveness (a re-mint during a roll gap would
//! otherwise fail, which is the failure class ADR 0121 deletes).
//!
//! `SlimCoordClient` carries exactly those three routes; the DTOs
//! mirror `engram_coordinator::api::host_http`'s JSON, the same wire
//! contract `engram-host-agent/src/coord_client.rs` spoke before the
//! move. Coord URL + token arrive as spawn-time env (the uffd-handler
//! pattern); a rotation is a config mismatch at adopt time and
//! replaces the daemon.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use engram_core::types::integration::{CredentialPurpose, SessionTunnel};
use engram_core::{HostId, SessionId};
use engram_egress_proxy::{EndpointFactory, InjectRefresher, RefreshedInject, TunnelEndpoint};
use serde::{Deserialize, Serialize};

/// The coord HTTP client, reduced to the egress request path's three
/// routes. One pooled `reqwest::Client`; cheap to clone.
#[derive(Clone)]
pub(crate) struct SlimCoordClient {
    http: reqwest::Client,
    base_url: String,
    auth_token: String,
}

#[derive(Debug)]
pub(crate) enum CoordError {
    Transport(String),
    Http { status: u16, body: String },
    Decode(String),
}

impl std::fmt::Display for CoordError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Transport(e) => write!(f, "transport: {e}"),
            Self::Http { status, body } => write!(f, "http {status}: {body}"),
            Self::Decode(e) => write!(f, "decode: {e}"),
        }
    }
}

impl std::error::Error for CoordError {}

#[derive(Serialize)]
struct RefreshInjectRequest {
    mint_source: engram_core::types::integration::CredentialMintSource,
    purpose: CredentialPurpose,
}

/// WS4: the coord's re-minted inject credential — the fresh rendered
/// header value the proxy substitutes, plus its new expiry.
#[derive(Deserialize)]
pub(crate) struct RefreshInjectResponse {
    #[allow(dead_code)] // part of the coord's reply shape; the proxy substitutes by entry
    pub header_name: String,
    pub secret: String,
    pub expires_at: DateTime<Utc>,
}

#[derive(Serialize)]
pub(crate) struct IntegrationAssetReport {
    pub provider: String,
    pub asset_kind: String,
    pub surface: String,
    pub data: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fetchable_url: Option<String>,
    pub at: DateTime<Utc>,
}

impl SlimCoordClient {
    pub(crate) fn new(coord_url: String, auth_token: Option<String>) -> Self {
        let http = engram_tls::client_builder()
            // The coord LB's back-end pods roll on every deploy; a
            // short pool idle keeps this low-rate client off stale
            // connections (the coord_client.rs 2026-05-23 lesson).
            .pool_idle_timeout(Some(std::time::Duration::from_secs(10)))
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .expect("reqwest client builder must not fail with default config");
        Self {
            http,
            base_url: coord_url.trim_end_matches('/').to_string(),
            auth_token: auth_token.unwrap_or_default(),
        }
    }

    fn endpoint(&self, path: &str) -> String {
        format!("{}/api/v1{}", self.base_url, path)
    }

    async fn post_json<Req: Serialize, Resp: serde::de::DeserializeOwned>(
        &self,
        url: String,
        body: &Req,
    ) -> Result<Resp, CoordError> {
        let mut builder = self.http.post(&url).json(body);
        if !self.auth_token.is_empty() {
            builder = builder.bearer_auth(&self.auth_token);
        }
        let resp = builder
            .send()
            .await
            .map_err(|e| CoordError::Transport(e.to_string()))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(CoordError::Http {
                status: status.as_u16(),
                body: resp.text().await.unwrap_or_default(),
            });
        }
        resp.json()
            .await
            .map_err(|e| CoordError::Decode(e.to_string()))
    }

    async fn post_ack<Req: Serialize>(&self, url: String, body: &Req) -> Result<(), CoordError> {
        let mut builder = self.http.post(&url).json(body);
        if !self.auth_token.is_empty() {
            builder = builder.bearer_auth(&self.auth_token);
        }
        let resp = builder
            .send()
            .await
            .map_err(|e| CoordError::Transport(e.to_string()))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(CoordError::Http {
                status: status.as_u16(),
                body: resp.text().await.unwrap_or_default(),
            });
        }
        Ok(())
    }

    /// POST /api/v1/hosts/:id/sessions/:session_id/inject/refresh —
    /// re-mint a near-expiry inject credential (WS4). The proxy
    /// awaits this on the request path.
    pub(crate) async fn refresh_inject(
        &self,
        host_id: HostId,
        session_id: SessionId,
        mint_source: &engram_core::types::integration::CredentialMintSource,
    ) -> Result<RefreshInjectResponse, CoordError> {
        let url = self.endpoint(&format!(
            "/hosts/{host_id}/sessions/{session_id}/inject/refresh"
        ));
        let req = RefreshInjectRequest {
            mint_source: mint_source.clone(),
            purpose: CredentialPurpose::api(),
        };
        self.post_json(url, &req).await
    }

    /// Same route, arbitrary purpose: the Cloud SQL connection
    /// credentials (`cloud_sql_admin` / `cloud_sql_login`).
    pub(crate) async fn mint_connection_credential(
        &self,
        host_id: HostId,
        session_id: SessionId,
        mint_source: &engram_core::types::integration::CredentialMintSource,
        purpose: CredentialPurpose,
    ) -> Result<RefreshInjectResponse, CoordError> {
        let url = self.endpoint(&format!(
            "/hosts/{host_id}/sessions/{session_id}/inject/refresh"
        ));
        let req = RefreshInjectRequest {
            mint_source: mint_source.clone(),
            purpose,
        };
        self.post_json(url, &req).await
    }

    /// POST /api/v1/sessions/:session_id/integration-asset — forward
    /// an observed asset (ADR 0056 Phase 4). Best-effort.
    pub(crate) async fn integration_asset(
        &self,
        session_id: SessionId,
        req: &IntegrationAssetReport,
    ) -> Result<(), CoordError> {
        let url = self.endpoint(&format!("/sessions/{session_id}/integration-asset"));
        self.post_ack(url, req).await
    }
}

/// WS4: bridges the proxy's near-expiry re-mint request to the coord's
/// inject-refresh route (which holds the mint authority). The proxy
/// AWAITS this before injecting a stale minted credential.
pub(crate) struct CoordInjectRefresher {
    coord: SlimCoordClient,
    host_id: HostId,
}

impl CoordInjectRefresher {
    pub(crate) fn new(coord: SlimCoordClient, host_id: HostId) -> Self {
        Self { coord, host_id }
    }
}

#[async_trait]
impl InjectRefresher for CoordInjectRefresher {
    async fn refresh(
        &self,
        session_id: SessionId,
        mint_source: &engram_core::types::integration::CredentialMintSource,
    ) -> Option<RefreshedInject> {
        match self
            .coord
            .refresh_inject(self.host_id, session_id, mint_source)
            .await
        {
            Ok(resp) => Some(RefreshedInject {
                secret: resp.secret,
                expires_at: resp.expires_at,
            }),
            Err(e) => {
                // The proxy keeps the stale secret on `None` — a stale token 401s
                // (recoverable), and a coord blip must not drop the guest's request.
                tracing::warn!(%session_id, source = ?mint_source, error = %e, "egress inject re-mint via coord failed");
                None
            }
        }
    }
}

/// ADR 0056 Phase 4: the observed-asset sink — forwards each
/// proxy-built IntegrationAsset to the coord. Best-effort
/// fire-and-forget: spawn the POST, log on failure.
pub(crate) fn observe_sink(coord: SlimCoordClient) -> engram_egress_proxy::ObserveSink {
    Arc::new(move |session_id, asset| {
        let coord = coord.clone();
        tokio::spawn(async move {
            let req = IntegrationAssetReport {
                provider: asset.provider,
                asset_kind: asset.asset_kind,
                surface: asset.surface,
                data: serde_json::Value::Object(asset.data),
                fetchable_url: asset.fetchable_url,
                at: Utc::now(),
            };
            if let Err(e) = coord.integration_asset(session_id, &req).await {
                tracing::debug!(%session_id, error = %e, "forward integration asset to coord failed");
            }
        });
    })
}

/// Builds pooled Cloud SQL endpoints: mint the two connection
/// credentials via the coordinator, then run the native connect protocol
/// (`engram-cloud-sql`). No child process is involved; an endpoint is an
/// ephemeral client certificate plus a TLS config, and every guest
/// connection through it is one TLS stream to the instance.
pub(crate) struct CloudSqlEndpointFactory {
    coord: SlimCoordClient,
    host_id: HostId,
    http: reqwest::Client,
}

const CLOUD_SQL_CONNECTOR_KIND: &str = "gcp.cloud_sql";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CloudSqlTunnelConfig {
    instance: String,
    database_user: String,
}

impl CloudSqlEndpointFactory {
    pub(crate) fn new(coord: SlimCoordClient, host_id: HostId) -> Self {
        Self {
            coord,
            host_id,
            // The sqladmin calls get their own bounded client so one stuck
            // request cannot wedge an endpoint build past the pool's
            // stale-serve window.
            http: engram_tls::client_builder()
                .timeout(std::time::Duration::from_secs(15))
                .build()
                .expect("the sqladmin client builds from static settings"),
        }
    }

    async fn token(
        &self,
        session_id: SessionId,
        tunnel: &SessionTunnel,
        purpose: CredentialPurpose,
    ) -> std::io::Result<(String, DateTime<Utc>)> {
        let mint_source = tunnel.mint_source.as_ref().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "Cloud SQL tunnel has no credential mint source",
            )
        })?;
        self.coord
            .mint_connection_credential(self.host_id, session_id, mint_source, purpose)
            .await
            .map(|response| (response.secret, response.expires_at))
            .map_err(|error| std::io::Error::other(error.to_string()))
    }
}

/// One pooled endpoint: the crate's immutable endpoint plus the
/// credential-derived staleness deadline the tunnel pool rotates on.
pub(crate) struct CloudSqlEndpoint {
    inner: engram_cloud_sql::CloudSqlEndpoint,
    stale_after: DateTime<Utc>,
}

#[async_trait]
impl TunnelEndpoint for CloudSqlEndpoint {
    type Conn = engram_cloud_sql::CloudSqlStream;

    async fn connect(&self) -> std::io::Result<Self::Conn> {
        self.inner.connect().await
    }

    fn stale_after(&self) -> Option<DateTime<Utc>> {
        Some(self.stale_after)
    }
}

#[async_trait]
impl EndpointFactory for CloudSqlEndpointFactory {
    type Endpoint = CloudSqlEndpoint;

    fn kind(&self) -> &'static str {
        CLOUD_SQL_CONNECTOR_KIND
    }

    async fn spawn_endpoint(
        &self,
        session_id: SessionId,
        tunnel: &SessionTunnel,
    ) -> std::io::Result<Self::Endpoint> {
        let config: CloudSqlTunnelConfig = serde_json::from_str(&tunnel.config_json)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        let instance = engram_cloud_sql::InstanceName::parse(&config.instance)?;
        let mint_start = std::time::Instant::now();
        let ((api_token, _api_expires), (login_token, login_expires)) = tokio::try_join!(
            self.token(
                session_id,
                tunnel,
                CredentialPurpose::new("cloud_sql_admin")
            ),
            self.token(
                session_id,
                tunnel,
                CredentialPurpose::new("cloud_sql_login")
            ),
        )?;
        let mint_ms = mint_start.elapsed().as_millis() as u64;

        let build_start = std::time::Instant::now();
        let endpoint = engram_cloud_sql::build_endpoint(engram_cloud_sql::EndpointRequest {
            http: &self.http,
            api_base: None,
            instance: &instance,
            admin_token: &api_token,
            login_token: &login_token,
            server_proxy_port_override: None,
        })
        .await?;
        let build_ms = build_start.elapsed().as_millis() as u64;

        // The ephemeral certificate IS the login credential and Google caps
        // its NotAfter at the login token's expiry; the minimum keeps the
        // deadline honest if that capping ever changes. The admin token only
        // mattered during the build.
        let stale_after = endpoint.cert_not_after().min(login_expires);
        tracing::info!(
            %session_id,
            tunnel_id = %tunnel.id,
            instance = %config.instance,
            database_user = %config.database_user,
            mint_ms,
            build_ms,
            %stale_after,
            "built cloud sql tunnel endpoint",
        );
        Ok(CloudSqlEndpoint {
            inner: endpoint,
            stale_after,
        })
    }
}
