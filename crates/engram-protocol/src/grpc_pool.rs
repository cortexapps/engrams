//! `GrpcHostPool` — per-coord-pod cache of `GrpcHostClient`s keyed
//! by `HostId` (ADR 0013).
//!
//! tonic's `Channel` is already HTTP/2-multiplexing, so a single
//! `Channel` per host backs every concurrent RPC. The pool's job is
//! to:
//!
//! 1. Look up the right `Channel` for a given `host_id`,
//!    constructing one lazily from `host_addr` on first use.
//! 2. **Warm** the connection ahead of dispatch — fire a no-op `Ping`
//!    so the TCP+H2 handshake completes before a session-create RPC
//!    lands on the same pod.
//! 3. **Evict** entries when the dead-host detector fires
//!    `pg_notify('host_dead', ...)` so in-flight gRPC calls fail
//!    fast with `Unavailable`.
//!
//! No retry logic lives here — `tonic::transport::Channel` plus the
//! HTTP/2 keepalive config does its own connection-level
//! reconnection. Per-RPC retry is the call-site's job.

use std::time::Duration;

use dashmap::DashMap;
use engram_core::{HostId, SandboxError};
use tonic::transport::Endpoint;

use crate::grpc_client::GrpcHostClient;

/// Pool of gRPC channels to hosts. Owned by `AppState` in the
/// coordinator; each coord pod gets its own.
pub struct GrpcHostPool {
    entries: DashMap<HostId, PooledHost>,
    /// HTTP/2 keepalive interval. Default 15s — short enough that
    /// idle connections stay healthy through L4-LB / NAT timeouts,
    /// long enough that the chatter is invisible in production.
    keepalive_interval: Duration,
    /// HTTP/2 keepalive timeout. Default 5s.
    keepalive_timeout: Duration,
    /// Per-RPC connect timeout (cold dial budget). Default 2s —
    /// in-VPC TCP+H2 setup is sub-second; anything past 2s means
    /// the host is unreachable, not slow.
    connect_timeout: Duration,
}

/// One entry per host. Holding a `GrpcHostClient` (which holds an
/// `Arc<Channel>`) keeps the underlying connection alive.
#[derive(Clone)]
struct PooledHost {
    client: GrpcHostClient,
    host_addr: String,
}

impl Default for GrpcHostPool {
    fn default() -> Self {
        Self::new()
    }
}

impl GrpcHostPool {
    pub fn new() -> Self {
        Self {
            entries: DashMap::new(),
            keepalive_interval: Duration::from_secs(15),
            keepalive_timeout: Duration::from_secs(5),
            connect_timeout: Duration::from_secs(2),
        }
    }

    /// Override keepalive interval. Useful in tests with tighter
    /// failure-detection windows.
    pub fn with_keepalive(mut self, interval: Duration, timeout: Duration) -> Self {
        self.keepalive_interval = interval;
        self.keepalive_timeout = timeout;
        self
    }

    /// Idempotent warm-up: ensure a `Channel` exists for `host_id`
    /// and that the underlying TCP+H2 handshake is in flight or
    /// complete.
    ///
    /// Called from three places:
    ///   - `POST /api/hosts/register` receive — initial host pickup
    ///   - `POST /api/hosts/:id/heartbeat` receive — keeps the entry
    ///     fresh on whatever pod fields the heartbeat (any pod can)
    ///   - coord-pod startup — preloads pool entries for every
    ///     `hosts` row with `status IN ('ready','draining')`
    ///
    /// `host_addr` example: `http://10.10.0.42:9101`. Plain HTTP/2
    /// (h2c) — TLS off, bearer-token auth happens per-RPC via tonic
    /// interceptors that the call sites attach.
    pub async fn warm(&self, host_id: HostId, host_addr: String) -> Result<(), SandboxError> {
        // If we already have an entry for this addr, leave it alone.
        // If the addr changed (host re-registered with a fresh IP
        // after MIG replacement), drop the old entry and rebuild —
        // the in-flight gRPC calls on the old Channel error with
        // Unavailable, which the call sites translate into retries.
        if let Some(existing) = self.entries.get(&host_id) {
            if existing.host_addr == host_addr {
                // Nothing to do; tonic handles keepalive.
                return Ok(());
            }
            drop(existing);
            self.entries.remove(&host_id);
        }

        let endpoint = build_endpoint(
            &host_addr,
            self.connect_timeout,
            self.keepalive_interval,
            self.keepalive_timeout,
        )?;
        // `connect_lazy` returns immediately; the first RPC pays the
        // TCP+H2 handshake. We follow up with `ping` below to force
        // the handshake before the first real RPC.
        let channel = endpoint.connect_lazy();
        let client = GrpcHostClient::new(channel);

        self.entries.insert(
            host_id,
            PooledHost {
                client: client.clone(),
                host_addr: host_addr.clone(),
            },
        );

        // Force the handshake. Errors are non-fatal — the entry is
        // already in the map; the first real RPC will retry. We log
        // so a perpetually-unreachable host shows up in metrics.
        if let Err(e) = client.ping().await {
            tracing::warn!(
                %host_id,
                host_addr = %host_addr,
                error = %e,
                "GrpcHostPool::warm Ping failed; first real RPC will retry",
            );
        }
        Ok(())
    }

    /// The address this pool is currently dialing for `host_id`, if any.
    /// ADR 0044 K2 (GAP 1): the heartbeat handler compares this against
    /// the host's advertised addr to detect a restarted pod whose IP
    /// changed under a stable HostId, and re-`warm`s when they differ.
    pub fn current_addr(&self, host_id: HostId) -> Option<String> {
        self.entries.get(&host_id).map(|e| e.host_addr.clone())
    }

    /// Drop the entry for a dead/migrated host. Any cloned
    /// `GrpcHostClient` held by an in-flight RPC keeps the
    /// `Channel` alive until that RPC completes; new lookups won't
    /// find this host until something calls `warm` again.
    pub fn evict(&self, host_id: HostId) {
        self.entries.remove(&host_id);
    }

    /// Hand out a clone of the pool's `GrpcHostClient` for
    /// `host_id`. Returns `NotFound` if no entry exists — the
    /// caller is expected to have routed via `sessions.host_id` /
    /// `hosts.host_addr` and warmed the pool first.
    pub fn get(&self, host_id: HostId) -> Result<GrpcHostClient, SandboxError> {
        self.entries
            .get(&host_id)
            .map(|e| e.client.clone())
            .ok_or(SandboxError::NotFound)
    }

    /// Convenience: warm + get in one call. Used by call sites that
    /// just looked up `host_addr` from PG and want to immediately
    /// dispatch.
    pub async fn get_or_warm(
        &self,
        host_id: HostId,
        host_addr: String,
    ) -> Result<GrpcHostClient, SandboxError> {
        self.warm(host_id, host_addr).await?;
        self.get(host_id)
    }

    /// Number of pool entries — exposed for metrics + tests.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

fn build_endpoint(
    host_addr: &str,
    connect_timeout: Duration,
    keepalive_interval: Duration,
    keepalive_timeout: Duration,
) -> Result<Endpoint, SandboxError> {
    let ep = Endpoint::from_shared(host_addr.to_string())
        .map_err(|e| SandboxError::InvalidSpec(format!("invalid host_addr {host_addr}: {e}")))?
        .connect_timeout(connect_timeout)
        .http2_keep_alive_interval(keepalive_interval)
        .keep_alive_timeout(keepalive_timeout)
        // Keep keepalive pings flowing even when no RPCs are
        // in-flight; otherwise a quiet host's connection silently
        // dies through L4-LB / NAT timeouts and the next RPC takes
        // a cold-dial penalty.
        .keep_alive_while_idle(true)
        .tcp_nodelay(true)
        // Send-window for ExecStream throughput. Default 64 KiB is
        // tight when an exec dumps a few MiB of stdout; 1 MiB keeps
        // streaming smooth without buffering on the host side.
        .initial_stream_window_size(Some(1024 * 1024))
        .initial_connection_window_size(Some(4 * 1024 * 1024));
    Ok(ep)
}

#[cfg(test)]
mod tests {
    use super::*;
    use engram_core::HostId;
    use std::sync::Arc;

    #[tokio::test]
    async fn pool_warm_inserts_entry() {
        let pool = Arc::new(GrpcHostPool::new());
        let host_id = HostId::new();
        // `connect_lazy` succeeds even when no server is listening —
        // the handshake happens on the first RPC. `Ping` fails, but
        // `warm` doesn't propagate the failure (warm is idempotent
        // and best-effort).
        pool.warm(host_id, "http://127.0.0.1:1".into())
            .await
            .expect("warm should not error on lazy connect");
        assert_eq!(pool.len(), 1);
        assert!(pool.get(host_id).is_ok());
    }

    #[tokio::test]
    async fn pool_warm_replaces_on_addr_change() {
        let pool = Arc::new(GrpcHostPool::new());
        let host_id = HostId::new();
        pool.warm(host_id, "http://127.0.0.1:1".into())
            .await
            .unwrap();
        pool.warm(host_id, "http://127.0.0.1:2".into())
            .await
            .unwrap();
        // Still one entry, but the addr changed under the hood.
        assert_eq!(pool.len(), 1);
    }

    #[tokio::test]
    async fn pool_evict_drops_entry() {
        let pool = Arc::new(GrpcHostPool::new());
        let host_id = HostId::new();
        pool.warm(host_id, "http://127.0.0.1:1".into())
            .await
            .unwrap();
        pool.evict(host_id);
        assert!(pool.is_empty());
        assert!(matches!(pool.get(host_id), Err(SandboxError::NotFound)));
    }

    #[tokio::test]
    async fn pool_get_unknown_host_is_notfound() {
        let pool = Arc::new(GrpcHostPool::new());
        let host_id = HostId::new();
        assert!(matches!(pool.get(host_id), Err(SandboxError::NotFound)));
    }
}
