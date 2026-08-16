//! ADR 0118: the same-session app-to-app short circuit.
//!
//! A guest that dials a sibling app's public hostname is trying to reach a
//! process on the same machine. Left alone that call would resolve off-box,
//! cross the internet, hit the load balancer, come back through the
//! orchestrator and the coordinator, and land on the host it started from —
//! and it would need a credential to get past the login wall on the way.
//!
//! Instead the proxy recognises one of the calling session's OWN app hostnames
//! at SNI-peek time and splices the connection straight back into the sibling's
//! guest port. One host hop, no DNS lookup off-box, no credential: the two apps
//! are already in one trust domain.
//!
//! The proxy cannot reach a guest port itself — that needs the sandbox backend
//! and the ADR 0066 vsock relay, neither of which this crate knows about. So it
//! takes a [`GuestPortDialer`], which `engram-host-agent` implements over
//! `SandboxBackend::open_guest_stream`.

use std::sync::Arc;

use async_trait::async_trait;
use engram_core::types::egress::AppEndpoint;
use engram_core::SandboxId;
use rustls::ServerConfig;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio_rustls::LazyConfigAcceptor;

use crate::guest_gateway::TunnelStream;
use crate::replayed::Replayed;

/// Opens a byte stream to a port inside a session's guest.
///
/// Implemented by the host-agent over the ADR 0066 vsock relay. Kept as a
/// narrow trait rather than a dependency on the sandbox backend so this crate
/// stays free of the FC/VZ/Process split, and so tests can drive the short
/// circuit against an ordinary loopback listener.
#[async_trait]
pub trait GuestPortDialer: Send + Sync {
    /// Connect to `port` inside `sandbox`'s guest.
    async fn dial(&self, sandbox: SandboxId, port: u16) -> std::io::Result<Box<dyn TunnelStream>>;
}

/// A dialer that refuses every call.
///
/// The default when the host-agent has not installed one, so the short circuit
/// is inert rather than half-wired: a proxy with no dialer simply never matches
/// an app hostname and every request takes the ordinary egress path.
pub struct NoGuestPortDialer;

#[async_trait]
impl GuestPortDialer for NoGuestPortDialer {
    async fn dial(
        &self,
        _sandbox: SandboxId,
        _port: u16,
    ) -> std::io::Result<Box<dyn TunnelStream>> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "no guest-port dialer installed",
        ))
    }
}

/// Shared handle to the installed dialer.
pub type SharedGuestPortDialer = Arc<dyn GuestPortDialer>;

/// The guest port a session's own app hostname resolves to, or `None`.
///
/// Matching is on the FULL hostname and is case-insensitive, because SNI
/// carries the whole name. Comparing whole strings is what makes the match
/// exact: a label-only compare would have to reconstruct the base domain and
/// could be fooled by a suffix near-miss such as
/// `api-x.preview.example.com.evil.test`.
///
/// Note the lookup is scoped to ONE session's apps — the caller has already
/// resolved the source IP to its `SessionState` — so a guest can only ever
/// short-circuit into its own sandbox, never another session's.
pub fn own_app_port(apps: &[AppEndpoint], sni: &str) -> Option<u16> {
    let sni = sni.trim_end_matches('.').to_ascii_lowercase();
    apps.iter()
        .find(|a| a.hostname.eq_ignore_ascii_case(&sni))
        .map(|a| a.port)
}

/// The HTTP/1.1 answer for an app whose port refused the connection.
///
/// Plain text on purpose: this is read by a browser AND by `curl` from inside
/// the guest, and the useful content is one sentence plus the port number.
/// ALPN is declined for this connection, so the client is speaking HTTP/1.1.
pub fn app_unreachable_response(port: u16) -> Vec<u8> {
    let body = format!(
        "This session app is reserved, but nothing is listening on port {port} inside the \
         session.\n\nStart the process that serves it, then reload. If it is already \
         running, check that it binds 0.0.0.0 or 127.0.0.1 on that exact port.\n"
    );
    format!(
        "HTTP/1.1 502 Bad Gateway\r\n\
         Content-Type: text/plain; charset=utf-8\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         \r\n\
         {body}",
        body.len()
    )
    .into_bytes()
}

/// Serve one short-circuited connection.
///
/// The guest opened a TLS connection to a sibling app's public hostname. We
/// terminate that TLS with a leaf minted by the CA already in the guest's trust
/// store — the same mechanism the secret/inject paths use — and then splice the
/// plaintext straight into the sibling's guest port.
///
/// The splice is RAW: no HTTP parsing, so WebSocket upgrades and h2c pass
/// through unchanged. One consequence follows and is deliberate — the callee
/// sees the app's real hostname in `Host`, where the browser-facing edge sends
/// `localhost:<port>`. Rewriting it would mean parsing HTTP and giving up the
/// protocol transparency the splice exists for. In practice the services
/// reached this way are APIs, which do not check `Host`; the ones that do are
/// dev servers, which are callers rather than callees.
pub async fn serve<C>(
    client_stream: C,
    peeked: Vec<u8>,
    sandbox_id: SandboxId,
    port: u16,
    dialer: &dyn GuestPortDialer,
    server_cfg: Arc<ServerConfig>,
) -> std::io::Result<(u64, u64)>
where
    C: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    // Complete the guest handshake BEFORE dialing the sibling.
    //
    // The reverse order is tempting — why handshake for a connection that is
    // about to fail? — and it is what shipped first. It produced a bad bug on
    // the day this went live (2026-08-16): the dev server simply was not up
    // yet, the dial was refused, and the connection closed before any
    // certificate was presented. In a browser that renders as
    // ERR_CONNECTION_CLOSED, and with curl as SSL_ERROR_SYSCALL after the
    // ClientHello. Both read as "the platform is blocking this hostname"
    // rather than "my app is not listening", and an agent spent an afternoon
    // hand-patching an auth allow-list chasing it.
    //
    // We own this hostname and mint a leaf the guest already trusts, so
    // finishing the handshake first lets a refused dial answer with a plain
    // 502 that says which port was dead. One wasted handshake buys an error a
    // human can act on.
    //
    // Stitch the peeked bytes back on so the acceptor sees the ClientHello from
    // byte 0 (the SNI peek already consumed them).
    let stitched = Replayed::new(peeked, client_stream);
    let start = LazyConfigAcceptor::new(rustls::server::Acceptor::default(), stitched)
        .await
        .map_err(|e| std::io::Error::other(format!("own-app tls accept: {e}")))?;
    // No ALPN negotiation to mirror: the far side is a plaintext guest port, so
    // there is no upstream TLS leg whose protocol we must match. Leaving the
    // list empty declines ALPN, and every client falls back to HTTP/1.1 — which
    // is what a plaintext dev server speaks anyway.
    let mut connection_cfg = (*server_cfg).clone();
    connection_cfg.alpn_protocols.clear();
    let mut client_tls = start
        .into_stream(Arc::new(connection_cfg))
        .await
        .map_err(|e| std::io::Error::other(format!("own-app tls handshake: {e}")))?;

    let mut guest = match dialer.dial(sandbox_id, port).await {
        Ok(guest) => guest,
        Err(e) => {
            // The app is declared but nothing answers on its port — almost
            // always "the dev server is not running yet". Say so, in the
            // browser, instead of dropping the connection.
            tracing::info!(
                port,
                error = %e,
                "own-app short circuit: nothing listening on the app's port"
            );
            let _ = client_tls.write_all(&app_unreachable_response(port)).await;
            let _ = client_tls.flush().await;
            // Shut the TLS session down properly. Without close_notify a client
            // treats the EOF as a truncation attack and surfaces an error
            // INSTEAD of the body — which would put us straight back to an
            // unreadable failure, the whole thing this branch exists to fix.
            let _ = client_tls.shutdown().await;
            return Ok((0, 0));
        }
    };

    tokio::io::copy_bidirectional(&mut client_tls, &mut guest).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(hostname: &str, port: u16) -> AppEndpoint {
        AppEndpoint {
            hostname: hostname.into(),
            port,
        }
    }

    #[test]
    fn matches_the_session_own_app_case_insensitively() {
        let apps = vec![
            app("web-tidy-swift-otters.preview.example.com", 3000),
            app("api-tidy-swift-otters.preview.example.com", 8080),
        ];
        assert_eq!(
            own_app_port(&apps, "api-tidy-swift-otters.preview.example.com"),
            Some(8080)
        );
        assert_eq!(
            own_app_port(&apps, "API-TIDY-SWIFT-OTTERS.PREVIEW.EXAMPLE.COM"),
            Some(8080)
        );
    }

    #[test]
    fn tolerates_a_fully_qualified_trailing_dot() {
        let apps = vec![app("api-x.preview.example.com", 8080)];
        assert_eq!(
            own_app_port(&apps, "api-x.preview.example.com."),
            Some(8080)
        );
    }

    #[test]
    fn a_suffix_near_miss_never_matches() {
        // The reason the match is on the whole hostname: an attacker-chosen
        // name that merely CONTAINS an app's name must not short-circuit into
        // that app's port.
        let apps = vec![app("api-x.preview.example.com", 8080)];
        for sni in [
            "api-x.preview.example.com.evil.test",
            "evil-api-x.preview.example.com",
            "preview.example.com",
            "api-y.preview.example.com",
        ] {
            assert_eq!(own_app_port(&apps, sni), None, "must not match {sni}");
        }
    }

    #[test]
    fn a_session_with_no_apps_never_short_circuits() {
        assert_eq!(own_app_port(&[], "anything.preview.example.com"), None);
    }

    /// The dial used to happen BEFORE the handshake, so a dev server that was
    /// not up yet closed the connection with no certificate — indistinguishable
    /// from the platform blocking the hostname. It cost a real afternoon on
    /// 2026-08-16. The answer must be a readable HTTP response instead.
    #[test]
    fn unreachable_response_names_the_port_and_is_well_formed() {
        let raw = app_unreachable_response(5173);
        let text = String::from_utf8(raw).expect("response is utf-8");
        let (head, body) = text
            .split_once("\r\n\r\n")
            .expect("headers end with a blank line");

        assert!(head.starts_with("HTTP/1.1 502 Bad Gateway\r\n"));
        assert!(head.contains("Connection: close"));
        // The port is the actionable half of the message.
        assert!(body.contains("5173"), "body must name the port: {body}");
        // Content-Length must match the body exactly, or the client hangs
        // waiting for bytes that never come.
        let declared: usize = head
            .lines()
            .find_map(|l| l.strip_prefix("Content-Length: "))
            .expect("Content-Length present")
            .parse()
            .expect("Content-Length is a number");
        assert_eq!(declared, body.len());
    }
}
