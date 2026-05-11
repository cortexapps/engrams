//! Outbound coordinator dialer. The host-agent's main loop after
//! Phase 3a: dial `coordinator://api/hosts/connect` over WebSocket
//! (with bearer auth), send the [`NotifyKind::Hello`], then drive
//! [`HostSession`] until the connection drops. Exponential backoff
//! between reconnect attempts so a coordinator restart doesn't
//! hammer the box.

use std::sync::Arc;
use std::time::Duration;

use engram_core::traits::SandboxBackend;
use engram_core::HostId;
use engram_protocol::heartbeat::Heartbeat;
use engram_protocol::server::HostSession;
use engram_protocol::wire::NotifyKind;
use engram_protocol::{HostCapacityReport, LocalSnapshotReport, WarmPoolReport};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::header;

/// Callback the dialer invokes on each heartbeat tick. Returns the
/// fresh state the host wants to publish: capacity / warm pools /
/// local snapshots / draining flag. Callers (the HostAgent) close
/// over their `Pool` + future SnapshotIndex so the WS payload always
/// reflects the current state.
pub type HeartbeatProvider = Arc<
    dyn Fn() -> (
            HostCapacityReport,
            Vec<WarmPoolReport>,
            Vec<LocalSnapshotReport>,
            bool,
        ) + Send
        + Sync,
>;

/// Configuration for [`run_dialer`]. Pulled out of HostAgentConfig so
/// it stays self-contained.
pub struct DialerConfig {
    /// Coordinator base URL, e.g. `ws://localhost:8080`. The dialer
    /// appends `/api/hosts/connect`.
    pub coordinator_url: String,
    /// Bearer token sent in `Authorization: Bearer <t>` on the upgrade
    /// request. Coordinator's existing auth middleware checks it.
    pub auth_token: Option<String>,
    /// Heartbeat cadence. The coordinator timeouts dead hosts at ~6×
    /// this interval; default 5s gives ~30s detection (matches
    /// DESIGN.md:700).
    pub heartbeat_interval: Duration,
    /// Returns the data each Heartbeat carries. `None` falls back to
    /// empty fields — the scheduler then ranks by capacity-fallback
    /// only, which is fine for early/dev deployments.
    pub heartbeat_provider: Option<HeartbeatProvider>,
    /// ADR 0007: when set, the dialer writes the live
    /// [`HostSession`] into this handle on connect (so
    /// `WsAuthResolver` can issue host→coord RPCs over the WS) and
    /// clears it on disconnect. `None` skips the wiring — the
    /// OCI client falls back to whatever resolver the binary
    /// configured at startup.
    pub auth_session_handle: Option<crate::ws_auth::SessionHandle>,
}

/// Dialer entry point. Runs forever, reconnecting on disconnect.
/// Returns on Ctrl-C / cancellation upstream.
pub async fn run_dialer(
    cfg: DialerConfig,
    host_id: HostId,
    backend: Arc<dyn SandboxBackend>,
) -> std::io::Result<()> {
    let mut backoff = Duration::from_millis(500);
    const BACKOFF_CAP: Duration = Duration::from_secs(30);

    loop {
        match connect_once(&cfg, host_id, backend.clone()).await {
            Ok(()) => {
                tracing::info!("coordinator connection ended cleanly; reconnecting");
                backoff = Duration::from_millis(500);
            }
            Err(e) => {
                tracing::warn!(error = %e, ?backoff, "coordinator connection failed; backing off");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(BACKOFF_CAP);
            }
        }
    }
}

async fn connect_once(
    cfg: &DialerConfig,
    host_id: HostId,
    backend: Arc<dyn SandboxBackend>,
) -> std::io::Result<()> {
    let target = build_ws_url(&cfg.coordinator_url);
    let mut request = target
        .as_str()
        .into_client_request()
        .map_err(|e| std::io::Error::other(format!("bad coordinator url: {e}")))?;
    if let Some(token) = cfg.auth_token.as_ref() {
        let value = format!("Bearer {token}")
            .parse()
            .map_err(|e| std::io::Error::other(format!("bad auth token: {e}")))?;
        request.headers_mut().insert(header::AUTHORIZATION, value);
    }

    let (ws, _resp) = connect_async(request)
        .await
        .map_err(|e| std::io::Error::other(format!("ws connect failed: {e}")))?;
    tracing::info!(url = %target, "host connected to coordinator");

    let (write, read) = futures::stream::StreamExt::split(ws);
    let session = HostSession::new(write);

    // ADR 0007: publish the live session to the auth handle BEFORE
    // sending Hello so any concurrent OCI pull that's already
    // awaiting a `resolve()` round-trip can land its RPC the moment
    // the coord registers us. Cleared at the end of this function.
    if let Some(h) = cfg.auth_session_handle.as_ref() {
        *h.write().await = Some(session.clone());
    }

    // First frame: Hello. Failure here means the coordinator never
    // sees us as registered; bail out to retry.
    let _ = session
        .notify(NotifyKind::Hello {
            host_id,
            agent_version: env!("CARGO_PKG_VERSION").to_string(),
            wire_version: engram_protocol::WIRE_VERSION,
        })
        .await
        .map_err(|e| std::io::Error::other(format!("hello send failed: {e}")));

    // Heartbeat loop runs alongside the inbound serve. Stops when the
    // session's writer errors (which happens once the connection drops).
    let hb_session = session.clone();
    let hb_handle = tokio::spawn(heartbeat_loop(
        hb_session,
        host_id,
        cfg.heartbeat_interval,
        cfg.heartbeat_provider.clone(),
    ));

    // Block on inbound frames until the connection ends.
    session.serve_with_reader(backend, None, read).await;

    hb_handle.abort();

    // Clear the auth handle so any pull issued mid-disconnect fails
    // fast instead of trying to RPC against a dead session.
    if let Some(h) = cfg.auth_session_handle.as_ref() {
        *h.write().await = None;
    }
    Ok(())
}

async fn heartbeat_loop(
    session: HostSession,
    host_id: HostId,
    interval: Duration,
    provider: Option<HeartbeatProvider>,
) {
    let mut tick = tokio::time::interval(interval);
    tick.tick().await; // first tick fires immediately; skip
    loop {
        tick.tick().await;
        let (capacity, warm_pools, local_snapshots, draining) = match provider.as_ref() {
            Some(f) => f(),
            None => (HostCapacityReport::default(), Vec::new(), Vec::new(), false),
        };
        let hb = Heartbeat {
            host_id,
            sent_at: chrono::Utc::now(),
            capacity,
            warm_pools,
            local_snapshots,
            draining,
        };
        if let Err(e) = session.notify(NotifyKind::Heartbeat(hb)).await {
            tracing::debug!(error = %e, "heartbeat send failed; stopping loop");
            break;
        }
    }
}

/// `http://host:port` and `https://host:port` map to `ws://...` /
/// `wss://...`; bare `host:port` defaults to `ws://`. The path is
/// always appended.
fn build_ws_url(coordinator_url: &str) -> String {
    let trimmed = coordinator_url.trim_end_matches('/');
    let base = if let Some(rest) = trimmed.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = trimmed.strip_prefix("http://") {
        format!("ws://{rest}")
    } else if trimmed.starts_with("ws://") || trimmed.starts_with("wss://") {
        trimmed.to_string()
    } else {
        format!("ws://{trimmed}")
    };
    format!("{base}/api/hosts/connect")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_ws_url_rewrites_http_schemes() {
        assert_eq!(
            build_ws_url("http://coord:8080"),
            "ws://coord:8080/api/hosts/connect",
        );
        assert_eq!(
            build_ws_url("https://coord:8080"),
            "wss://coord:8080/api/hosts/connect",
        );
        assert_eq!(
            build_ws_url("ws://coord:8080/"),
            "ws://coord:8080/api/hosts/connect",
        );
        assert_eq!(
            build_ws_url("coord:8080"),
            "ws://coord:8080/api/hosts/connect",
        );
    }
}
