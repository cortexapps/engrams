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

/// One host's load + budget from `GET /api/hosts` (the `HostView` fields the
/// scale-down wave planner needs). serde ignores the rest of the payload.
/// ADR 0048: the budget fields are recent — `#[serde(default)]` keeps the
/// operator tolerant of a pre-0048 coordinator (the wave just sees 0 free,
/// which the 2D guard treats as "can't absorb", i.e. it won't shed — safe).
#[derive(Clone, Debug, Deserialize)]
pub struct HostLoad {
    pub id: HostId,
    pub cordoned: bool,
    pub running_sandboxes: u32,
    #[serde(default)]
    pub reserved_mib: u64,
    #[serde(default)]
    pub free_mib: u64,
    #[serde(default)]
    pub reserved_vcpus: u64,
    #[serde(default)]
    pub free_vcpus: u64,
}

/// The coordinator API is versioned under this prefix — `api/mod.rs` nests the
/// whole router at `/api/v1`. Centralised here (not at each call site) so a
/// path typo can't silently 404; the `urls_are_v1_prefixed` test guards it.
const API_V1: &str = "/api/v1";

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

    /// `<base>/api/v1<suffix>`.
    fn url(&self, suffix: &str) -> String {
        format!("{}{API_V1}{suffix}", self.base)
    }

    async fn post(&self, op: &'static str, suffix: &str) -> Result<(), OperatorError> {
        let resp = self
            .with_auth(self.http.post(self.url(suffix)))
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
        self.post("cordon", &format!("/admin/hosts/{host}/cordon"))
            .await
    }

    /// `POST /api/v1/admin/hosts/:id/uncordon`.
    pub async fn uncordon(&self, host: HostId) -> Result<(), OperatorError> {
        self.post("uncordon", &format!("/admin/hosts/{host}/uncordon"))
            .await
    }

    /// `POST /api/admin/hosts/:id/drain` — evacuate every active session
    /// (Evacuating = snapshot + warm-restore on a peer). Returns 202; the
    /// actual progress is observed via [`Self::host_status`].
    ///
    /// The node-removal drain path (ADR 0045 Phase E / ADR 0048 scale-down).
    /// Image rolls reattach and never drain (`reconcile::roll_node`); the
    /// caller is the wave executor (`autoscale`).
    pub async fn drain(&self, host: HostId) -> Result<(), OperatorError> {
        self.post("drain", &format!("/admin/hosts/{host}/drain"))
            .await
    }

    /// `DELETE /api/v1/admin/hosts/:id` (ADR 0048) — deregister a drained host
    /// immediately after `remove_node`, so its row doesn't linger to the
    /// dead-host TTL. The coordinator 409s if any session is still bound, so
    /// the wave only calls this after the drain gate reports 0 sandboxes.
    pub async fn delete_host(&self, host: HostId) -> Result<(), OperatorError> {
        let resp = self
            .with_auth(self.http.delete(self.url(&format!("/admin/hosts/{host}"))))
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(OperatorError::Coord {
                op: "delete_host",
                status: status.as_u16(),
                body,
            });
        }
        Ok(())
    }

    /// `GET /api/v1/hosts` (ADR 0048) — every host's load + budget, for the
    /// scale-down wave planner's victim selection + 2D capacity guard.
    pub async fn list_hosts(&self) -> Result<Vec<HostLoad>, OperatorError> {
        let resp = self
            .with_auth(self.http.get(self.url("/hosts")))
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(OperatorError::Coord {
                op: "list_hosts",
                status: status.as_u16(),
                body,
            });
        }
        // `GET /api/hosts` → `{ "hosts": [ HostView, … ] }`.
        #[derive(Deserialize)]
        struct ListResp {
            hosts: Vec<HostLoad>,
        }
        Ok(resp.json::<ListResp>().await?.hosts)
    }

    /// `GET /api/v1/hosts/:id` — the drain gate. `Ok(None)` means the host is
    /// no longer registered (already gone — nothing left to drain).
    pub async fn host_status(&self, host: HostId) -> Result<Option<HostStatus>, OperatorError> {
        let resp = self
            .with_auth(self.http.get(self.url(&format!("/hosts/{host}"))))
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

    /// `GET /api/v1/admin/fleet/demand` — the K4 autoscaler's input (schedulable
    /// hosts + free/total guest-RAM reservation).
    pub async fn fleet_demand(&self) -> Result<crate::scaler::FleetDemand, OperatorError> {
        let resp = self
            .with_auth(self.http.get(self.url("/admin/fleet/demand")))
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

#[cfg(test)]
mod tests {
    use super::*;

    // The coordinator API is nested at `/api/v1` (api/mod.rs). This pins every
    // path the operator builds to that prefix — the regression that a mock
    // coordinator (which accepts any path) silently let through, caught only by
    // the live K5 cutover.
    #[test]
    fn urls_are_v1_prefixed() {
        let c = CoordClient::new("http://coord:8080/".into(), None);
        assert_eq!(
            c.url("/admin/hosts/h1/cordon"),
            "http://coord:8080/api/v1/admin/hosts/h1/cordon"
        );
        assert_eq!(
            c.url("/admin/hosts/h1/drain"),
            "http://coord:8080/api/v1/admin/hosts/h1/drain"
        );
        assert_eq!(c.url("/hosts/h1"), "http://coord:8080/api/v1/hosts/h1");
        assert_eq!(c.url("/hosts"), "http://coord:8080/api/v1/hosts");
        assert_eq!(
            c.url("/admin/hosts/h1"),
            "http://coord:8080/api/v1/admin/hosts/h1"
        );
        assert_eq!(
            c.url("/admin/fleet/demand"),
            "http://coord:8080/api/v1/admin/fleet/demand"
        );
    }
}
