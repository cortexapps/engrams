//! TLS MITM + secret substitution.
//!
//! For Intercept decisions: terminate TLS with a per-SNI leaf cert
//! signed by our CA (which the guest already trusts via the substrate
//! install), decrypt incoming requests, scan/substitute placeholders,
//! re-encrypt to upstream. Responses stream back unchanged.
//!
//! Notes on shape:
//!
//! - **Length-Content limits.** A naïve implementation buffers the
//!   entire request in memory before substituting, which makes
//!   chunked or streaming uploads (image gen, file upload) blow up.
//!   We cap the buffered prefix at 1 MiB; once we cross that mark
//!   without seeing a placeholder, we stop scanning and stream
//!   through. The intuition: secrets are short (<200 bytes) and
//!   appear in headers or small JSON bodies; an attacker trying to
//!   hide a placeholder past the 1 MiB mark would need cooperation
//!   from the upstream service to receive it, which is the same
//!   threat model SSRF protections deal with elsewhere.
//! - **HTTP/1.1 and HTTP/2.** HTTP/2 streams are gated independently and keep
//!   the brokered authorization header on the host side. This includes gRPC.
//! - **Reqs vs responses.** We rewrite client→upstream only.
//!   Responses stream back as-is (Cloudflare etc. may stuff things
//!   in headers but they don't carry our placeholders).

use std::sync::Arc;

use rustls::pki_types::ServerName;
use rustls::{ClientConfig, ServerConfig};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::{TlsAcceptor, TlsConnector};

use engram_core::SessionId;

use crate::cert_mint::CertMint;
use crate::graphql::{self, ParsedGraphql};
use crate::inject;
use crate::observe::{self, ObserveSink};
use crate::registry::{InjectEntry, InjectRefresher, ObserveEntry, SecretEntry};
use crate::replayed::Replayed;
use crate::resolver::{ResolveError, UpstreamResolver};
use crate::substitute::{scan_for_violation, substitute};

/// Cap on the request prefix we buffer before falling back to
/// straight relay. 1 MiB covers every reasonable case — headers +
/// JSON bodies are tens of KiB at the high end. Streaming uploads
/// past this mark just don't get scanned (and a streaming upload
/// with a placeholder at byte 1M+1 is not a realistic attack).
const SCAN_BUDGET: usize = 1024 * 1024;

/// ADR 0059: cap on the GraphQL request body we buffer before gating. A GraphQL
/// endpoint's body must be read in full before we can decide (we cannot
/// stream-then-revoke), so this bounds per-connection memory. GraphQL query
/// documents are a few KiB; 256 KiB is generous. Only requests to a declared
/// GraphQL endpoint pay this — REST/bypass hosts keep the header-only early stop.
const GRAPHQL_REQUEST_BODY_BUDGET: usize = 256 * 1024;

#[derive(Debug)]
pub enum InterceptError {
    Io(std::io::Error),
    Tls(rustls::Error),
    H2(h2::Error),
    Mint(crate::cert_mint::MintError),
    Resolve(ResolveError),
    Violation {
        placeholder: String,
    },
    /// ADR 0056: an inject-gated host got a request whose (method, path)
    /// matched no injection's `RequestPolicy` — the operation isn't permitted.
    RequestRejected {
        method: String,
        path: String,
    },
    /// ADR 0056: an inject-gated request had no parseable HTTP/1.1 request line.
    MalformedRequest,
    /// ADR 0059: a GraphQL request to a gated endpoint was rejected — the body was
    /// unparseable / over-cap / unsupported framing, or its operation+field isn't
    /// permitted by the integration policy. `reason` is a static tag for logs
    /// (never the request body).
    GraphqlRejected {
        reason: &'static str,
    },
    InjectHeader(crate::inject::InjectHeaderError),
    InvalidServerName(String),
    InvalidInjectedHeader(String),
    CredentialRequestRejected,
}

impl std::fmt::Display for InterceptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "io: {e}"),
            Self::Tls(e) => write!(f, "tls: {e}"),
            Self::H2(e) => write!(f, "http2: {e}"),
            Self::Mint(e) => write!(f, "mint: {e}"),
            Self::Resolve(e) => write!(f, "resolve: {e}"),
            Self::Violation { placeholder } => {
                write!(f, "placeholder leak: {placeholder} sent to disallowed host")
            }
            Self::RequestRejected { method, path } => {
                write!(f, "request rejected by integration policy: {method} {path}")
            }
            Self::MalformedRequest => {
                write!(f, "malformed request line on an inject-gated host")
            }
            Self::GraphqlRejected { reason } => {
                write!(
                    f,
                    "graphql request rejected by integration policy: {reason}"
                )
            }
            Self::InjectHeader(e) => write!(f, "credential injection rejected: {e}"),
            Self::InvalidServerName(s) => write!(f, "invalid SNI `{s}`"),
            Self::InvalidInjectedHeader(name) => {
                write!(f, "invalid injected HTTP header `{name}`")
            }
            Self::CredentialRequestRejected => {
                write!(f, "credential-producing Google API request rejected")
            }
        }
    }
}

impl std::error::Error for InterceptError {}

impl From<std::io::Error> for InterceptError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<rustls::Error> for InterceptError {
    fn from(e: rustls::Error) -> Self {
        Self::Tls(e)
    }
}

impl From<h2::Error> for InterceptError {
    fn from(e: h2::Error) -> Self {
        Self::H2(e)
    }
}

impl From<crate::inject::InjectHeaderError> for InterceptError {
    fn from(e: crate::inject::InjectHeaderError) -> Self {
        Self::InjectHeader(e)
    }
}

impl From<crate::cert_mint::MintError> for InterceptError {
    fn from(e: crate::cert_mint::MintError) -> Self {
        Self::Mint(e)
    }
}

impl From<ResolveError> for InterceptError {
    fn from(e: ResolveError) -> Self {
        Self::Resolve(e)
    }
}

/// Build a rustls server config that uses `mint.leaf_for(sni)` to
/// answer ClientHellos. Static — built once, shared across
/// connections.
pub fn build_server_config(mint: Arc<CertMint>) -> Arc<ServerConfig> {
    let resolver = SniResolver { mint };
    let mut cfg = ServerConfig::builder()
        .with_no_client_auth()
        .with_cert_resolver(Arc::new(resolver));
    cfg.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Arc::new(cfg)
}

/// Build a rustls client config with Mozilla's WebPKI roots. Upstream identity
/// verification is mandatory before the proxy can attach a host-side secret.
pub fn build_client_config() -> Arc<ClientConfig> {
    let roots = rustls::RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let mut cfg = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    cfg.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Arc::new(cfg)
}

struct SniResolver {
    mint: Arc<CertMint>,
}

impl std::fmt::Debug for SniResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SniResolver")
            .field("cache_size", &self.mint.cache_size())
            .finish()
    }
}

impl rustls::server::ResolvesServerCert for SniResolver {
    fn resolve(
        &self,
        client_hello: rustls::server::ClientHello<'_>,
    ) -> Option<Arc<rustls::sign::CertifiedKey>> {
        let sni = client_hello.server_name()?;
        self.mint.leaf_for(sni).ok()
    }
}

/// Drive a MITM intercept on `client_stream`. Bytes already peeked
/// during SNI extraction are stitched back at the front via
/// [`crate::replayed::Replayed`]. The upstream is dialed by SNI
/// through `resolver`, not by guest-supplied IP — see the
/// `resolver` module for why.
#[allow(clippy::too_many_arguments)]
pub async fn run<C>(
    client_stream: C,
    peeked: Vec<u8>,
    sni: &str,
    port: u16,
    resolver: Arc<dyn UpstreamResolver>,
    secrets: &[&SecretEntry],
    injects: &[&InjectEntry],
    observes: &[&ObserveEntry],
    session_id: SessionId,
    sink: Option<&ObserveSink>,
    refresher: Option<&dyn InjectRefresher>,
    server_cfg: Arc<ServerConfig>,
    client_cfg: Arc<ClientConfig>,
) -> Result<(), InterceptError>
where
    C: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    // Stitch the peeked bytes back onto the client stream so the
    // TLS acceptor sees the full ClientHello from byte 0.
    let stitched = Replayed::new(peeked, client_stream);
    let acceptor = TlsAcceptor::from(server_cfg);
    let mut client_tls = acceptor.accept(stitched).await?;

    // Resolve upstream by SNI on the host's resolver. The SNI
    // doubles as the ServerName for the upstream TLS handshake —
    // that's the host the cert chain should authenticate.
    let upstream_addr = resolver.resolve(sni, port).await?;
    let upstream_tcp = TcpStream::connect(upstream_addr).await?;
    let connector = TlsConnector::from(client_cfg);
    let server_name: ServerName<'static> = ServerName::try_from(sni.to_string())
        .map_err(|_| InterceptError::InvalidServerName(sni.to_string()))?;
    let mut upstream_tls = connector.connect(server_name, upstream_tcp).await?;

    let client_protocol = client_tls.get_ref().1.alpn_protocol();
    let upstream_protocol = upstream_tls.get_ref().1.alpn_protocol();
    if client_protocol == Some(b"h2") {
        if upstream_protocol != Some(b"h2") {
            return Err(InterceptError::Tls(rustls::Error::General(
                "upstream did not negotiate HTTP/2".into(),
            )));
        }
        return run_h2(
            client_tls,
            upstream_tls,
            sni,
            secrets,
            injects,
            session_id,
            refresher,
        )
        .await;
    }

    // Buffer the request prefix up to SCAN_BUDGET, scan for
    // violations + substitute placeholders, then forward + bidir
    // copy the rest.
    let mut prefix = Vec::with_capacity(8192);
    while prefix.len() < SCAN_BUDGET {
        let mut chunk = [0u8; 8192];
        let n = client_tls.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        prefix.extend_from_slice(&chunk[..n]);
        // Don't keep buffering once the request body has clearly
        // ended. For HTTP/1.1 a reasonable signal is a CRLF-CRLF
        // followed by Content-Length bytes — but parsing that here
        // would be overkill. The simple heuristic: stop once we see
        // an `\r\n\r\n` AND the buffer is < 64 KiB (typical headers
        // + small body). Anything bigger keeps reading.
        if prefix.len() >= 64 * 1024
            || (prefix.windows(4).any(|w| w == b"\r\n\r\n") && prefix.len() < 64 * 1024)
        {
            break;
        }
    }

    // Parse the request line once if any inject/observe gating needs it
    // (method + path are stable across header injection + substitution).
    let req_line = if !injects.is_empty() || !observes.is_empty() {
        inject::request_line(&prefix)
    } else {
        None
    };
    if let Some((method, path)) = &req_line {
        if rejects_google_credential_request(sni, method, path) {
            return Err(InterceptError::CredentialRequestRejected);
        }
    }

    // ADR 0059: is this request destined for a declared GraphQL endpoint? (Any
    // inject/observe entry that carries a GraphQL matcher AND whose path glob
    // matches this request's path — i.e. `POST /graphql`.) Only then do we buffer
    // + parse the body; REST/bypass hosts keep the header-only early stop above.
    let is_graphql = match &req_line {
        Some((_, path)) => {
            injects
                .iter()
                .any(|i| i.policy.graphql.is_some() && i.policy.path_matches(path))
                || observes
                    .iter()
                    .any(|o| o.policy.graphql.is_some() && o.policy.path_matches(path))
        }
        None => false,
    };

    // ADR 0059: for a GraphQL endpoint, read the FULL request body (bounded) and
    // parse it into its top-level operation/fields. Fail-closed: anything we can't
    // read or parse cleanly rejects the request before a byte reaches upstream.
    // `gh` (and standard GraphQL clients) send a Content-Length'd, identity-encoded
    // JSON body — chunked/compressed bodies are denied.
    let parsed_graphql: Option<ParsedGraphql> = if is_graphql {
        let headers_end = prefix.windows(4).position(|w| w == b"\r\n\r\n").ok_or(
            InterceptError::GraphqlRejected {
                reason: "no header terminator",
            },
        )?;
        let head = std::str::from_utf8(&prefix[..headers_end]).map_err(|_| {
            InterceptError::GraphqlRejected {
                reason: "non-utf8 headers",
            }
        })?;
        let content_length = graphql_content_length(head)?;
        let body_start = headers_end + 4;
        let body_end = body_start + content_length;
        read_to_content_length(&mut client_tls, &mut prefix, body_end).await?;
        let body = &prefix[body_start..body_end];
        Some(
            graphql::parse_request_body(body).ok_or(InterceptError::GraphqlRejected {
                reason: "unparseable or unsupported graphql operation",
            })?,
        )
    } else {
        None
    };

    // ADR 0056/0059 (Plane B): an inject-gated host must satisfy a request policy.
    // REST: gate by (method, path) glob. GraphQL: every top-level field must be
    // covered by some granted GraphQL inject (set coverage). A request matching no
    // injection is rejected — the operation isn't permitted on this host.
    if !injects.is_empty() {
        let (method, path) = req_line.clone().ok_or(InterceptError::MalformedRequest)?;
        let matched: Vec<&InjectEntry> = if let Some(doc) = &parsed_graphql {
            gate_graphql_injects(injects, &method, &path, doc).ok_or(
                InterceptError::GraphqlRejected {
                    reason: "operation not permitted by integration policy",
                },
            )?
        } else {
            let m: Vec<&InjectEntry> = injects
                .iter()
                .copied()
                .filter(|i| i.policy.graphql.is_none() && i.policy.allows(&method, &path))
                .collect();
            if m.is_empty() {
                return Err(InterceptError::RequestRejected { method, path });
            }
            m
        };
        // WS4: re-mint any near-expiry minted credential BEFORE injecting it, so a
        // long-lived session never sends a stale (expired ~1h post-boot)
        // installation token — the campaign's reads-401/writes-succeed asymmetry.
        // Single-flighted per entry. Generic providers keep a still-present
        // stale secret on refresh failure. Google entries revalidate on every
        // request and fail closed so connection disablement is immediate.
        if let Some(refresher) = refresher {
            for e in &matched {
                if !e.refresh_for_request(session_id, refresher).await {
                    return Err(InterceptError::CredentialRequestRejected);
                }
            }
        }
        prefix = inject::inject_headers(prefix, &matched)?;
    }

    if let Some(ph) = scan_for_violation(&prefix, sni, secrets) {
        return Err(InterceptError::Violation {
            placeholder: ph.to_string(),
        });
    }
    prefix = substitute(prefix, sni, secrets);

    // ADR 0056 (Phase 4) / 0059: observe the response for any observe spec whose
    // request shape matches — REST by (method, path); GraphQL by a top-level
    // (operation, field) in the parsed body. Only when a sink is wired (the
    // host-agent's bridge to the coordinator) — without a consumer there's no
    // point buffering the response.
    let firing: Vec<&ObserveEntry> = match (&req_line, sink) {
        (Some((method, path)), Some(_)) => observes
            .iter()
            .copied()
            .filter(|o| match (&o.policy.graphql, &parsed_graphql) {
                (Some(g), Some(doc)) => {
                    o.policy.method_matches(method)
                        && o.policy.path_matches(path)
                        && doc
                            .top_level
                            .iter()
                            .any(|(op, field)| g.matches(*op, field))
                }
                (None, _) => o.policy.allows(method, path),
                // A GraphQL observe spec never fires on a non-GraphQL request.
                (Some(_), None) => false,
            })
            .collect(),
        _ => Vec::new(),
    };

    if !firing.is_empty() {
        let (method, path) = req_line.expect("firing observes imply a parsed request line");
        let sink = sink.expect("firing observes imply a sink");
        // Force identity encoding + a single, close-delimited response so the
        // observation completes on upstream EOF without keep-alive bookkeeping.
        prefix = observe::prepare_observed_request(prefix);
        upstream_tls.write_all(&prefix).await?;
        upstream_tls.flush().await?;

        let resp_buf = pump_and_observe(client_tls, upstream_tls, OBSERVE_RESPONSE_BUDGET).await;
        let parsed = observe::parse_response(&resp_buf);
        for o in &firing {
            if let Some(asset) = observe::evaluate(o, parsed.as_ref(), &method, &path) {
                sink(session_id, asset);
            }
        }
        return Ok(());
    }

    // The gate/inject/substitute passes above saw only THIS request; anything
    // the client sends after the buffered prefix streams verbatim below. Force
    // `Connection: close` so a keep-alive client can't ride request #2 through
    // ungated with the guest's placeholder credential (see
    // `observe::force_connection_close`).
    prefix = observe::force_connection_close(prefix);
    upstream_tls.write_all(&prefix).await?;
    upstream_tls.flush().await?;

    // Stream the rest in both directions; client→upstream is
    // bytes-after-prefix (no further substitution — a request body mid-stream
    // is fine, a second request dies with the connection), upstream→client
    // is everything.
    tokio::io::copy_bidirectional(&mut client_tls, &mut upstream_tls).await?;

    // Explicit close_notify on both sides. Without this rustls
    // peers see "peer closed without close_notify" when they read
    // the trailing bytes — that's noisy in logs and causes tests
    // (legitimately checking for clean shutdown) to fail.
    let _ = client_tls.shutdown().await;
    let _ = upstream_tls.shutdown().await;
    Ok(())
}

/// Forward one HTTP/2 connection. Every stream gets an independent policy
/// decision and authorization overwrite, so gRPC cannot reuse an approved
/// stream to reach a second method.
async fn run_h2<C, U>(
    client_tls: C,
    upstream_tls: U,
    sni: &str,
    secrets: &[&SecretEntry],
    injects: &[&InjectEntry],
    session_id: SessionId,
    refresher: Option<&dyn InjectRefresher>,
) -> Result<(), InterceptError>
where
    C: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    U: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let mut inbound = h2::server::handshake(client_tls).await?;
    let (mut outbound, connection) = h2::client::handshake(upstream_tls).await?;
    let upstream = tokio::spawn(connection);

    while let Some(request) = inbound.accept().await {
        let (request, mut respond) = request?;
        let method = request.method().as_str().to_string();
        let path = request
            .uri()
            .path_and_query()
            .map_or_else(|| "/".to_string(), ToString::to_string);
        if rejects_google_credential_request(sni, &method, &path) {
            return Err(InterceptError::CredentialRequestRejected);
        }

        let matched: Vec<&InjectEntry> = injects
            .iter()
            .copied()
            .filter(|entry| entry.policy.graphql.is_none() && entry.policy.allows(&method, &path))
            .collect();
        if !injects.is_empty() && matched.is_empty() {
            return Err(InterceptError::RequestRejected { method, path });
        }
        if let Some(refresher) = refresher {
            for entry in &matched {
                if !entry.refresh_for_request(session_id, refresher).await {
                    return Err(InterceptError::CredentialRequestRejected);
                }
            }
        }

        let (mut parts, mut request_body) = request.into_parts();
        inject_h2_headers(&mut parts.headers, &matched)?;
        if h2_headers_contain_disallowed_placeholder(&parts.headers, sni, secrets) {
            return Err(InterceptError::Violation {
                placeholder: "redacted".to_string(),
            });
        }

        outbound = outbound.ready().await?;
        let request_end = request_body.is_end_stream();
        let request = http::Request::from_parts(parts, ());
        let (response, mut upstream_body) = outbound.send_request(request, request_end)?;
        let forward_request = async move {
            if !request_end {
                while let Some(data) = request_body.data().await {
                    let data = data?;
                    let len = data.len();
                    request_body.flow_control().release_capacity(len)?;
                    upstream_body.send_data(data, false)?;
                }
                if let Some(trailers) = request_body.trailers().await? {
                    upstream_body.send_trailers(trailers)?;
                } else {
                    upstream_body.send_data(bytes::Bytes::new(), true)?;
                }
            }
            Ok::<(), h2::Error>(())
        };

        let forward_response = async move {
            let response = response.await?;
            let (parts, mut response_body) = response.into_parts();
            let response_end = response_body.is_end_stream();
            let mut client_body =
                respond.send_response(http::Response::from_parts(parts, ()), response_end)?;
            if !response_end {
                while let Some(data) = response_body.data().await {
                    let data = data?;
                    let len = data.len();
                    response_body.flow_control().release_capacity(len)?;
                    client_body.send_data(data, false)?;
                }
                if let Some(trailers) = response_body.trailers().await? {
                    client_body.send_trailers(trailers)?;
                } else {
                    client_body.send_data(bytes::Bytes::new(), true)?;
                }
            }
            Ok::<(), h2::Error>(())
        };
        tokio::try_join!(forward_request, forward_response)?;
    }

    upstream.abort();
    Ok(())
}

fn inject_h2_headers(
    headers: &mut http::HeaderMap,
    entries: &[&InjectEntry],
) -> Result<(), InterceptError> {
    let mut rendered: Vec<(http::HeaderName, http::HeaderValue)> = Vec::new();
    for entry in entries {
        let name = http::HeaderName::from_bytes(entry.header_name.as_bytes())
            .map_err(|_| InterceptError::InvalidInjectedHeader(entry.header_name.clone()))?;
        let value =
            http::HeaderValue::from_str(&entry.header_template.replace("{}", &entry.secret()))
                .map_err(|_| InterceptError::InvalidInjectedHeader(entry.header_name.clone()))?;
        if let Some((_, existing)) = rendered.iter().find(|(candidate, _)| candidate == name) {
            if existing != value {
                return Err(InterceptError::InjectHeader(
                    crate::inject::InjectHeaderError::ConflictingValues {
                        header_name: entry.header_name.clone(),
                    },
                ));
            }
            continue;
        }
        rendered.push((name, value));
    }
    for (name, value) in rendered {
        headers.remove(&name);
        headers.insert(name, value);
    }
    Ok(())
}

fn h2_headers_contain_disallowed_placeholder(
    headers: &http::HeaderMap,
    sni: &str,
    secrets: &[&SecretEntry],
) -> bool {
    headers.values().any(|value| {
        value.to_str().is_ok_and(|value| {
            secrets
                .iter()
                .any(|secret| value.contains(&secret.placeholder) && !secret.allow.matches(sni))
        })
    })
}

/// Google STS and OAuth are never guest surfaces. IAM credential minting is
/// denied even when an administrator selects a broad googleapis.com endpoint.
fn rejects_google_credential_request(sni: &str, method: &str, path: &str) -> bool {
    if sni.eq_ignore_ascii_case("sts.googleapis.com")
        || sni.eq_ignore_ascii_case("oauth2.googleapis.com")
        || sni.eq_ignore_ascii_case("accounts.google.com")
        || sni.eq_ignore_ascii_case("securetoken.googleapis.com")
        || sni.eq_ignore_ascii_case("iamcredentials.googleapis.com")
    {
        return true;
    }
    let path = path.to_ascii_lowercase();
    let operation_path = path.split('?').next().unwrap_or(&path);
    if sni.to_ascii_lowercase().ends_with(".googleapis.com")
        && [
            ":generateaccesstoken",
            ":generateidtoken",
            ":signblob",
            ":signjwt",
        ]
        .iter()
        .any(|operation| operation_path.ends_with(operation))
    {
        return true;
    }
    if sni.eq_ignore_ascii_case("www.googleapis.com")
        && path.starts_with("/oauth2/")
        && path
            .split('?')
            .next()
            .is_some_and(|value| value.ends_with("/token"))
    {
        return true;
    }
    if !method.eq_ignore_ascii_case("POST") {
        return false;
    }
    (sni.eq_ignore_ascii_case("iam.googleapis.com")
        && (path.contains("/serviceaccountkeys")
            || (path.contains("/serviceaccounts/")
                && path.split('?').next().is_some_and(|p| p.ends_with("/keys")))))
        || (sni.eq_ignore_ascii_case("identitytoolkit.googleapis.com")
            && [":signin", ":signup"]
                .iter()
                .any(|operation| path.contains(operation)))
}

/// ADR 0059: GraphQL set-coverage gate. Returns the inject entries whose auth
/// header should be applied iff EVERY top-level field in `doc` is covered by some
/// granted GraphQL inject whose method + path also match. Fail-closed: an empty
/// document, or ANY uncovered field, → `None` (the caller rejects the request).
fn gate_graphql_injects<'a>(
    injects: &[&'a InjectEntry],
    method: &str,
    path: &str,
    doc: &ParsedGraphql,
) -> Option<Vec<&'a InjectEntry>> {
    if doc.top_level.is_empty() {
        return None;
    }
    let candidates: Vec<&'a InjectEntry> = injects
        .iter()
        .copied()
        .filter(|i| {
            i.policy.graphql.is_some()
                && i.policy.method_matches(method)
                && i.policy.path_matches(path)
        })
        .collect();
    let mut applied: Vec<&'a InjectEntry> = Vec::new();
    for (op, field) in &doc.top_level {
        let m = candidates.iter().copied().find(|i| {
            i.policy
                .graphql
                .as_ref()
                .is_some_and(|g| g.matches(*op, field))
        })?;
        if !applied.iter().any(|a| std::ptr::eq(*a, m)) {
            applied.push(m);
        }
    }
    Some(applied)
}

/// ADR 0059: extract + validate the Content-Length of a GraphQL request from its
/// header block. Fail-closed: a chunked or non-identity-encoded body, an over-cap
/// length, or a missing Content-Length → `Err` (we can't safely bound + read it).
/// `gh` and standard GraphQL clients always send an identity Content-Length'd body.
fn graphql_content_length(head: &str) -> Result<usize, InterceptError> {
    let mut content_length: Option<usize> = None;
    let mut lines = head.split("\r\n");
    let _ = lines.next(); // request line
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let name = name.trim().to_ascii_lowercase();
        let value = value.trim();
        match name.as_str() {
            "content-length" => content_length = value.parse::<usize>().ok(),
            "transfer-encoding" if value.to_ascii_lowercase().contains("chunked") => {
                return Err(InterceptError::GraphqlRejected {
                    reason: "chunked graphql body unsupported",
                });
            }
            "content-encoding" if !value.eq_ignore_ascii_case("identity") => {
                return Err(InterceptError::GraphqlRejected {
                    reason: "compressed graphql body",
                });
            }
            _ => {}
        }
    }
    match content_length {
        Some(n) if n <= GRAPHQL_REQUEST_BODY_BUDGET => Ok(n),
        Some(_) => Err(InterceptError::GraphqlRejected {
            reason: "graphql body exceeds cap",
        }),
        None => Err(InterceptError::GraphqlRejected {
            reason: "graphql request without content-length",
        }),
    }
}

/// Read from `client_tls` into `prefix` until it holds `body_end` bytes (the body
/// per Content-Length). Fail-closed on a truncated stream (client EOF before the
/// declared length). `body_end` is already known `<= headers + cap`.
async fn read_to_content_length<C>(
    client_tls: &mut C,
    prefix: &mut Vec<u8>,
    body_end: usize,
) -> Result<(), InterceptError>
where
    C: AsyncRead + Unpin,
{
    while prefix.len() < body_end {
        let mut chunk = [0u8; 8192];
        let n = client_tls.read(&mut chunk).await?;
        if n == 0 {
            return Err(InterceptError::GraphqlRejected {
                reason: "truncated graphql body",
            });
        }
        prefix.extend_from_slice(&chunk[..n]);
    }
    Ok(())
}

/// Cap on the response prefix buffered for observation. Asset-bearing JSON
/// responses (issue/PR/query metadata) are a few KiB; 256 KiB is generous.
/// Bytes past the cap still stream to the client — only the *tapped* copy is
/// bounded.
const OBSERVE_RESPONSE_BUDGET: usize = 256 * 1024;

/// Forward the response (upstream→client) while tapping a bounded copy for
/// observation, and concurrently forward client→upstream so an upstream that
/// withholds its response pending the request body can't deadlock us. Returns
/// the tapped response bytes once the upstream closes (forced promptly by the
/// `Connection: close` we set on the request).
async fn pump_and_observe<C, U>(client_tls: C, upstream_tls: U, budget: usize) -> Vec<u8>
where
    C: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    U: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (client_rd, mut client_wr) = tokio::io::split(client_tls);
    let (mut up_rd, up_wr) = tokio::io::split(upstream_tls);

    // Detached: drain any remaining request body to upstream. Aborted once the
    // response is in — by then upstream has the full request (it responded).
    let c2u = tokio::spawn(async move {
        let mut client_rd = client_rd;
        let mut up_wr = up_wr;
        let _ = tokio::io::copy(&mut client_rd, &mut up_wr).await;
        let _ = up_wr.shutdown().await;
    });

    let mut resp_buf = Vec::new();
    let mut tmp = [0u8; 8192];
    loop {
        let n = match up_rd.read(&mut tmp).await {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        if client_wr.write_all(&tmp[..n]).await.is_err() {
            break;
        }
        if resp_buf.len() < budget {
            let take = (budget - resp_buf.len()).min(n);
            resp_buf.extend_from_slice(&tmp[..take]);
        }
    }
    let _ = client_wr.flush().await;
    let _ = client_wr.shutdown().await;
    c2u.abort();
    resp_buf
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::HostList;
    use crate::registry::{RefreshableCred, RequestPolicy};

    #[test]
    fn denies_google_credential_production_surfaces() {
        assert!(rejects_google_credential_request(
            "sts.googleapis.com",
            "POST",
            "/v1/token"
        ));
        assert!(rejects_google_credential_request(
            "oauth2.googleapis.com",
            "GET",
            "/token"
        ));
        assert!(rejects_google_credential_request(
            "accounts.google.com",
            "GET",
            "/o/oauth2/auth"
        ));
        assert!(rejects_google_credential_request(
            "www.googleapis.com",
            "POST",
            "/oauth2/v4/token"
        ));
        for method in [
            ":generateAccessToken",
            ":generateIdToken",
            ":signBlob",
            ":signJwt",
        ] {
            assert!(rejects_google_credential_request(
                "iamcredentials.googleapis.com",
                "POST",
                &format!("/v1/projects/-/serviceAccounts/a@example.com{method}")
            ));
        }
        assert!(rejects_google_credential_request(
            "iam.googleapis.com",
            "POST",
            "/v1/projects/-/serviceAccounts/a@example.com:signJwt"
        ));
        assert!(rejects_google_credential_request(
            "iam.googleapis.com",
            "POST",
            "/v1/projects/p/serviceAccounts/a/keys"
        ));
        assert!(rejects_google_credential_request(
            "identitytoolkit.googleapis.com",
            "POST",
            "/v1/accounts:signInWithCustomToken"
        ));
        assert!(!rejects_google_credential_request(
            "compute.googleapis.com",
            "POST",
            "/compute/v1/projects/p/zones/z/instances/i/start"
        ));
    }

    #[test]
    fn h2_authorization_is_overwritten() {
        let entry = InjectEntry {
            header_name: "authorization".into(),
            header_template: "Bearer {}".into(),
            allow: HostList::from_manifest(&["compute.googleapis.com".into()], &[]).unwrap(),
            policy: RequestPolicy::default(),
            mint_provider: "gcp|c|compute.instances.get|compute.googleapis.com".into(),
            cred: RefreshableCred::new("host-token".into(), None),
        };
        let mut headers = http::HeaderMap::new();
        headers.insert(
            "authorization",
            http::HeaderValue::from_static("Bearer guest-value"),
        );
        inject_h2_headers(&mut headers, &[&entry]).unwrap();
        assert_eq!(headers.get("authorization").unwrap(), "Bearer host-token");
    }

    #[tokio::test]
    async fn proxies_a_grpc_style_h2_request_with_host_authorization() {
        let (guest_io, proxy_guest_io) = tokio::io::duplex(16 * 1024);
        let (proxy_upstream_io, upstream_io) = tokio::io::duplex(16 * 1024);

        let proxy = tokio::spawn(async move {
            let entry = InjectEntry {
                header_name: "authorization".into(),
                header_template: "Bearer {}".into(),
                allow: HostList::from_manifest(&["logging.googleapis.com".into()], &[]).unwrap(),
                policy: RequestPolicy {
                    methods: vec!["POST".into()],
                    path_globs: vec![
                        "segment:/google.logging.v2.LoggingServiceV2/ListLogEntries".into()
                    ],
                    graphql: None,
                },
                mint_provider: "gcp|c|logging.entries.list|logging.googleapis.com".into(),
                cred: RefreshableCred::new("host-token".into(), None),
            };
            run_h2(
                proxy_guest_io,
                proxy_upstream_io,
                "logging.googleapis.com",
                &[],
                &[&entry],
                SessionId::new(),
                None,
            )
            .await
        });

        let upstream = tokio::spawn(async move {
            let mut server = h2::server::handshake(upstream_io).await.unwrap();
            let (request, mut respond) = server.accept().await.unwrap().unwrap();
            assert_eq!(request.method(), http::Method::POST);
            assert_eq!(
                request.uri().path(),
                "/google.logging.v2.LoggingServiceV2/ListLogEntries"
            );
            assert_eq!(
                request.headers().get("authorization").unwrap(),
                "Bearer host-token"
            );
            assert_eq!(
                request.headers().get("content-type").unwrap(),
                "application/grpc"
            );
            let mut body = request.into_body();
            let data = body.data().await.unwrap().unwrap();
            body.flow_control().release_capacity(data.len()).unwrap();
            assert_eq!(data, bytes::Bytes::from_static(b"grpc-request"));

            let response = http::Response::builder()
                .status(200)
                .header("content-type", "application/grpc")
                .body(())
                .unwrap();
            let mut response_body = respond.send_response(response, false).unwrap();
            response_body
                .send_data(bytes::Bytes::from_static(b"grpc-response"), true)
                .unwrap();
            // Keep polling the server connection until the proxy closes it so
            // the queued response frames reach the guest.
            while server.accept().await.is_some() {}
        });

        let (mut guest, guest_connection) = h2::client::handshake(guest_io).await.unwrap();
        let guest_connection = tokio::spawn(guest_connection);
        guest = guest.ready().await.unwrap();
        let request = http::Request::builder()
            .method("POST")
            .uri("https://logging.googleapis.com/google.logging.v2.LoggingServiceV2/ListLogEntries")
            .header("content-type", "application/grpc")
            .header("authorization", "Bearer guest-token")
            .body(())
            .unwrap();
        let (response, mut request_body) = guest.send_request(request, false).unwrap();
        request_body
            .send_data(bytes::Bytes::from_static(b"grpc-request"), true)
            .unwrap();
        let response = response.await.unwrap();
        assert_eq!(response.status(), 200);
        let mut response_body = response.into_body();
        let data = response_body.data().await.unwrap().unwrap();
        response_body
            .flow_control()
            .release_capacity(data.len())
            .unwrap();
        assert_eq!(data, bytes::Bytes::from_static(b"grpc-response"));

        drop(guest);
        guest_connection.abort();
        upstream.await.unwrap();
        proxy.abort();
    }
}
