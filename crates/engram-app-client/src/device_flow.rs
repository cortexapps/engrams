//! RFC 8628 device authorization against the orchestrator's better-auth
//! device plugin: `POST /api/auth/device/code` then poll
//! `POST /api/auth/device/token` until the browser approves the code.

use std::time::{Duration, Instant};

use serde::Deserialize;

use crate::config::DEVICE_CLIENT_ID;

#[derive(Debug, Clone, Deserialize)]
pub struct DeviceGrant {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    #[serde(default)]
    pub verification_uri_complete: String,
    pub expires_in: u64,
    pub interval: u64,
}

impl DeviceGrant {
    /// The URL to hand the human: the complete one when the server gives
    /// it, else the bare verification page.
    pub fn url(&self) -> &str {
        if self.verification_uri_complete.is_empty() {
            &self.verification_uri
        } else {
            &self.verification_uri_complete
        }
    }
}

/// Why `request_grant` did not return a grant.
#[derive(Debug)]
pub enum GrantError {
    /// The host did not answer.
    Unreachable(String),
    /// The host answered, but not with a device grant.
    Rejected { status: u16 },
    /// The body was not a grant.
    Malformed(String),
}

pub async fn request_grant(http: &reqwest::Client, host: &str) -> Result<DeviceGrant, GrantError> {
    let resp = http
        .post(format!("{host}/api/auth/device/code"))
        .header("content-type", "application/json")
        .body(serde_json::json!({ "client_id": DEVICE_CLIENT_ID }).to_string())
        .send()
        .await
        .map_err(|e| GrantError::Unreachable(crate::error::Error::transport(e).message))?;
    let status = resp.status().as_u16();
    if !(200..300).contains(&status) {
        return Err(GrantError::Rejected { status });
    }
    resp.json::<DeviceGrant>()
        .await
        .map_err(|e| GrantError::Malformed(e.to_string()))
}

/// How the poll loop ended without a token.
#[derive(Debug)]
pub enum PollError {
    /// The user declined in the browser.
    Denied,
    /// The code expired before approval (`expired_token` or the local deadline).
    Expired,
    /// An unexpected error field, or a non-OK status with no error field
    /// (carried as `HTTP <status>`).
    Failed(String),
    /// A request failed at the transport.
    Transport(String),
}

/// Poll until the grant is approved. Sleeps `max(1, interval)` seconds before
/// every poll and honours `slow_down` by adding five seconds, exactly as the
/// Bun CLI did.
pub async fn poll_token(
    http: &reqwest::Client,
    host: &str,
    grant: &DeviceGrant,
) -> Result<String, PollError> {
    #[derive(Deserialize)]
    struct TokenOk {
        access_token: String,
    }
    #[derive(Deserialize, Default)]
    struct TokenErr {
        error: Option<String>,
    }
    let deadline = Instant::now() + Duration::from_secs(grant.expires_in);
    let mut interval = Duration::from_secs(grant.interval.max(1));
    let body = serde_json::json!({
        "grant_type": "urn:ietf:params:oauth:grant-type:device_code",
        "device_code": grant.device_code,
        "client_id": DEVICE_CLIENT_ID,
    })
    .to_string();
    while Instant::now() < deadline {
        tokio::time::sleep(interval).await;
        let resp = http
            .post(format!("{host}/api/auth/device/token"))
            .header("content-type", "application/json")
            .body(body.clone())
            .send()
            .await
            .map_err(|e| PollError::Transport(crate::error::Error::transport(e).message))?;
        let status = resp.status().as_u16();
        if (200..300).contains(&status) {
            let ok = resp
                .json::<TokenOk>()
                .await
                .map_err(|e| PollError::Failed(format!("malformed token response: {e}")))?;
            return Ok(ok.access_token);
        }
        let err = resp.json::<TokenErr>().await.unwrap_or_default().error;
        match err.as_deref() {
            Some("authorization_pending") => continue,
            Some("slow_down") => {
                interval += Duration::from_secs(5);
                continue;
            }
            Some("access_denied") => return Err(PollError::Denied),
            Some("expired_token") => return Err(PollError::Expired),
            Some(other) => return Err(PollError::Failed(other.to_string())),
            None => return Err(PollError::Failed(format!("HTTP {status}"))),
        }
    }
    Err(PollError::Expired)
}
