//! Dead-host auto-detector.
//!
//! Background task that polls for hosts whose `last_heartbeat_at` is
//! older than the configured threshold and races other coordinator
//! replicas (via Postgres advisory locks) for the right to evacuate
//! each candidate. The winner:
//!
//! 1. Atomically marks the host `Dead` in Postgres and transitions
//!    every session pointed at it to `Dead` with `host_id`
//!    cleared.
//! 2. Emits a `StatusChanged` event for each affected session so SSE
//!    subscribers see the transition.
//! 3. Fires `pg_notify('host_dead', host_id::text)` so other replicas
//!    drop the host from their in-memory `HostRegistry` (handled in
//!    `pg_listener`).
//! 4. Unregisters the host locally.
//!
//! Active execs running on the dead host don't need explicit
//! synthesis: dropping the host's `RemoteSandboxBackend` cascades
//! through the WS demuxer's `Closed` state, the per-exec stream
//! channel drops, and the SSE handler in `api/exec.rs` emits
//! `SessionEvent::ExecCompleted{exit_status: None}` at the natural
//! end of its event loop — same path as a clean exec exit.
//!
//! Without this detector, sessions on a dead host stay `Active`
//! forever (with a `host_id` pointing at a host that won't respond);
//! operators can still `POST /sessions/:id/migrate` by hand.

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use engram_core::traits::MetadataStore;
use engram_core::types::SessionState;
use engram_core::HostId;
use sqlx::postgres::PgPool;

use crate::host_registry::HostRegistry;
use crate::state::{IndexedEvent, SessionEvent, SessionEventBus};

#[derive(Clone, Debug)]
pub struct DeadHostConfig {
    /// How often to poll for stale hosts. The dead-host detection
    /// latency is `poll_interval + threshold` worst-case; with
    /// defaults that's 30s + 10s = 40s, just inside the
    /// DESIGN.md:700 deliverable target of 30s migration on
    /// `kill -9`. Tighten for stricter targets.
    pub poll_interval: Duration,
    /// A host is considered dead when its `last_heartbeat_at` is
    /// older than this. Default 30s (~6× the 5s heartbeat cadence)
    /// — comfortable margin for transient network blips, fast
    /// enough to catch real failures.
    pub stale_threshold: Duration,
}

impl Default for DeadHostConfig {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_secs(10),
            stale_threshold: Duration::from_secs(30),
        }
    }
}

/// Spawn the detector as a background task. Returns a JoinHandle the
/// caller can drop on shutdown. Runs forever; logs and continues on
/// per-tick errors so a transient Postgres blip doesn't stop the loop.
pub fn spawn(
    cfg: DeadHostConfig,
    pool: PgPool,
    meta: Arc<dyn MetadataStore>,
    host_registry: Arc<HostRegistry>,
    events: Arc<SessionEventBus>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(cfg.poll_interval);
        // Skip the immediate first tick — the coordinator just
        // started and no host has had time to be considered stale.
        tick.tick().await;
        loop {
            tick.tick().await;
            if let Err(e) = run_once(&cfg, &pool, &meta, &host_registry, &events).await {
                tracing::warn!(error = %e, "dead-host detector tick failed; will retry");
            }
        }
    })
}

async fn run_once(
    cfg: &DeadHostConfig,
    pool: &PgPool,
    meta: &Arc<dyn MetadataStore>,
    host_registry: &Arc<HostRegistry>,
    events: &Arc<SessionEventBus>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let candidates = meta.list_stale_hosts(cfg.stale_threshold.as_secs()).await?;
    if candidates.is_empty() {
        return Ok(());
    }
    tracing::debug!(
        count = candidates.len(),
        "dead-host detector found stale candidates"
    );
    for host in candidates {
        if let Err(e) = evict_host(pool, meta, host_registry, events, host.id).await {
            tracing::warn!(host_id = %host.id, error = %e, "evict failed; another replica may have it");
        }
    }
    Ok(())
}

async fn evict_host(
    pool: &PgPool,
    meta: &Arc<dyn MetadataStore>,
    host_registry: &Arc<HostRegistry>,
    events: &Arc<SessionEventBus>,
    host_id: HostId,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Pin a single connection so the advisory lock stays with us for
    // the duration of the eviction. `pg_try_advisory_lock` is a
    // session-scoped lock and auto-releases when the connection
    // closes — so even if we panic mid-eviction, the lock doesn't
    // strand the host.
    let mut conn = pool.acquire().await?;
    let lock_key: String = format!("dead-host:{host_id}");

    let got: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock(hashtext($1))")
        .bind(&lock_key)
        .fetch_one(&mut *conn)
        .await?;
    if !got {
        // Another coordinator replica won the race for this host.
        // It will do the eviction; we just skip.
        tracing::debug!(host_id = %host_id, "advisory lock contested; skipping");
        return Ok(());
    }

    // Re-check the host's status *after* taking the lock — another
    // replica that already won may have flipped it to Dead in the
    // window between our `list_stale_hosts` and now.
    let status: Option<String> = sqlx::query_scalar("SELECT status FROM hosts WHERE id = $1")
        .bind(host_id.as_uuid())
        .fetch_optional(&mut *conn)
        .await?
        .flatten();
    if matches!(status.as_deref(), Some("dead") | None) {
        tracing::debug!(host_id = %host_id, "host already dead; releasing lock");
        sqlx::query("SELECT pg_advisory_unlock(hashtext($1))")
            .bind(&lock_key)
            .execute(&mut *conn)
            .await?;
        return Ok(());
    }

    let session_ids = meta.mark_host_dead_and_reassign_sessions(host_id).await?;

    // Notify other replicas so they drop their HostRegistry entry.
    sqlx::query("SELECT pg_notify('host_dead', $1)")
        .bind(host_id.to_string())
        .execute(&mut *conn)
        .await?;

    // Emit StatusChanged for every reassigned session so SSE clients
    // see the transition in their event stream. Failures here are
    // logged but don't roll back the eviction — the persistent state
    // already changed.
    for session_id in &session_ids {
        let event = SessionEvent::StatusChanged {
            from: SessionState::Active,
            to: SessionState::Dead,
            at: Utc::now(),
        };
        let kind = event.kind();
        let payload = match serde_json::to_value(&event) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(error = %e, %session_id, "serialize StatusChanged failed");
                continue;
            }
        };
        match meta.append_session_event(*session_id, kind, payload).await {
            Ok(idx) => {
                events.publish(*session_id, IndexedEvent { idx, event });
            }
            Err(e) => {
                tracing::warn!(error = %e, %session_id, "persist StatusChanged failed");
            }
        }
    }

    host_registry.unregister(host_id);
    tracing::info!(
        host_id = %host_id,
        sessions_reassigned = session_ids.len(),
        "host marked dead and sessions transitioned to dead",
    );

    sqlx::query("SELECT pg_advisory_unlock(hashtext($1))")
        .bind(&lock_key)
        .execute(&mut *conn)
        .await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    // The detector's polling loop and advisory-lock dance are
    // Postgres-specific and require a live database to test
    // meaningfully. The trait-layer logic
    // (`mark_host_dead_and_reassign_sessions` semantics) is covered
    // by Mock-based tests in `tests/dead_host_mock.rs`. End-to-end
    // multi-replica behaviour is the live-Postgres test
    // (`#[ignore]`'d, gated behind dev-VM Docker compose).
}
