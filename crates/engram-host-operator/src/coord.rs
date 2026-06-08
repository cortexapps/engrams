//! ADR 0044 K3: thin client for the coordinator admin API the operator
//! drives during a roll — cordon → drain → poll the drain gate → uncordon.
//!
//! Auth: the admin routes sit behind `require_admin` (ADR 0031). In a synthetic
//! -admin dev coord (`AuthMode::None`) no token is needed; in prod the operator
//! carries a service bearer token via `ENGRAM_COORDINATOR_TOKEN`.

use engram_core::HostId;
use serde::Deserialize;

use crate::error::OperatorError;

/// The `HostView` fields (GET /api/hosts/:id) the drain gate needs. serde
/// ignores the rest of the payload.
#[derive(Debug, Deserialize)]
pub struct HostStatus {
    /// `"ready"` | `"draining"` | … — the coordinator's view of the host.
    #[allow(dead_code)]
    pub status: String,
    /// Live sandbox count from the host's latest heartbeat. The gate waits
    /// for this to reach 0.
    pub running_sandboxes: u32,
}

pub struct CoordClient {
    base: String,
    http: reqwest::Client,
    token: Option<String>,
}

impl CoordClient {
    pub fn new(base: String, token: Option<String>) -> Self {
        Self {
            base: base.trim_end_matches('/').to_string(),
            http: reqwest::Client::new(),
            token,
        }
    }

    fn with_auth(&self, rb: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.token {
            Some(t) => rb.bearer_auth(t),
            None => rb,
        }
    }

    async fn post(&self, op: &'static str, path: String) -> Result<(), OperatorError> {
        let resp = self
            .with_auth(self.http.post(format!("{}{path}", self.base)))
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(OperatorError::Coord {
                op,
                status: status.as_u16(),
                body,
            });
        }
        Ok(())
    }

    /// `POST /api/admin/hosts/:id/cordon` — stop the picker placing new
    /// sessions on the host.
    pub async fn cordon(&self, host: HostId) -> Result<(), OperatorError> {
        self.post("cordon", format!("/api/admin/hosts/{host}/cordon"))
            .await
    }

    /// `POST /api/admin/hosts/:id/uncordon`.
    pub async fn uncordon(&self, host: HostId) -> Result<(), OperatorError> {
        self.post("uncordon", format!("/api/admin/hosts/{host}/uncordon"))
            .await
    }

    /// `POST /api/admin/hosts/:id/drain` — evacuate every active session
    /// (Evacuating = snapshot + warm-restore on a peer). Returns 202; the
    /// actual progress is observed via [`Self::host_status`].
    pub async fn drain(&self, host: HostId) -> Result<(), OperatorError> {
        self.post("drain", format!("/api/admin/hosts/{host}/drain"))
            .await
    }

    /// `GET /api/hosts/:id` — the drain gate. `Ok(None)` means the host is no
    /// longer registered (already gone — nothing left to drain).
    pub async fn host_status(&self, host: HostId) -> Result<Option<HostStatus>, OperatorError> {
        let resp = self
            .with_auth(self.http.get(format!("{}/api/hosts/{host}", self.base)))
            .send()
            .await?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(OperatorError::Coord {
                op: "host_status",
                status: status.as_u16(),
                body,
            });
        }
        Ok(Some(resp.json().await?))
    }

    /// `GET /api/admin/fleet/demand` — the K4 autoscaler's input (schedulable
    /// hosts + free/total guest-RAM reservation).
    pub async fn fleet_demand(&self) -> Result<crate::scaler::FleetDemand, OperatorError> {
        let resp = self
            .with_auth(
                self.http
                    .get(format!("{}/api/admin/fleet/demand", self.base)),
            )
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(OperatorError::Coord {
                op: "fleet_demand",
                status: status.as_u16(),
                body,
            });
        }
        Ok(resp.json().await?)
    }
}
