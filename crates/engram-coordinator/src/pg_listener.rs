//! Cross-coordinator event fan-out via Postgres `LISTEN/NOTIFY`.
//!
//! Phase 3c HA: when a host emits an event, the producing coordinator
//! replica writes the row + fires `NOTIFY session_events <payload>`
//! (see `crates/engram-postgres/src/lib.rs` `append_session_event`).
//! Every coordinator replica runs an instance of [`spawn`] which
//! `LISTEN`s on the same channel and re-broadcasts the event into its
//! local `SessionEventBus` so SSE subscribers see every event regardless
//! of which replica produced it.
//!
//! Dedupe: the SSE handler at `api/events.rs` already tracks the
//! highest-seen `idx` per subscriber and drops duplicates when the
//! "replay then live" seam emits an overlap. The same dedupe catches
//! duplicates between the local emit and the LISTEN echo on the
//! producing replica, so no extra plumbing is needed here.

use std::sync::Arc;

use engram_core::traits::MetadataStore;
use engram_core::{HostId, SessionId};
use serde::Deserialize;
use sqlx::postgres::PgListener;

use crate::boot_bundle::BootBundleCache;
use crate::host_registry::HostRegistry;
use crate::integrations::IntegrationBroker;
use crate::state::{IndexedEvent, SessionEvent, SessionEventBus};

#[derive(Debug, Deserialize)]
struct NotifyPayload {
    session_id: String,
    idx: i64,
}

/// Phase 1c: the `session_event_deltas` NOTIFY payload. Unlike
/// `session_events` (which carries only `{session_id, idx}` and has the
/// listener re-fetch the row), an ephemeral token chunk is NEVER persisted,
/// so its full [`SessionEvent`] rides inline — there is no row to fetch.
#[derive(Debug, Deserialize)]
struct DeltaNotifyPayload {
    session_id: String,
    event: SessionEvent,
}

/// Spawn the listener task. Returns immediately; the task runs until
/// the connection drops, at which point it logs and exits. A future
/// follow-up wraps this in an exp-backoff supervisor; for 3c the
/// coordinator restarts on connection loss because there's nothing
/// else useful to do without Postgres.
pub fn spawn(
    database_url: String,
    meta: Arc<dyn MetadataStore>,
    events: Arc<SessionEventBus>,
    host_registry: Arc<HostRegistry>,
    integrations: IntegrationBroker,
    boot_bundles: Arc<BootBundleCache>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        if let Err(e) = run(
            &database_url,
            meta,
            events,
            host_registry,
            integrations,
            boot_bundles,
        )
        .await
        {
            tracing::error!(error = %e, "pg listener task exited");
        }
    })
}

async fn run(
    database_url: &str,
    meta: Arc<dyn MetadataStore>,
    events: Arc<SessionEventBus>,
    host_registry: Arc<HostRegistry>,
    integrations: IntegrationBroker,
    boot_bundles: Arc<BootBundleCache>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut listener = PgListener::connect(database_url).await?;
    listener.listen("session_events").await?;
    listener.listen("session_event_deltas").await?;
    listener.listen("host_dead").await?;
    // ADR 0057 C2: org-secret writes/rotations invalidate the mint-engine cache.
    listener.listen("org_secret_changed").await?;
    // Issue #535 (a): boot-bundle cache invalidation. `enabled_image_changed`
    // fires on every enable/soft-delete/delete of an `enabled_images` row;
    // `fleet_catalog_changed` fires only when a host's `current_bundles`
    // stamp actually changes (the migration 0077 trigger's WHEN guard), not
    // on every heartbeat.
    listener.listen("enabled_image_changed").await?;
    listener.listen("fleet_catalog_changed").await?;
    tracing::info!(
        "pg_listener subscribed to session_events + session_event_deltas + host_dead + \
         org_secret_changed + enabled_image_changed + fleet_catalog_changed"
    );

    loop {
        let notification = listener.recv().await?;
        match notification.channel() {
            "host_dead" => {
                // Payload is just `<uuid>` (no JSON wrapper) — the
                // detector emits it as a plain text NOTIFY.
                match notification.payload().parse::<HostId>() {
                    Ok(host_id) => {
                        host_registry.unregister(host_id);
                        tracing::debug!(
                            %host_id,
                            "pg_listener dropped host_registry entry on host_dead",
                        );
                    }
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            payload = notification.payload(),
                            "malformed host_dead notification; skipping",
                        );
                    }
                }
                continue;
            }
            "org_secret_changed" => {
                // ADR 0057 C2: an org secret was written or rotated (A1's write
                // path NOTIFYs the secret name). Invalidate the broker's lazy
                // mint-engine cache so the next mint rebuilds from the updated
                // value. Fires on every replica, so each rebuilds independently.
                integrations.invalidate();
                tracing::info!(
                    secret = notification.payload(),
                    "org secret changed; invalidated mint-engine cache",
                );
                continue;
            }
            "enabled_image_changed" => {
                // Issue #535 (a): the payload is the plain `image_uri` text
                // (no JSON wrapper), matching `host_dead`'s convention.
                boot_bundles.invalidate_image(notification.payload());
                tracing::debug!(
                    image_uri = notification.payload(),
                    "enabled image changed; invalidated boot-bundle cache entry",
                );
                continue;
            }
            "fleet_catalog_changed" => {
                // Issue #535 (a): any host's `current_bundles` stamp changed
                // (a host roll) — invalidate the whole cached catalog rather
                // than tracking which host the cached view came from (see
                // `BootBundleCache::invalidate_fleet_catalog` docs).
                boot_bundles.invalidate_fleet_catalog();
                tracing::debug!(
                    host_id = notification.payload(),
                    "fleet bundle stamp changed; invalidated fleet-catalog cache",
                );
                continue;
            }
            "session_event_deltas" => {
                // Phase 1c: an EPHEMERAL token chunk. The full event rides
                // inline (no persisted row to fetch); decode it and publish
                // straight to the local bus marked ephemeral. This arm fires
                // on EVERY replica including the producer — it is the single
                // delivery path for chunks, so a client on a replica without
                // the harness connection still streams.
                match serde_json::from_str::<DeltaNotifyPayload>(notification.payload()) {
                    Ok(p) => match p.session_id.parse::<SessionId>() {
                        Ok(session_id) => {
                            events.publish(
                                session_id,
                                IndexedEvent {
                                    idx: 0,
                                    event: p.event,
                                    ephemeral: true,
                                },
                            );
                        }
                        Err(e) => tracing::warn!(
                            error = %e,
                            session_id = %p.session_id,
                            "delta notification has bad uuid; skipping",
                        ),
                    },
                    Err(e) => tracing::warn!(
                        error = %e,
                        "malformed session_event_deltas notification; skipping",
                    ),
                }
                continue;
            }
            "session_events" => {}
            other => {
                tracing::debug!(channel = other, "unexpected NOTIFY channel; ignoring");
                continue;
            }
        }
        let payload: NotifyPayload = match serde_json::from_str(notification.payload()) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    payload = notification.payload(),
                    "malformed session_events notification; skipping",
                );
                continue;
            }
        };
        let session_id: SessionId = match payload.session_id.parse() {
            Ok(id) => id,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    session_id = %payload.session_id,
                    "session_events notification has bad uuid; skipping",
                );
                continue;
            }
        };

        // Fetch the row (`since=idx-1` returns at most this one event).
        let rows = match meta
            .list_session_events_since(session_id, payload.idx - 1, 1)
            .await
        {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    session_id = %session_id,
                    idx = payload.idx,
                    "fetching listened-event row failed; skipping",
                );
                continue;
            }
        };
        let row = match rows.into_iter().next() {
            Some(r) => r,
            None => {
                // Producer's commit hadn't propagated yet. Coordinator
                // replicas trade a tiny bit of latency for a clean dedupe
                // story by re-querying once after a short delay.
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                match meta
                    .list_session_events_since(session_id, payload.idx - 1, 1)
                    .await
                {
                    Ok(rs) => match rs.into_iter().next() {
                        Some(r) => r,
                        None => {
                            tracing::warn!(
                                session_id = %session_id,
                                idx = payload.idx,
                                "notification arrived but row never materialised after 50ms",
                            );
                            continue;
                        }
                    },
                    Err(e) => {
                        tracing::warn!(error = %e, "retry fetch failed; skipping");
                        continue;
                    }
                }
            }
        };

        // Decode the persisted JSON back into a typed SessionEvent.
        // Legacy rows that don't round-trip cleanly (schema drift)
        // get logged + dropped — better than poisoning the bus.
        let event: SessionEvent = match serde_json::from_value(row.payload) {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    session_id = %session_id,
                    idx = row.idx,
                    "listened event payload didn't decode as SessionEvent; dropping",
                );
                continue;
            }
        };
        events.publish(
            session_id,
            IndexedEvent {
                idx: row.idx,
                event,
                ephemeral: false,
            },
        );
    }
}
