//! `GET /sessions/:id/shell` — WebSocket proxy to `ttyd` running
//! inside the session's guest VM.
//!
//! Wire shape: the browser opens a WebSocket here; the coordinator
//! looks up the sandbox's `guest_ip` (provided by agentd via the
//! existing `WireRequest::GuestIp` verb), opens a WebSocket to
//! `ws://<guest_ip>:7681/ws`, and forwards messages bidirectionally
//! with no payload inspection. ttyd's protocol is opaque to the
//! proxy — the dashboard's `<TerminalPane>` adapts it client-side.
//!
//! Failure modes:
//! - 404 if the session doesn't have a sandbox bound (e.g. it's
//!   still pending or already evicted).
//! - 503 if the backend has no host→guest IP routing (FC today)
//!   or if agentd hasn't reported its IP yet (boot race).
//! - WebSocket close on inner-connection failure.
//!
//! Auth surface: this endpoint sits behind the same bearer-token
//! middleware as the rest of the coordinator. With auth disabled
//! (the `just dev` default) anyone with network access to the
//! coordinator can drop into a session's shell — see
//! `docs/known-issues.md` for the explicit warning.

use axum::extract::ws::{Message as AxumMessage, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use engram_core::SessionId;
use futures::sink::SinkExt;
use futures::stream::StreamExt;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::{
    protocol::CloseFrame as TungCloseFrame, Message as TungMessage,
};

use crate::state::SharedState;

/// ttyd inside the guest binds here. Pinned to match the launch
/// command in `engram-image-builder::DEFAULT_INIT_SHIM`.
const GUEST_TTYD_PORT: u16 = 7681;

pub async fn shell(
    State(state): State<SharedState>,
    Path(id): Path<SessionId>,
    upgrade: WebSocketUpgrade,
) -> Response {
    // Auto-resume idle sessions, same as `POST /sessions/:id/prompt`.
    // The user clicking SHELL on an idle session reads as "I want to
    // poke around in this session's filesystem" — the right
    // affordance is to spin the sandbox back up, not 404. Dead
    // sessions surface 410 Gone here (ensure_active → resume).
    if let Err(e) = crate::api::snapshot::ensure_active(&state, id).await {
        return e.into_response();
    }

    let Some(sandbox_id) = state.registry.get(id) else {
        // After ensure_active an Active session must have a sandbox
        // bound. If we get here something raced; surface a 409
        // matching the prompt-endpoint affordance.
        return (
            StatusCode::CONFLICT,
            "session has no live sandbox after auto-resume; \
             try again or `engram session resume <id>` and retry",
        )
            .into_response();
    };

    let Some(guest_ip) = state.services.host.guest_ip(sandbox_id).await else {
        // FC has no networking wired (see docs/known-issues.md);
        // VZ may also return None briefly between create and the
        // first DHCP lease landing. Either way the right thing is
        // a 503 with a hint, not an empty WS.
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "guest_ip not available — shell tab is VZ-only and may need a moment after boot",
        )
            .into_response();
    };

    let target = format!("ws://{guest_ip}:{GUEST_TTYD_PORT}/ws");
    let hub = state.harness_hub.clone();
    upgrade
        .protocols(["tty"]) // ttyd advertises the "tty" subprotocol
        .on_upgrade(move |socket| async move {
            // Pin the sandbox against idle eviction for the lifetime
            // of the WebSocket. Opening a shell is unambiguous user
            // intent — the human is debugging and doesn't want the
            // VM yanked out from under them. Released on bridge exit
            // (success or error) so a closed tab doesn't leak the
            // keep-alive forever. Reference-counted in the hub so a
            // future second client doesn't decrement to zero
            // prematurely.
            hub.acquire_shell(sandbox_id);
            let bridge_result = bridge(socket, target.clone()).await;
            hub.release_shell(sandbox_id);
            if let Err(e) = bridge_result {
                tracing::warn!(
                    session_id = %id,
                    target = %target,
                    error = %e,
                    "shell proxy ended with error",
                );
            }
        })
}

async fn bridge(browser: WebSocket, target: String) -> Result<(), String> {
    // Open the upstream WebSocket. ttyd validates the
    // `Sec-WebSocket-Protocol` header (it expects "tty"), so we
    // mirror it on the way in.
    //
    // ttyd inside the guest takes a few seconds to bind on :7681
    // after kernel boot — the same boot race agentd handles in the
    // exec path. On a freshly-resumed sandbox (auto-resume just
    // cold-booted a new VM), the very first dashboard click will
    // race the bind. Retry connection-refused for up to ~8 seconds
    // before bubbling the error to the browser.
    let build_request = || {
        let mut request = target
            .as_str()
            .into_client_request()
            .map_err(|e| format!("bad ttyd url: {e}"))?;
        request
            .headers_mut()
            .insert("sec-websocket-protocol", "tty".parse().unwrap());
        Ok::<_, String>(request)
    };

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(8);
    let mut backoff = std::time::Duration::from_millis(100);
    let upstream = loop {
        let request = build_request()?;
        match tokio_tungstenite::connect_async(request).await {
            Ok((ws, _resp)) => break ws,
            Err(e) => {
                let msg = e.to_string();
                let is_refused = msg.contains("Connection refused");
                if !is_refused || std::time::Instant::now() >= deadline {
                    return Err(format!("connect to ttyd: {e}"));
                }
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(std::time::Duration::from_millis(800));
            }
        }
    };

    let (mut up_sink, mut up_stream) = upstream.split();
    let (mut br_sink, mut br_stream) = browser.split();

    // Browser → ttyd: copy each axum frame as a tungstenite frame,
    // exiting on close or error from either side.
    let b2t = async {
        while let Some(msg) = br_stream.next().await {
            let msg = msg.map_err(|e| format!("browser recv: {e}"))?;
            let tung = match msg {
                AxumMessage::Text(t) => TungMessage::Text(t),
                AxumMessage::Binary(b) => TungMessage::Binary(b),
                AxumMessage::Ping(b) => TungMessage::Ping(b),
                AxumMessage::Pong(b) => TungMessage::Pong(b),
                AxumMessage::Close(Some(cf)) => TungMessage::Close(Some(TungCloseFrame {
                    code: cf.code.into(),
                    reason: cf.reason,
                })),
                AxumMessage::Close(None) => TungMessage::Close(None),
            };
            up_sink
                .send(tung)
                .await
                .map_err(|e| format!("ttyd send: {e}"))?;
        }
        Ok::<(), String>(())
    };

    // ttyd → browser: same shape, opposite direction. Convert each
    // tungstenite Message into an axum Message; close translates to
    // a clean upgrade close so the browser sees the connection end
    // cleanly.
    let t2b = async {
        while let Some(msg) = up_stream.next().await {
            let msg = msg.map_err(|e| format!("ttyd recv: {e}"))?;
            let axum = match msg {
                TungMessage::Text(t) => AxumMessage::Text(t),
                TungMessage::Binary(b) => AxumMessage::Binary(b),
                TungMessage::Ping(b) => AxumMessage::Ping(b),
                TungMessage::Pong(b) => AxumMessage::Pong(b),
                TungMessage::Close(Some(cf)) => {
                    use axum::extract::ws::CloseFrame as AxumCloseFrame;
                    AxumMessage::Close(Some(AxumCloseFrame {
                        code: cf.code.into(),
                        reason: cf.reason,
                    }))
                }
                TungMessage::Close(None) => AxumMessage::Close(None),
                TungMessage::Frame(_) => continue,
            };
            br_sink
                .send(axum)
                .await
                .map_err(|e| format!("browser send: {e}"))?;
        }
        Ok::<(), String>(())
    };

    // First side to finish (or error) takes the connection down.
    // Forwarder errors are logged but not bubbled — both directions
    // share fate.
    tokio::select! {
        r = b2t => r,
        r = t2b => r,
    }
}
