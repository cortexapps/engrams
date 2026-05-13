//! Host-agent WebSocket endpoint. Each host dials
//! `GET /api/hosts/connect` (with `Authorization: Bearer <token>` so
//! the existing auth middleware applies). After the upgrade the host's
//! first frame must be a [`NotifyKind::Hello`] carrying its `HostId`;
//! the coordinator then registers a [`RemoteHostClient`] in
//! [`HostRegistry`] so all subsequent SandboxBackend calls route over
//! the wire.
//!
//! `--mode=all` skips this path entirely (the local backend is
//! registered directly at startup), so `just dev` keeps working.

use std::sync::Arc;

use axum::extract::ws::{Message as AxumMessage, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::Response;
use axum::Json;
use chrono::Utc;
use engram_core::types::host::{HostCapacity, HostMetadata, HostRecord, HostStatus};
use engram_core::HostId;
use engram_protocol::client::{ConnectedHost, RemoteHostClient};
use engram_protocol::wire::NotifyKind;
use engram_protocol::HeartbeatAck;
use serde::Serialize;

use crate::host_registry::HostState;
use futures::sink::SinkExt;
use futures::stream::StreamExt;
use tokio_tungstenite::tungstenite::{
    protocol::CloseFrame as TungsteniteCloseFrame, Error as TungsteniteError,
    Message as TungMessage,
};

use crate::error::ApiError;
use crate::state::SharedState;

pub async fn connect(State(state): State<SharedState>, ws: WebSocketUpgrade) -> Response {
    ws.on_upgrade(move |socket| handle_connection(state, socket))
}

/// `GET /api/hosts` — list all hosts the coordinator knows about,
/// merging the persisted Postgres rows with the live in-memory
/// scheduler state (capacity, local snapshots, draining).
pub async fn list(State(state): State<SharedState>) -> Result<Json<ListHostsResponse>, ApiError> {
    let rows = state.services.meta.list_active_hosts().await?;
    let hosts = rows
        .into_iter()
        .map(|row| {
            let live = state.host_registry.snapshot_state(row.id);
            HostView::from_row_and_live(row, live)
        })
        .collect();
    Ok(Json(ListHostsResponse { hosts }))
}

/// `GET /api/hosts/:id`. NotFound if the row isn't in Postgres.
pub async fn get(
    State(state): State<SharedState>,
    Path(host_id): Path<HostId>,
) -> Result<Json<HostView>, ApiError> {
    let rows = state.services.meta.list_active_hosts().await?;
    let row = rows
        .into_iter()
        .find(|r| r.id == host_id)
        .ok_or_else(|| ApiError::NotFound("host not found".into()))?;
    let live = state.host_registry.snapshot_state(host_id);
    Ok(Json(HostView::from_row_and_live(row, live)))
}

/// `POST /api/hosts/:id/drain`. Flips the host to Draining in both
/// the Postgres row and the in-memory scheduler view; new sessions
/// won't be assigned to it. In-flight sessions stay put — evacuation
/// lands in 3d.
pub async fn drain(
    State(state): State<SharedState>,
    Path(host_id): Path<HostId>,
) -> Result<StatusCode, ApiError> {
    state
        .services
        .meta
        .set_host_status(host_id, HostStatus::Draining)
        .await?;
    if let Some(mut s) = state.host_registry.snapshot_state(host_id) {
        s.draining = true;
        state.host_registry.update_state(host_id, s);
    }
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Serialize)]
pub struct ListHostsResponse {
    pub hosts: Vec<HostView>,
}

#[derive(Serialize)]
pub struct HostView {
    pub id: HostId,
    pub hostname: String,
    pub status: &'static str,
    pub capacity_total_mib: u64,
    pub capacity_used_mib: u64,
    pub running_sandboxes: u32,
    pub local_snapshots: usize,
    pub last_heartbeat_at: chrono::DateTime<Utc>,
}

impl HostView {
    fn from_row_and_live(
        row: engram_core::types::HostRecord,
        live: Option<crate::host_registry::HostState>,
    ) -> Self {
        let live = live.unwrap_or_default();
        Self {
            id: row.id,
            hostname: row.hostname,
            status: row.status.as_str(),
            capacity_total_mib: live.capacity.total_mib,
            capacity_used_mib: live.capacity.used_mib,
            running_sandboxes: live.capacity.running_sandboxes,
            local_snapshots: live.local_snapshots.len(),
            last_heartbeat_at: row.last_heartbeat_at,
        }
    }
}

// `tungstenite::Error` is 136 bytes — defined upstream, every variant
// counts, and `Result<_, TungsteniteError>` flowing through this
// closure is the boundary type ConnectedHost::spawn expects. We can't
// box it locally without a cascading signature change. Allow the
// `result_large_err` lint at the boundary.
#[allow(clippy::result_large_err)]
async fn handle_connection(state: SharedState, socket: WebSocket) {
    // Adapt axum::ws::Message ↔ tungstenite::Message at the boundary so
    // engram_protocol can stay axum-agnostic. We only ever encode/decode
    // Binary frames; the Text/Ping/Pong/Close paths just round-trip
    // the variant.
    let (axum_sink, axum_stream) = socket.split();

    let tung_sink = axum_sink
        .sink_map_err(|e| TungsteniteError::Io(std::io::Error::other(e.to_string())))
        .with(|m: TungMessage| async move { Ok::<AxumMessage, TungsteniteError>(tung_to_axum(m)) });
    let tung_stream = axum_stream.map(|res| {
        res.map(axum_to_tung)
            .map_err(|e| TungsteniteError::Io(std::io::Error::other(e.to_string())))
    });

    let (host, mut notify_rx, _demux_handle) =
        ConnectedHost::spawn(Box::pin(tung_sink), Box::pin(tung_stream));

    // Wait for Hello before registering. If the host disconnects or
    // sends garbage instead, give up and log.
    let host_id = match tokio::time::timeout(std::time::Duration::from_secs(10), notify_rx.recv())
        .await
    {
        Ok(Some(NotifyKind::Hello {
            host_id,
            agent_version,
            wire_version,
        })) => {
            if wire_version != engram_protocol::WIRE_VERSION {
                tracing::error!(
                    host_id = %host_id,
                    %agent_version,
                    host_wire_version = wire_version,
                    coord_wire_version = engram_protocol::WIRE_VERSION,
                    "WIRE-VERSION MISMATCH on /api/hosts/connect; rejecting registration. \
                     Rebuild coordinator + host-agent at the same commit, or drain before redeploy."
                );
                return;
            }
            tracing::info!(host_id = %host_id, %agent_version, wire_version, "host registered via /api/hosts/connect");
            host_id
        }
        Ok(Some(other)) => {
            tracing::warn!(?other, "host's first frame was not Hello; refusing");
            return;
        }
        Ok(None) => {
            tracing::warn!("host disconnected before sending Hello");
            return;
        }
        Err(_) => {
            tracing::warn!("host did not send Hello within 10s; closing");
            return;
        }
    };

    // Register: `RemoteHostClient` is a thin `HostClient` impl
    // wrapping the `ConnectedHost` demuxer. Cloning `host` lets the
    // supervisor task keep a handle for ack'ing heartbeats while the
    // backend trait object owns its own copy.
    let backend: Arc<dyn engram_core::traits::HostClient> =
        Arc::new(RemoteHostClient::new(host.clone()));
    // Hand a second clone to the registry as the admin client so
    // out-of-band RPCs (materialize-dir reap fanout, ADR 0007) can
    // reach this host directly.
    state
        .host_registry
        .register_remote(host_id, backend, host.clone());

    // ADR 0007: install the host-request handler so the host-agent
    // can resolve OCI auth via the coord's `PgAuthResolver`. Plaintext
    // creds traverse the WS only at pull time; never persisted on
    // the host side.
    host.set_request_handler(Arc::new(AuthRequestHandler {
        auth_resolver: state.services.auth_resolver.clone(),
    }));

    // Persist the row in Postgres so future scheduler queries see this
    // host. Phase 3a: capacity is unknown yet (heartbeats will populate
    // it in 3b); record an initial Ready entry with zero capacity.
    let initial_record = HostRecord {
        id: host_id,
        hostname: format!("host-{host_id}"),
        cloud_metadata: HostMetadata::default(),
        capacity: HostCapacity {
            total_gb: 0,
            used_gb: 0,
        },
        status: HostStatus::Ready,
        last_heartbeat_at: Utc::now(),
    };
    if let Err(e) = state.services.meta.upsert_host(initial_record).await {
        tracing::warn!(host_id = %host_id, error = %e, "host upsert failed; in-memory registration still active");
    }

    // Supervisor: drain Notifies until the connection closes. Phase 3a
    // just acks heartbeats so the host's keepalive timer doesn't fire
    // a reconnect; capacity is recorded in 3b.
    while let Some(notify) = notify_rx.recv().await {
        match notify {
            NotifyKind::Heartbeat(hb) => {
                // ADR 0009 §1-§3: run the reconcile pass synchronously
                // on every heartbeat ingest. Cheap (one indexed
                // SELECT per host); the strike-counter keeps it from
                // flapping on transient `backend.list()` errors. Any
                // session transitions land before the rest of the
                // heartbeat-handler work so subsequent metrics +
                // host-state updates reflect the post-flip view.
                let flipped = state
                    .reconciler
                    .reconcile_host(&state, host_id, &hb.running_sandboxes)
                    .await;
                if !flipped.is_empty() {
                    tracing::info!(
                        host_id = %host_id,
                        count = flipped.len(),
                        "reconcile flipped missing-sandbox sessions"
                    );
                }

                // Refresh the in-memory scheduler view first so the
                // next session creation sees the updated capacity and
                // local snapshots.
                state.host_registry.update_state(
                    host_id,
                    HostState {
                        capacity: hb.capacity.clone(),
                        local_snapshots: hb.local_snapshots.clone(),
                        draining: hb.draining,
                    },
                );

                let ack = HeartbeatAck {
                    server_time: Utc::now(),
                    revoked_sessions: Vec::new(),
                };
                if let Err(e) = host.notify(NotifyKind::HeartbeatAck(ack)).await {
                    tracing::debug!(host_id = %host_id, error = %e, "heartbeat ack failed; supervisor will exit");
                    break;
                }
                let row_status = if hb.draining {
                    HostStatus::Draining
                } else {
                    HostStatus::Ready
                };
                if let Err(e) = state
                    .services
                    .meta
                    .touch_host_heartbeat(host_id, row_status)
                    .await
                {
                    tracing::debug!(host_id = %host_id, error = %e, "heartbeat persistence failed");
                }
            }
            NotifyKind::Hello { .. } => {
                tracing::debug!(host_id = %host_id, "duplicate Hello; ignoring");
            }
            NotifyKind::HeartbeatAck(_) => {
                tracing::debug!(host_id = %host_id, "host sent unexpected HeartbeatAck; ignoring");
            }
            NotifyKind::SessionEgressPolicy(_) => {
                // Coordinator → host frame; if a host echoed one
                // back, it's a confused peer. Drop.
                tracing::debug!(host_id = %host_id, "host sent unexpected SessionEgressPolicy; ignoring");
            }
            NotifyKind::HarnessEvent {
                session_id,
                sandbox_id,
                event,
                at,
            } => {
                // Forwarded by the host-agent's local HarnessHub on every
                // adapter event. We re-emit through `state.emit` so SSE
                // subscribers see the same stream they would in mode=all,
                // where the hub's `EventSink` writes to session_events
                // directly. Mirrors the closure built by
                // `harness_event_sink(...)` in state.rs.
                if let Err(e) =
                    crate::state::emit_harness_event(&state, session_id, sandbox_id, event, at)
                        .await
                {
                    tracing::warn!(
                        host_id = %host_id,
                        session_id = %session_id,
                        sandbox_id = %sandbox_id,
                        error = %e,
                        "failed to emit forwarded harness event",
                    );
                }
            }
        }
    }

    tracing::info!(host_id = %host_id, "host disconnected; unregistering");
    state.host_registry.unregister(host_id);
}

// ---- axum::Message <-> tungstenite::Message bridges -------------------

fn axum_to_tung(m: AxumMessage) -> TungMessage {
    match m {
        AxumMessage::Text(t) => TungMessage::Text(t),
        AxumMessage::Binary(b) => TungMessage::Binary(b),
        AxumMessage::Ping(b) => TungMessage::Ping(b),
        AxumMessage::Pong(b) => TungMessage::Pong(b),
        AxumMessage::Close(Some(cf)) => TungMessage::Close(Some(TungsteniteCloseFrame {
            code: cf.code.into(),
            reason: cf.reason,
        })),
        AxumMessage::Close(None) => TungMessage::Close(None),
    }
}

fn tung_to_axum(m: TungMessage) -> AxumMessage {
    use axum::extract::ws::CloseFrame as AxumCloseFrame;
    match m {
        TungMessage::Text(t) => AxumMessage::Text(t),
        TungMessage::Binary(b) => AxumMessage::Binary(b),
        TungMessage::Ping(b) => AxumMessage::Ping(b),
        TungMessage::Pong(b) => AxumMessage::Pong(b),
        TungMessage::Close(Some(cf)) => AxumMessage::Close(Some(AxumCloseFrame {
            code: cf.code.into(),
            reason: cf.reason,
        })),
        TungMessage::Close(None) => AxumMessage::Close(None),
        TungMessage::Frame(_) => {
            // Raw frames don't appear in normal flow; treat as a Close
            // so the peer cleans up.
            AxumMessage::Close(None)
        }
    }
}

/// Coord-side handler for host-initiated requests. ADR 0007 wires
/// `ResolveRegistryAuth`; future host→coord RPCs slot in alongside.
/// Holds an `Arc<dyn RegistryAuthResolver>` so the existing
/// `PgAuthResolver` from `Services` is the single source of truth.
struct AuthRequestHandler {
    auth_resolver: Arc<dyn engram_oci::RegistryAuthResolver>,
}

#[async_trait::async_trait]
impl engram_protocol::HostRequestHandler for AuthRequestHandler {
    async fn handle(
        &self,
        kind: engram_protocol::RequestKind,
    ) -> Result<engram_protocol::ResponseKind, engram_protocol::RemoteError> {
        match kind {
            engram_protocol::RequestKind::ResolveRegistryAuth { host } => {
                tracing::debug!(host = %host, "host-agent requested OCI auth resolution");
                match self.auth_resolver.resolve(&host).await {
                    Ok(Some(creds)) => Ok(engram_protocol::ResponseKind::RegistryAuth {
                        creds: Some(engram_protocol::RegistryCreds {
                            username: creds.username,
                            password: creds.password,
                        }),
                    }),
                    Ok(None) => Ok(engram_protocol::ResponseKind::RegistryAuth { creds: None }),
                    Err(e) => Err(engram_protocol::RemoteError::Other(format!(
                        "resolve registry auth: {e}"
                    ))),
                }
            }
            other => Err(engram_protocol::RemoteError::Other(format!(
                "coord-side host-request handler doesn't serve {:?}",
                std::mem::discriminant(&other),
            ))),
        }
    }
}
