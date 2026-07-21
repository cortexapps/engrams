//! ADR 0073 phase 4: the shell keep-alive pin as a PG column.
//!
//! Replaces the host-side `shell_attached` refcount + acquire/renew/
//! release RPCs + stale sweep (issue #219 class): the relay stamps
//! `sessions.shell_pinned_until = now() + TTL` on open and re-stamps on
//! a renew tick; the idle detector skips pinned sessions; release (or a
//! dead coordinator pod) simply lets the stamp lapse. There is no
//! refcount: concurrent relays each re-stamp the same column, and the
//! pin holds until the LAST renewer stops — the same net semantics the
//! count provided, without host state.

use std::time::Duration;

use engram_core::SessionId;

use crate::state::SharedState;

/// Pin horizon per stamp. Mirrors the retired host-side
/// `SHELL_PIN_STALE_AGE` (300s): a healthy relay refreshes ~5× per
/// horizon, and a dead one lapses on the same clock the sweep used.
const PIN_TTL: Duration = Duration::from_secs(300);
/// Renew cadence — the retired `SHELL_PIN_RENEW_INTERVAL`.
const RENEW_EVERY: Duration = Duration::from_secs(60);

/// RAII pin: stamps on `new`, re-stamps on a tick, un-pins on
/// `release`/`Drop` (best-effort — a lost un-pin lapses by itself).
pub(crate) struct SessionShellPin {
    state: SharedState,
    session_id: SessionId,
    renew: tokio::task::JoinHandle<()>,
    released: bool,
}

impl SessionShellPin {
    pub(crate) async fn new(state: SharedState, session_id: SessionId) -> Self {
        stamp(&state, session_id, PIN_TTL).await;
        let renew_state = state.clone();
        let renew = tokio::spawn(async move {
            let mut tick = tokio::time::interval(RENEW_EVERY);
            tick.tick().await; // the constructor already stamped
            loop {
                tick.tick().await;
                stamp(&renew_state, session_id, PIN_TTL).await;
            }
        });
        Self {
            state,
            session_id,
            renew,
            released: false,
        }
    }

    /// Explicit un-pin on the normal exit path.
    pub(crate) fn release(mut self) {
        self.released = true;
        self.renew.abort();
        let state = self.state.clone();
        let session_id = self.session_id;
        tokio::spawn(async move {
            stamp(&state, session_id, Duration::ZERO).await;
        });
    }
}

impl Drop for SessionShellPin {
    fn drop(&mut self) {
        self.renew.abort();
        if self.released {
            return;
        }
        // Tonic cancelled mid-bridge: best-effort un-pin; a failure
        // here just means the pin lapses at the horizon.
        let state = self.state.clone();
        let session_id = self.session_id;
        tokio::spawn(async move {
            stamp(&state, session_id, Duration::ZERO).await;
        });
    }
}

async fn stamp(state: &SharedState, session_id: SessionId, ttl: Duration) {
    let until =
        state.services.clock.now_utc() + chrono::Duration::from_std(ttl).unwrap_or_default();
    if let Err(e) = state.services.meta.stamp_shell_pin(session_id, until).await {
        tracing::warn!(%session_id, error = %e, "shell pin stamp failed");
    }
}
