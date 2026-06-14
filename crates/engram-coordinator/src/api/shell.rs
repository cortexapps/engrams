//! `GET /sessions/:id/shell` — WebSocket proxy to `ttyd` running
//! inside the session's guest VM.
//!
//! Wire shape (ADR 0014 issue #6): browser opens a WebSocket here;
//! the coordinator opens a [`HostClient::proxy_shell`] tunnel to the
//! host that owns the sandbox; both ends bridge WS frames over the
//! tunnel's mpsc channels. The pre-M1.16 design dialed
//! `ws://<guest_ip>:7681/ws` directly from coord, which failed
//! every time from k8s (coord pod has no route to the per-VM
//! `10.200.0.0/24`). The new wire shape goes through the existing
//! coord ↔ host-agent gRPC channel.
//!
//! Failure modes:
//! - 404 if the session doesn't have a sandbox bound.
//! - 503 if the host's `proxy_shell` returns an error (typically
//!   because ttyd hasn't bound yet or `guest_ip` is unavailable).
//! - WebSocket close on inner-tunnel termination.
//!
//! Auth surface: this endpoint sits behind the same bearer-token
//! middleware as the rest of the coordinator. With auth disabled
//! (the `just dev` default) anyone with network access to the
//! coordinator can drop into a session's shell.

use axum::extract::ws::{Message as AxumMessage, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use engram_core::types::shell::{ShellClose, ShellFrame, ShellTunnel};
use engram_core::SessionId;
use futures::sink::SinkExt;
use futures::stream::StreamExt;

use crate::state::SharedState;

pub async fn shell(
    State(state): State<SharedState>,
    Path(id): Path<SessionId>,
    upgrade: WebSocketUpgrade,
) -> Response {
    // Auto-resume idle sessions, same as `POST /sessions/:id/prompt`.
    if let Err(e) = crate::api::snapshot::ensure_active(&state, id).await {
        return e.into_response();
    }

    let Some(sandbox_id) = state.resolve_sandbox(id).await else {
        return (
            StatusCode::CONFLICT,
            "session has no live sandbox after auto-resume; \
             try again or `engram session resume <id>` and retry",
        )
            .into_response();
    };

    let host = state.services.host.clone();
    upgrade
        .protocols(["tty"]) // ttyd advertises the "tty" subprotocol
        .on_upgrade(move |socket| async move {
            // Pin the sandbox against idle eviction for the lifetime
            // of the WebSocket (same as pre-M1.16). Released on
            // bridge exit (success or error).
            if let Err(e) = host.acquire_shell(sandbox_id).await {
                tracing::warn!(
                    session_id = %id,
                    error = %e,
                    "acquire_shell failed; shell still opens but idle eviction may race",
                );
            }

            // Open the host-side tunnel. The host-agent's impl
            // dials ttyd inside the right netns and bridges frames
            // through the gRPC tunnel.
            let tunnel = match host.proxy_shell(sandbox_id).await {
                Ok(t) => t,
                Err(e) => {
                    tracing::warn!(
                        session_id = %id,
                        error = %e,
                        "proxy_shell tunnel open failed; closing browser WS",
                    );
                    let _ = host.release_shell(sandbox_id).await;
                    let _ = close_browser(socket, 1011, &format!("proxy_shell: {e}")).await;
                    return;
                }
            };

            // Issue #219: renew the host-side pin on a fixed interval
            // while the bridge is live. The `release_shell` below is the
            // happy-path teardown, but if THIS task dies without running
            // it — the coord pod is killed mid-session (rolling deploy),
            // the future is dropped, the WS is severed — the host would
            // otherwise keep the sandbox pinned against idle eviction
            // forever. The renewer is part of this same task, so its
            // ticks stop the instant the bridge does; the host reaps any
            // pin it stops hearing about within its stale window.
            let renew_host = host.clone();
            let mut renew_tick =
                tokio::time::interval(engram_host_agent::harness::SHELL_PIN_RENEW_INTERVAL);
            renew_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            // First tick fires immediately; skip it (acquire_shell just
            // stamped the pin) so we don't issue a redundant RPC.
            renew_tick.tick().await;
            let renewer = async {
                loop {
                    renew_tick.tick().await;
                    if let Err(e) = renew_host.renew_shell(sandbox_id).await {
                        tracing::warn!(
                            session_id = %id,
                            error = %e,
                            "renew_shell failed; pin may be reaped early if this persists",
                        );
                    }
                }
            };

            let bridge_result = tokio::select! {
                r = bridge_browser_to_tunnel(socket, tunnel) => r,
                // The renewer loops forever; it only resolves if the
                // process is shutting down. In practice the bridge arm
                // always wins. `never` keeps the select exhaustive.
                _ = renewer => Ok(()),
            };
            if let Err(e) = bridge_result {
                tracing::warn!(
                    session_id = %id,
                    error = %e,
                    "shell proxy bridge ended with error",
                );
            }
            if let Err(e) = host.release_shell(sandbox_id).await {
                tracing::warn!(
                    session_id = %id,
                    error = %e,
                    "release_shell failed; the hub's pin count may drift \
                     (the host-side stale sweep is the backstop — issue #219)",
                );
            }
        })
}

async fn bridge_browser_to_tunnel(browser: WebSocket, tunnel: ShellTunnel) -> Result<(), String> {
    let (mut br_sink, mut br_stream) = browser.split();
    let ShellTunnel {
        outbound,
        mut inbound,
    } = tunnel;

    // Browser → ttyd: translate Axum WS frames to ShellFrame and push
    // into the tunnel's outbound channel.
    let b2t = async {
        while let Some(msg) = br_stream.next().await {
            let msg = msg.map_err(|e| format!("browser recv: {e}"))?;
            let frame = match msg {
                AxumMessage::Text(t) => ShellFrame::Text(t),
                AxumMessage::Binary(b) => ShellFrame::Binary(Bytes::from(b)),
                AxumMessage::Ping(b) => ShellFrame::Ping(Bytes::from(b)),
                AxumMessage::Pong(b) => ShellFrame::Pong(Bytes::from(b)),
                AxumMessage::Close(Some(cf)) => ShellFrame::Close(Some(ShellClose {
                    code: cf.code,
                    reason: cf.reason.to_string(),
                })),
                AxumMessage::Close(None) => ShellFrame::Close(None),
            };
            outbound
                .send(frame)
                .await
                .map_err(|_| "tunnel outbound closed (host-side tore down)".to_string())?;
        }
        Ok::<(), String>(())
    };

    // ttyd → browser: drain ShellFrame from the tunnel and forward
    // to the Axum WS.
    let t2b = async {
        while let Some(frame) = inbound.recv().await {
            let axum = match frame {
                ShellFrame::Text(t) => AxumMessage::Text(t),
                ShellFrame::Binary(b) => AxumMessage::Binary(b.to_vec()),
                ShellFrame::Ping(b) => AxumMessage::Ping(b.to_vec()),
                ShellFrame::Pong(b) => AxumMessage::Pong(b.to_vec()),
                ShellFrame::Close(Some(c)) => {
                    use axum::extract::ws::CloseFrame as AxumCloseFrame;
                    AxumMessage::Close(Some(AxumCloseFrame {
                        code: c.code,
                        reason: c.reason.into(),
                    }))
                }
                ShellFrame::Close(None) => AxumMessage::Close(None),
            };
            br_sink
                .send(axum)
                .await
                .map_err(|e| format!("browser send: {e}"))?;
        }
        Ok::<(), String>(())
    };

    tokio::select! {
        r = b2t => r,
        r = t2b => r,
    }
}

/// Close the browser WebSocket with a status code + reason so the
/// dashboard's `<TerminalPane>` can surface the failure.
async fn close_browser(socket: WebSocket, code: u16, reason: &str) -> Result<(), String> {
    use axum::extract::ws::CloseFrame as AxumCloseFrame;
    let mut socket = socket;
    socket
        .send(AxumMessage::Close(Some(AxumCloseFrame {
            code,
            reason: reason.to_string().into(),
        })))
        .await
        .map_err(|e| format!("browser close send: {e}"))
}
