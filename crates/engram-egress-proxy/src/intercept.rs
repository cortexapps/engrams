//! TLS MITM + secret substitution.
//!
//! For Intercept decisions: terminate TLS with a per-SNI leaf cert
//! signed by our CA (which the guest already trusts via the substrate
//! install), decrypt incoming requests, scan/substitute placeholders,
//! re-encrypt to upstream. Credential-bearing responses are forced to identity
//! encoding and redacted before they return to the guest.
//!
//! Notes on shape:
//!
//! - **Request buffering.** The HTTP/1 adapter buffers a bounded request prefix
//!   for policy evaluation and credential replacement. It forces one request
//!   per connection. A later transport-hardening layer will replace this with
//!   framed, streaming request processing.
//! - **HTTP/1.1 and HTTP/2.** Each HTTP/2 stream gets an independent policy
//!   decision. This preserves the same credential boundary for gRPC.
//! - **Response safety.** A host-issued credential must not reach the guest,
//!   even when an upstream reflects it. The adapter strips content negotiation,
//!   rejects encoded responses, and redacts credential bytes across chunks.

use std::sync::Arc;

use futures_util::stream::{FuturesUnordered, StreamExt};
use rustls::pki_types::ServerName;
use rustls::{ClientConfig, ServerConfig};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::{LazyConfigAcceptor, TlsConnector};

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
    /// The request target can carry an authority only in proxy form. This proxy
    /// authenticates the authority through SNI, so it accepts only origin form.
    InvalidRequestTarget,
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
    /// Google API calls that create reusable credentials are never a guest
    /// surface, even when a broad endpoint policy matches.
    CredentialRequestRejected {
        method: String,
        path: String,
    },
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
                write!(f, "malformed HTTP/1.1 request line")
            }
            Self::InvalidRequestTarget => {
                write!(f, "request target is not in origin form")
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
            Self::CredentialRequestRejected { method, path } => {
                write!(
                    f,
                    "credential-producing Google API request rejected: {method} {path}"
                )
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

/// Build the production upstream TLS config from Mozilla's WebPKI roots.
/// A host-side credential is attached only after rustls authenticates the
/// upstream certificate for the SNI-selected server name.
pub fn build_client_config() -> Arc<ClientConfig> {
    let roots = rustls::RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    build_client_config_with_roots(roots)
}

/// Build an upstream TLS config from an explicit trust store. Production uses
/// [`build_client_config`]. Full-network tests use a private CA so they can
/// exercise certificate verification without external network access.
pub fn build_client_config_with_roots(roots: rustls::RootCertStore) -> Arc<ClientConfig> {
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
    // Stitch the peeked bytes back onto the client stream so the TLS acceptor
    // sees the full ClientHello from byte 0. Stop after parsing the hello: the
    // upstream must select a protocol before this leg promises one to the
    // guest. Otherwise an H2-capable guest and an HTTP/1.1-only upstream can
    // leave the proxy with two incompatible TLS legs.
    let stitched = Replayed::new(peeked, client_stream);
    let start = LazyConfigAcceptor::new(rustls::server::Acceptor::default(), stitched).await?;
    let guest_protocols: Vec<Vec<u8>> = start
        .client_hello()
        .alpn()
        .into_iter()
        .flatten()
        .map(<[u8]>::to_vec)
        .collect();

    // Resolve upstream by SNI on the host's resolver. The SNI
    // doubles as the ServerName for the upstream TLS handshake —
    // that's the host the cert chain should authenticate.
    let upstream_addr = resolver.resolve(sni, port).await?;
    let upstream_tcp = TcpStream::connect(upstream_addr).await?;
    let mut connection_client_cfg = (*client_cfg).clone();
    connection_client_cfg
        .alpn_protocols
        .retain(|protocol| guest_protocols.contains(protocol));
    let connector = TlsConnector::from(Arc::new(connection_client_cfg));
    let server_name: ServerName<'static> = ServerName::try_from(sni.to_string())
        .map_err(|_| InterceptError::InvalidServerName(sni.to_string()))?;
    let mut upstream_tls = connector.connect(server_name, upstream_tcp).await?;

    let upstream_protocol = upstream_tls.get_ref().1.alpn_protocol().map(<[u8]>::to_vec);
    let mut connection_server_cfg = (*server_cfg).clone();
    connection_server_cfg.alpn_protocols = upstream_protocol.iter().cloned().collect();
    let mut client_tls = start.into_stream(Arc::new(connection_server_cfg)).await?;
    let client_protocol = client_tls.get_ref().1.alpn_protocol();
    let upstream_protocol = upstream_protocol.as_deref();
    if client_protocol != upstream_protocol {
        return Err(InterceptError::Tls(rustls::Error::General(
            "client and upstream negotiated different application protocols".into(),
        )));
    }
    if client_protocol == Some(b"h2") {
        return run_h2(
            client_tls,
            upstream_tls,
            H2Context {
                sni,
                port,
                secrets,
                injects,
                observes,
                session_id,
                sink,
                refresher,
            },
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

    // Policy and upstream TLS identity are selected by SNI. Bind the cleartext
    // HTTP authority to that same name before any credential is attached.
    prefix = bind_http1_authority(prefix, &upstream_authority(sni, port));

    let parsed_request_line =
        inject::request_line(&prefix).ok_or(InterceptError::MalformedRequest)?;
    if !request_target_is_origin_form(&parsed_request_line.1) {
        return Err(InterceptError::InvalidRequestTarget);
    }
    if rejects_google_credential_request(sni, &parsed_request_line.0, &parsed_request_line.1) {
        return Err(InterceptError::CredentialRequestRejected {
            method: parsed_request_line.0.clone(),
            path: path_without_query(&parsed_request_line.1).to_string(),
        });
    }
    let head_request = parsed_request_line.0.eq_ignore_ascii_case("HEAD");

    // Parse the request line once if any inject/observe gating needs it
    // (method + path are stable across header injection + substitution).
    let req_line = if !injects.is_empty() || !observes.is_empty() {
        Some(parsed_request_line)
    } else {
        None
    };

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
    // Any host-side credential that this request can send must be removed from
    // the response. This includes both injected credentials and static secrets
    // that replace guest placeholders.
    let mut response_redactions: Vec<Vec<u8>> = secrets
        .iter()
        .filter(|entry| entry.allow.matches(sni))
        .map(|entry| entry.real_value.as_bytes().to_vec())
        .filter(|secret| !secret.is_empty())
        .collect();
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
        // Single-flighted per entry; on refresh failure the stale secret is kept
        // (see `InjectEntry::refresh_if_stale`). Static entries are a no-op.
        if let Some(refresher) = refresher {
            for e in &matched {
                e.refresh_if_stale(session_id, refresher).await;
            }
        }
        response_redactions.extend(
            matched
                .iter()
                .map(|entry| entry.secret().into_bytes())
                .filter(|secret| !secret.is_empty()),
        );
        response_redactions.sort_unstable();
        response_redactions.dedup();
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

        let resp_buf = pump_and_observe(
            client_tls,
            upstream_tls,
            OBSERVE_RESPONSE_BUDGET,
            &response_redactions,
            head_request,
        )
        .await?;
        let parsed = observe::parse_response(&resp_buf);
        // The GraphQL request variables feed the entries' `$.vars.*` extractors
        // (mutation inputs the response won't echo); `None` for REST requests.
        let gql_vars = parsed_graphql.as_ref().and_then(|p| p.variables.as_ref());
        for o in &firing {
            if let Some(asset) = observe::evaluate(o, parsed.as_ref(), &method, &path, gql_vars) {
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
    prefix = if response_redactions.is_empty() {
        observe::force_connection_close(prefix)
    } else {
        // Identity encoding makes the response inspectable and also forces the
        // connection closed. A non-identity response is rejected below.
        observe::prepare_observed_request(prefix)
    };
    upstream_tls.write_all(&prefix).await?;
    upstream_tls.flush().await?;

    // Stream the rest in both directions; client→upstream is
    // bytes-after-prefix (no further substitution — a request body mid-stream
    // is fine, a second request dies with the connection), upstream→client
    // is everything.
    let (mut client_read, mut client_write) = tokio::io::split(client_tls);
    let (mut upstream_read, mut upstream_write) = tokio::io::split(upstream_tls);
    let request = async {
        tokio::io::copy(&mut client_read, &mut upstream_write).await?;
        upstream_write.shutdown().await
    };
    let response = copy_redacting_response(
        &mut upstream_read,
        &mut client_write,
        &response_redactions,
        head_request,
    );
    tokio::try_join!(request, response)?;
    Ok(())
}

/// Data shared by every stream on one authenticated HTTP/2 connection.
struct H2Context<'a> {
    sni: &'a str,
    port: u16,
    secrets: &'a [&'a SecretEntry],
    injects: &'a [&'a InjectEntry],
    observes: &'a [&'a ObserveEntry],
    session_id: SessionId,
    sink: Option<&'a ObserveSink>,
    refresher: Option<&'a dyn InjectRefresher>,
}

/// Forward one HTTP/2 connection. A denied stream gets its own response. Other
/// streams continue, and slow streams do not block new policy decisions.
async fn run_h2<C, U>(
    client_tls: C,
    upstream_tls: U,
    context: H2Context<'_>,
) -> Result<(), InterceptError>
where
    C: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    U: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let mut inbound = h2::server::handshake(client_tls).await?;
    let (outbound, connection) = h2::client::handshake(upstream_tls).await?;
    let upstream = tokio::spawn(connection);
    let mut active = FuturesUnordered::new();
    let mut accepting = true;

    while accepting || !active.is_empty() {
        tokio::select! {
            incoming = inbound.accept(), if accepting => {
                match incoming {
                    Some(Ok((request, respond))) => {
                        active.push(process_h2_stream(
                            request,
                            respond,
                            outbound.clone(),
                            &context,
                        ));
                    }
                    Some(Err(error)) => return Err(error.into()),
                    None => accepting = false,
                }
            }
            completed = active.next(), if !active.is_empty() => {
                if let Some(Err(error)) = completed {
                    tracing::warn!(
                        %error,
                        %context.session_id,
                        target = context.sni,
                        "HTTP/2 stream forwarding failed",
                    );
                }
            }
        }
    }

    upstream.abort();
    let _ = upstream.await;
    Ok(())
}

async fn process_h2_stream(
    request: http::Request<h2::RecvStream>,
    mut respond: h2::server::SendResponse<bytes::Bytes>,
    mut outbound: h2::client::SendRequest<bytes::Bytes>,
    context: &H2Context<'_>,
) -> Result<(), InterceptError> {
    let method = request.method().as_str().to_string();
    let path = request
        .uri()
        .path_and_query()
        .map_or_else(|| "/".to_string(), ToString::to_string);
    if rejects_google_credential_request(context.sni, &method, &path) {
        deny_h2(&mut respond, http::StatusCode::FORBIDDEN)?;
        return Ok(());
    }
    let is_graphql = context
        .injects
        .iter()
        .any(|entry| entry.policy.graphql.is_some() && entry.policy.path_matches(&path))
        || context
            .observes
            .iter()
            .any(|entry| entry.policy.graphql.is_some() && entry.policy.path_matches(&path));

    let (mut parts, mut request_body) = request.into_parts();
    let (buffered_body, request_trailers, parsed_graphql) = if is_graphql {
        match read_h2_body(&mut request_body, GRAPHQL_REQUEST_BODY_BUDGET).await {
            Ok((body, trailers)) => {
                let parsed = match graphql::parse_request_body(&body) {
                    Some(parsed) => parsed,
                    None => {
                        deny_h2(&mut respond, http::StatusCode::BAD_REQUEST)?;
                        return Ok(());
                    }
                };
                (Some(body), trailers, Some(parsed))
            }
            Err(_) => {
                deny_h2(&mut respond, http::StatusCode::PAYLOAD_TOO_LARGE)?;
                return Ok(());
            }
        }
    } else {
        (None, None, None)
    };

    let matched: Vec<&InjectEntry> = if let Some(document) = &parsed_graphql {
        match gate_graphql_injects(context.injects, &method, &path, document) {
            Some(entries) => entries,
            None => {
                deny_h2(&mut respond, http::StatusCode::FORBIDDEN)?;
                return Ok(());
            }
        }
    } else {
        context
            .injects
            .iter()
            .copied()
            .filter(|entry| entry.policy.graphql.is_none() && entry.policy.allows(&method, &path))
            .collect()
    };
    if !context.injects.is_empty() && matched.is_empty() {
        deny_h2(&mut respond, http::StatusCode::FORBIDDEN)?;
        return Ok(());
    }
    if let Some(refresher) = context.refresher {
        for entry in &matched {
            entry.refresh_if_stale(context.session_id, refresher).await;
        }
    }

    let firing: Vec<&ObserveEntry> = context
        .observes
        .iter()
        .copied()
        .filter(|entry| match (&entry.policy.graphql, &parsed_graphql) {
            (Some(graphql_match), Some(document)) => {
                entry.policy.method_matches(&method)
                    && entry.policy.path_matches(&path)
                    && document
                        .top_level
                        .iter()
                        .any(|(operation, field)| graphql_match.matches(*operation, field))
            }
            (None, _) => entry.policy.allows(&method, &path),
            (Some(_), None) => false,
        })
        .collect();

    transform_h2_headers(&mut parts.headers, context.sni, context.secrets)?;
    inject_h2_headers(&mut parts.headers, &matched)?;
    let response_redactions = response_redactions(context.sni, context.secrets, &matched);
    if !response_redactions.is_empty() || !firing.is_empty() {
        parts.headers.remove(http::header::ACCEPT_ENCODING);
        parts.headers.remove("grpc-accept-encoding");
    }
    if context
        .secrets
        .iter()
        .any(|entry| entry.allow.matches(context.sni))
    {
        parts.headers.remove(http::header::CONTENT_LENGTH);
    }

    let authority = upstream_authority(context.sni, context.port);
    let mut uri_parts = parts.uri.into_parts();
    uri_parts.authority = Some(
        authority
            .parse()
            .map_err(|_| InterceptError::InvalidServerName(authority.clone()))?,
    );
    parts.uri = http::Uri::from_parts(uri_parts)
        .map_err(|_| InterceptError::InvalidServerName(authority))?;
    parts.headers.remove(http::header::HOST);

    outbound = outbound.ready().await?;
    let request_end = buffered_body.is_none() && request_body.is_end_stream();
    let (response, mut upstream_body) =
        outbound.send_request(http::Request::from_parts(parts, ()), request_end)?;

    let forward_request = async {
        if let Some(body) = buffered_body {
            let body = transform_complete(body, context.sni, context.secrets)?;
            let end = request_trailers.is_none();
            send_h2_data(&mut upstream_body, body.into(), end).await?;
            if let Some(mut trailers) = request_trailers {
                transform_h2_headers(&mut trailers, context.sni, context.secrets)?;
                upstream_body.send_trailers(trailers)?;
            }
        } else if !request_end {
            let mut transformer = PlaceholderTransformer::new(context.sni, context.secrets);
            while let Some(data) = request_body.data().await {
                let data = data?;
                let len = data.len();
                request_body.flow_control().release_capacity(len)?;
                let output = transformer.push(&data, false)?;
                if !output.is_empty() {
                    send_h2_data(&mut upstream_body, output.into(), false).await?;
                }
            }
            let tail = transformer.push(&[], true)?;
            if !tail.is_empty() {
                send_h2_data(&mut upstream_body, tail.into(), false).await?;
            }
            if let Some(mut trailers) = request_body.trailers().await? {
                transform_h2_headers(&mut trailers, context.sni, context.secrets)?;
                upstream_body.send_trailers(trailers)?;
            } else {
                send_h2_data(&mut upstream_body, bytes::Bytes::new(), true).await?;
            }
        }
        Ok::<(), InterceptError>(())
    };

    let forward_response = async {
        let response = response.await?;
        let status = response.status().as_u16();
        let (mut response_parts, mut response_body) = response.into_parts();
        if (!response_redactions.is_empty() || !firing.is_empty())
            && !h2_response_is_inspectable(&response_parts.headers)
        {
            deny_h2(&mut respond, http::StatusCode::BAD_GATEWAY)?;
            return Ok(());
        }
        redact_h2_headers(&mut response_parts.headers, &response_redactions);
        let response_end = response_body.is_end_stream();
        let mut client_body =
            respond.send_response(http::Response::from_parts(response_parts, ()), response_end)?;
        let mut redactor = ByteRedactor::new(&response_redactions);
        let mut observed_body = Vec::new();
        if !response_end {
            while let Some(data) = response_body.data().await {
                let data = data?;
                let len = data.len();
                response_body.flow_control().release_capacity(len)?;
                let output = redactor.push(&data, false);
                if observed_body.len() < OBSERVE_RESPONSE_BUDGET {
                    let take = (OBSERVE_RESPONSE_BUDGET - observed_body.len()).min(output.len());
                    observed_body.extend_from_slice(&output[..take]);
                }
                if !output.is_empty() {
                    send_h2_data(&mut client_body, output.into(), false).await?;
                }
            }
            let tail = redactor.push(&[], true);
            if observed_body.len() < OBSERVE_RESPONSE_BUDGET {
                let take = (OBSERVE_RESPONSE_BUDGET - observed_body.len()).min(tail.len());
                observed_body.extend_from_slice(&tail[..take]);
            }
            if !tail.is_empty() {
                send_h2_data(&mut client_body, tail.into(), false).await?;
            }
            if let Some(mut trailers) = response_body.trailers().await? {
                redact_h2_headers(&mut trailers, &response_redactions);
                client_body.send_trailers(trailers)?;
            } else {
                send_h2_data(&mut client_body, bytes::Bytes::new(), true).await?;
            }
        }

        if let Some(sink) = context.sink {
            let parsed = observe::ParsedResponse {
                status,
                body: observed_body,
            };
            let variables = parsed_graphql
                .as_ref()
                .and_then(|document| document.variables.as_ref());
            for entry in &firing {
                if let Some(asset) =
                    observe::evaluate(entry, Some(&parsed), &method, &path, variables)
                {
                    sink(context.session_id, asset);
                }
            }
        }
        Ok::<(), InterceptError>(())
    };

    tokio::try_join!(forward_request, forward_response)?;
    Ok(())
}

fn deny_h2(
    respond: &mut h2::server::SendResponse<bytes::Bytes>,
    status: http::StatusCode,
) -> Result<(), InterceptError> {
    let response = http::Response::builder()
        .status(status)
        .body(())
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error.to_string()))?;
    respond.send_response(response, true)?;
    Ok(())
}

async fn read_h2_body(
    body: &mut h2::RecvStream,
    budget: usize,
) -> Result<(Vec<u8>, Option<http::HeaderMap>), InterceptError> {
    let mut output = Vec::new();
    while let Some(data) = body.data().await {
        let data = data?;
        let len = data.len();
        body.flow_control().release_capacity(len)?;
        if output.len().saturating_add(len) > budget {
            return Err(InterceptError::GraphqlRejected {
                reason: "body exceeds limit",
            });
        }
        output.extend_from_slice(&data);
    }
    Ok((output, body.trailers().await?))
}

async fn send_h2_data(
    stream: &mut h2::SendStream<bytes::Bytes>,
    mut data: bytes::Bytes,
    end_stream: bool,
) -> Result<(), InterceptError> {
    if data.is_empty() {
        stream.send_data(data, end_stream)?;
        return Ok(());
    }
    while !data.is_empty() {
        stream.reserve_capacity(data.len());
        let capacity = futures_util::future::poll_fn(|cx| stream.poll_capacity(cx))
            .await
            .ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::BrokenPipe, "HTTP/2 stream closed")
            })??;
        let take = capacity.min(data.len());
        let chunk = data.split_to(take);
        stream.send_data(chunk, end_stream && data.is_empty())?;
    }
    Ok(())
}

fn transform_complete(
    value: Vec<u8>,
    sni: &str,
    secrets: &[&SecretEntry],
) -> Result<Vec<u8>, InterceptError> {
    if let Some(placeholder) = scan_for_violation(&value, sni, secrets) {
        return Err(InterceptError::Violation {
            placeholder: placeholder.to_string(),
        });
    }
    Ok(substitute(value, sni, secrets))
}

fn transform_h2_headers(
    headers: &mut http::HeaderMap,
    sni: &str,
    secrets: &[&SecretEntry],
) -> Result<(), InterceptError> {
    for (name, value) in headers.iter_mut() {
        let transformed = transform_complete(value.as_bytes().to_vec(), sni, secrets)?;
        if transformed != value.as_bytes() {
            *value = http::HeaderValue::from_bytes(&transformed)
                .map_err(|_| InterceptError::InvalidInjectedHeader(name.to_string()))?;
        }
    }
    Ok(())
}

fn inject_h2_headers(
    headers: &mut http::HeaderMap,
    entries: &[&InjectEntry],
) -> Result<(), InterceptError> {
    for (name, value) in inject::rendered_headers(entries)? {
        let name = http::HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| InterceptError::InvalidInjectedHeader(name.clone()))?;
        let value = http::HeaderValue::from_str(&value)
            .map_err(|_| InterceptError::InvalidInjectedHeader(name.to_string()))?;
        headers.remove(&name);
        headers.insert(name, value);
    }
    Ok(())
}

fn response_redactions(
    sni: &str,
    secrets: &[&SecretEntry],
    injects: &[&InjectEntry],
) -> Vec<Vec<u8>> {
    let mut values: Vec<Vec<u8>> = secrets
        .iter()
        .filter(|entry| entry.allow.matches(sni))
        .map(|entry| entry.real_value.as_bytes().to_vec())
        .chain(injects.iter().map(|entry| entry.secret().into_bytes()))
        .filter(|value| !value.is_empty())
        .collect();
    values.sort_unstable();
    values.dedup();
    values
}

fn redact_h2_headers(headers: &mut http::HeaderMap, needles: &[Vec<u8>]) {
    for value in headers.values_mut() {
        let mut bytes = value.as_bytes().to_vec();
        redact_bytes(&mut bytes, needles);
        if bytes != value.as_bytes() {
            if let Ok(redacted) = http::HeaderValue::from_bytes(&bytes) {
                *value = redacted;
            }
        }
    }
}

fn h2_response_is_inspectable(headers: &http::HeaderMap) -> bool {
    [http::header::CONTENT_ENCODING.as_str(), "grpc-encoding"]
        .iter()
        .all(|name| {
            headers.get_all(*name).iter().all(|value| {
                value
                    .to_str()
                    .is_ok_and(|value| value.eq_ignore_ascii_case("identity"))
            })
        })
}

struct PlaceholderTransformer<'a> {
    sni: &'a str,
    secrets: &'a [&'a SecretEntry],
    pending: Vec<u8>,
    keep: usize,
}

impl<'a> PlaceholderTransformer<'a> {
    fn new(sni: &'a str, secrets: &'a [&'a SecretEntry]) -> Self {
        Self {
            sni,
            secrets,
            pending: Vec::new(),
            keep: secrets
                .iter()
                .map(|entry| entry.placeholder.len())
                .filter(|length| *length > 0)
                .max()
                .unwrap_or(1)
                .saturating_sub(1),
        }
    }

    fn push(&mut self, input: &[u8], final_chunk: bool) -> Result<Vec<u8>, InterceptError> {
        self.pending.extend_from_slice(input);
        let process_limit = if final_chunk {
            self.pending.len()
        } else {
            self.pending.len().saturating_sub(self.keep)
        };
        let mut output = Vec::new();
        let mut cursor = 0;
        while cursor < process_limit {
            let next = self
                .secrets
                .iter()
                .filter_map(|entry| {
                    let needle = entry.placeholder.as_bytes();
                    if needle.is_empty() {
                        return None;
                    }
                    self.pending[cursor..]
                        .windows(needle.len())
                        .position(|candidate| candidate == needle)
                        .map(|offset| (cursor + offset, *entry))
                })
                .filter(|(start, _)| *start < process_limit)
                .min_by_key(|(start, _)| *start);
            let Some((start, entry)) = next else {
                output.extend_from_slice(&self.pending[cursor..process_limit]);
                cursor = process_limit;
                break;
            };
            output.extend_from_slice(&self.pending[cursor..start]);
            if !entry.allow.matches(self.sni) {
                return Err(InterceptError::Violation {
                    placeholder: entry.placeholder.clone(),
                });
            }
            output.extend_from_slice(entry.real_value.as_bytes());
            cursor = start + entry.placeholder.len();
        }
        self.pending.drain(..cursor);
        Ok(output)
    }
}

/// Google STS and OAuth are never guest surfaces. IAM credential minting is
/// denied even when an administrator selects a broad Google API endpoint.
fn rejects_google_credential_request(sni: &str, method: &str, path: &str) -> bool {
    if sni.eq_ignore_ascii_case("sts.googleapis.com")
        || sni.eq_ignore_ascii_case("oauth2.googleapis.com")
        || sni.eq_ignore_ascii_case("accounts.google.com")
        || sni.eq_ignore_ascii_case("securetoken.googleapis.com")
        || sni.eq_ignore_ascii_case("iamcredentials.googleapis.com")
    {
        return true;
    }
    let operation_path = normalized_operation_path(path);
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
        && operation_path.starts_with("/oauth2/")
        && operation_path.ends_with("/token")
    {
        return true;
    }
    if !method.eq_ignore_ascii_case("POST") {
        return false;
    }
    (sni.eq_ignore_ascii_case("iam.googleapis.com")
        && (operation_path.contains("/serviceaccountkeys")
            || (operation_path.contains("/serviceaccounts/") && operation_path.ends_with("/keys"))))
        || (sni.eq_ignore_ascii_case("identitytoolkit.googleapis.com")
            && [":signin", ":signup"]
                .iter()
                .any(|operation| operation_path.contains(operation)))
}

fn normalized_operation_path(path: &str) -> String {
    let mut current = path.split('?').next().unwrap_or(path).as_bytes().to_vec();
    // A Google frontend can decode percent escapes while it routes a
    // transcoded API method. Check nested encodings before that router does.
    for _ in 0..3 {
        let mut decoded = Vec::with_capacity(current.len());
        let mut index = 0;
        while index < current.len() {
            if current[index] == b'%' && index + 2 < current.len() {
                let high = (current[index + 1] as char).to_digit(16);
                let low = (current[index + 2] as char).to_digit(16);
                if let (Some(high), Some(low)) = (high, low) {
                    decoded.push(((high << 4) | low) as u8);
                    index += 3;
                    continue;
                }
            }
            decoded.push(current[index]);
            index += 1;
        }
        if decoded == current {
            break;
        }
        current = decoded;
    }
    String::from_utf8_lossy(&current).to_ascii_lowercase()
}

fn path_without_query(path: &str) -> &str {
    path.split('?').next().unwrap_or(path)
}

fn upstream_authority(sni: &str, port: u16) -> String {
    if port == 443 {
        sni.to_string()
    } else {
        format!("{sni}:{port}")
    }
}

fn request_target_is_origin_form(target: &str) -> bool {
    target == "*" || target.starts_with('/')
}

/// Replace every guest-supplied Host header with the SNI-authenticated
/// authority. Duplicate or mixed-case Host headers cannot select a different
/// virtual host after policy evaluation.
fn bind_http1_authority(prefix: Vec<u8>, authority: &str) -> Vec<u8> {
    let Some(request_line_end) = prefix.windows(2).position(|value| value == b"\r\n") else {
        return prefix;
    };
    let insert_at = request_line_end + 2;
    let mut out = Vec::with_capacity(prefix.len() + authority.len() + 8);
    out.extend_from_slice(&prefix[..insert_at]);
    out.extend_from_slice(b"Host: ");
    out.extend_from_slice(authority.as_bytes());
    out.extend_from_slice(b"\r\n");

    let mut cursor = insert_at;
    loop {
        let Some(relative_end) = prefix[cursor..]
            .windows(2)
            .position(|value| value == b"\r\n")
        else {
            out.extend_from_slice(&prefix[cursor..]);
            break;
        };
        let line_end = cursor + relative_end;
        let line = &prefix[cursor..line_end];
        if line.is_empty() {
            out.extend_from_slice(&prefix[cursor..]);
            break;
        }
        let name = line
            .iter()
            .position(|byte| *byte == b':')
            .map_or(line, |colon| &line[..colon]);
        if !name.eq_ignore_ascii_case(b"host") {
            out.extend_from_slice(&prefix[cursor..line_end + 2]);
        }
        cursor = line_end + 2;
    }
    out
}

struct ByteRedactor {
    needles: Vec<Vec<u8>>,
    tail: Vec<u8>,
    keep: usize,
}

/// Tracks whether an HTTP/1 response has complete, self-delimiting framing.
///
/// Some Google frontends close HTTP/1 connections without a TLS `close_notify`.
/// Rustls correctly reports that as `UnexpectedEof`, but all authenticated
/// plaintext can still contain a complete HTTP response. We accept that EOF only
/// after this tracker sees a complete Content-Length or chunked body. A
/// close-delimited or incomplete response still fails closed.
struct Http1ResponseFraming {
    state: Http1ResponseState,
    head_request: bool,
}

enum Http1ResponseState {
    Headers(Vec<u8>),
    ContentLength(usize),
    Chunked(ChunkedFraming),
    CloseDelimited,
    Complete,
    Invalid,
}

enum ChunkedFraming {
    Size(Vec<u8>),
    Data(usize),
    DataCrlf(u8),
    TrailerLine(Vec<u8>),
    Complete,
    Invalid,
}

impl Http1ResponseFraming {
    fn new(head_request: bool) -> Self {
        Self {
            state: Http1ResponseState::Headers(Vec::new()),
            head_request,
        }
    }

    fn push(&mut self, mut input: &[u8]) {
        while !input.is_empty() {
            match &mut self.state {
                Http1ResponseState::Headers(buffer) => {
                    let take = input
                        .len()
                        .min(RESPONSE_HEADER_BUDGET.saturating_sub(buffer.len()));
                    buffer.extend_from_slice(&input[..take]);
                    input = &input[take..];
                    let Some(end) = buffer.windows(4).position(|value| value == b"\r\n\r\n") else {
                        if buffer.len() == RESPONSE_HEADER_BUDGET {
                            self.state = Http1ResponseState::Invalid;
                        }
                        continue;
                    };
                    let remainder = buffer.split_off(end + 4);
                    let head = &buffer[..end];
                    let Some(next) = response_body_framing(head, self.head_request) else {
                        self.state = Http1ResponseState::Invalid;
                        continue;
                    };
                    self.state = next;
                    self.push(&remainder);
                }
                Http1ResponseState::ContentLength(remaining) => {
                    let take = input.len().min(*remaining);
                    *remaining -= take;
                    input = &input[take..];
                    if *remaining == 0 {
                        self.state = if input.is_empty() {
                            Http1ResponseState::Complete
                        } else {
                            Http1ResponseState::Invalid
                        };
                    }
                }
                Http1ResponseState::Chunked(chunked) => {
                    let consumed = chunked.push(input);
                    input = &input[consumed..];
                    if matches!(chunked, ChunkedFraming::Complete) {
                        self.state = if input.is_empty() {
                            Http1ResponseState::Complete
                        } else {
                            Http1ResponseState::Invalid
                        };
                    } else if matches!(chunked, ChunkedFraming::Invalid) {
                        self.state = Http1ResponseState::Invalid;
                    }
                }
                Http1ResponseState::CloseDelimited => return,
                Http1ResponseState::Complete | Http1ResponseState::Invalid => {
                    self.state = Http1ResponseState::Invalid;
                    return;
                }
            }
        }
    }

    fn is_complete(&self) -> bool {
        matches!(self.state, Http1ResponseState::Complete)
    }
}

fn response_body_framing(head: &[u8], head_request: bool) -> Option<Http1ResponseState> {
    let text = std::str::from_utf8(head).ok()?;
    let mut lines = text.split("\r\n");
    let status = lines
        .next()?
        .split_ascii_whitespace()
        .nth(1)?
        .parse::<u16>()
        .ok()?;
    let mut content_length = None;
    let mut transfer_encoding = None;
    for line in lines {
        let (name, value) = line.split_once(':')?;
        if name.trim().eq_ignore_ascii_case("content-length") {
            let parsed = value.trim().parse::<usize>().ok()?;
            if content_length
                .replace(parsed)
                .is_some_and(|prior| prior != parsed)
            {
                return None;
            }
        } else if name.trim().eq_ignore_ascii_case("transfer-encoding") {
            transfer_encoding = Some(value.trim().to_ascii_lowercase());
        }
    }
    if (100..200).contains(&status) && status != 101 {
        return Some(Http1ResponseState::Headers(Vec::new()));
    }
    if head_request || status == 204 || status == 304 {
        return Some(Http1ResponseState::Complete);
    }
    if transfer_encoding.as_deref().is_some_and(|value| {
        value
            .split(',')
            .next_back()
            .is_some_and(|v| v.trim() == "chunked")
    }) {
        return Some(Http1ResponseState::Chunked(
            ChunkedFraming::Size(Vec::new()),
        ));
    }
    Some(match content_length {
        Some(0) => Http1ResponseState::Complete,
        Some(length) => Http1ResponseState::ContentLength(length),
        None => Http1ResponseState::CloseDelimited,
    })
}

impl ChunkedFraming {
    fn push(&mut self, input: &[u8]) -> usize {
        let mut consumed = 0;
        while consumed < input.len() {
            match self {
                Self::Size(line) => {
                    let byte = input[consumed];
                    consumed += 1;
                    line.push(byte);
                    if line.len() > 8192 {
                        *self = Self::Invalid;
                    } else if line.ends_with(b"\r\n") {
                        line.truncate(line.len() - 2);
                        let hex = line.split(|byte| *byte == b';').next().unwrap_or_default();
                        let Ok(text) = std::str::from_utf8(hex) else {
                            *self = Self::Invalid;
                            continue;
                        };
                        let Ok(size) = usize::from_str_radix(text.trim(), 16) else {
                            *self = Self::Invalid;
                            continue;
                        };
                        *self = if size == 0 {
                            Self::TrailerLine(Vec::new())
                        } else {
                            Self::Data(size)
                        };
                    }
                }
                Self::Data(remaining) => {
                    let take = (input.len() - consumed).min(*remaining);
                    *remaining -= take;
                    consumed += take;
                    if *remaining == 0 {
                        *self = Self::DataCrlf(0);
                    }
                }
                Self::DataCrlf(seen) => {
                    let expected = if *seen == 0 { b'\r' } else { b'\n' };
                    if input[consumed] != expected {
                        *self = Self::Invalid;
                    } else {
                        consumed += 1;
                        *seen += 1;
                        if *seen == 2 {
                            *self = Self::Size(Vec::new());
                        }
                    }
                }
                Self::TrailerLine(line) => {
                    let byte = input[consumed];
                    consumed += 1;
                    line.push(byte);
                    if line.len() > RESPONSE_HEADER_BUDGET {
                        *self = Self::Invalid;
                    } else if line.ends_with(b"\r\n") {
                        if line.len() == 2 {
                            *self = Self::Complete;
                        } else {
                            line.clear();
                        }
                    }
                }
                Self::Complete | Self::Invalid => return consumed,
            }
        }
        consumed
    }
}

impl ByteRedactor {
    fn new(needles: &[Vec<u8>]) -> Self {
        Self {
            needles: needles.to_vec(),
            tail: Vec::new(),
            keep: needles
                .iter()
                .map(Vec::len)
                .max()
                .unwrap_or(1)
                .saturating_sub(1),
        }
    }

    fn push(&mut self, input: &[u8], final_chunk: bool) -> Vec<u8> {
        self.tail.extend_from_slice(input);
        redact_bytes(&mut self.tail, &self.needles);
        let emit = if final_chunk {
            self.tail.len()
        } else {
            self.tail.len().saturating_sub(self.keep)
        };
        self.tail.drain(..emit).collect()
    }
}

fn redact_bytes(value: &mut [u8], needles: &[Vec<u8>]) {
    for needle in needles.iter().filter(|needle| !needle.is_empty()) {
        let mut start = 0;
        while start + needle.len() <= value.len() {
            let Some(relative) = value[start..]
                .windows(needle.len())
                .position(|candidate| candidate == needle)
            else {
                break;
            };
            let found = start + relative;
            value[found..found + needle.len()].fill(b'*');
            start = found + needle.len();
        }
    }
}

const RESPONSE_HEADER_BUDGET: usize = 64 * 1024;

fn response_headers_are_inspectable(headers: &[u8]) -> bool {
    let text = String::from_utf8_lossy(headers);
    text.lines().all(|line| {
        line.split_once(':').is_none_or(|(name, value)| {
            !name.trim().eq_ignore_ascii_case("content-encoding")
                || value.trim().eq_ignore_ascii_case("identity")
        })
    })
}

async fn read_response_prefix<R>(reader: &mut R) -> std::io::Result<Vec<u8>>
where
    R: AsyncRead + Unpin,
{
    let mut prefix = Vec::with_capacity(4096);
    while prefix.len() < RESPONSE_HEADER_BUDGET {
        let mut chunk = [0_u8; 4096];
        let read = reader.read(&mut chunk).await?;
        if read == 0 {
            break;
        }
        prefix.extend_from_slice(&chunk[..read]);
        if prefix.windows(4).any(|value| value == b"\r\n\r\n") {
            return Ok(prefix);
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        "upstream response headers exceed the inspection limit",
    ))
}

async fn copy_redacting_response<R, W>(
    reader: &mut R,
    writer: &mut W,
    needles: &[Vec<u8>],
    head_request: bool,
) -> std::io::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    if needles.is_empty() {
        tokio::io::copy(reader, writer).await?;
        return writer.shutdown().await;
    }

    let prefix = read_response_prefix(reader).await?;
    let headers_end = prefix
        .windows(4)
        .position(|value| value == b"\r\n\r\n")
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "incomplete response headers",
            )
        })?;
    if !response_headers_are_inspectable(&prefix[..headers_end]) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "credential-bearing response uses an unsupported content encoding",
        ));
    }

    let mut framing = Http1ResponseFraming::new(head_request);
    framing.push(&prefix);
    let mut redactor = ByteRedactor::new(needles);
    let first = redactor.push(&prefix, false);
    if !first.is_empty() {
        writer.write_all(&first).await?;
    }
    let mut buffer = [0_u8; 8192];
    loop {
        let read = match reader.read(&mut buffer).await {
            Ok(read) => read,
            Err(error)
                if error.kind() == std::io::ErrorKind::UnexpectedEof && framing.is_complete() =>
            {
                0
            }
            Err(error) => return Err(error),
        };
        if read == 0 {
            let tail = redactor.push(&[], true);
            if !tail.is_empty() {
                writer.write_all(&tail).await?;
            }
            return writer.shutdown().await;
        }
        framing.push(&buffer[..read]);
        let redacted = redactor.push(&buffer[..read], false);
        if !redacted.is_empty() {
            writer.write_all(&redacted).await?;
        }
    }
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
async fn pump_and_observe<C, U>(
    client_tls: C,
    upstream_tls: U,
    budget: usize,
    response_redactions: &[Vec<u8>],
    head_request: bool,
) -> Result<Vec<u8>, InterceptError>
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

    let response_result = async {
        let mut resp_buf = Vec::new();
        let mut redactor = ByteRedactor::new(response_redactions);
        let mut framing = Http1ResponseFraming::new(head_request);
        if !response_redactions.is_empty() {
            let prefix = read_response_prefix(&mut up_rd).await?;
            framing.push(&prefix);
            let headers_end = prefix
                .windows(4)
                .position(|value| value == b"\r\n\r\n")
                .ok_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "incomplete response headers",
                    )
                })?;
            if !response_headers_are_inspectable(&prefix[..headers_end]) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "credential-bearing response uses an unsupported content encoding",
                )
                .into());
            }
            let redacted = redactor.push(&prefix, false);
            client_wr.write_all(&redacted).await?;
            resp_buf.extend_from_slice(&redacted[..redacted.len().min(budget)]);
        }
        let mut tmp = [0u8; 8192];
        loop {
            let n = match up_rd.read(&mut tmp).await {
                Ok(0) => break,
                Ok(n) => n,
                Err(error)
                    if error.kind() == std::io::ErrorKind::UnexpectedEof
                        && framing.is_complete() =>
                {
                    break;
                }
                Err(error) => return Err(error.into()),
            };
            framing.push(&tmp[..n]);
            let output = if response_redactions.is_empty() {
                tmp[..n].to_vec()
            } else {
                redactor.push(&tmp[..n], false)
            };
            if client_wr.write_all(&output).await.is_err() {
                break;
            }
            if resp_buf.len() < budget {
                let take = (budget - resp_buf.len()).min(output.len());
                resp_buf.extend_from_slice(&output[..take]);
            }
        }
        if !response_redactions.is_empty() {
            let tail = redactor.push(&[], true);
            client_wr.write_all(&tail).await?;
            if resp_buf.len() < budget {
                let take = (budget - resp_buf.len()).min(tail.len());
                resp_buf.extend_from_slice(&tail[..take]);
            }
        }
        let _ = client_wr.flush().await;
        let _ = client_wr.shutdown().await;
        Ok::<_, InterceptError>(resp_buf)
    }
    .await;
    c2u.abort();
    let _ = c2u.await;
    response_result
}

#[cfg(test)]
mod hardening_tests {
    use super::*;
    use crate::policy::HostList;
    use crate::registry::{RefreshableCred, RequestPolicy};
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

    struct UncleanEofReader {
        bytes: Vec<u8>,
        position: usize,
    }

    impl AsyncRead for UncleanEofReader {
        fn poll_read(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            output: &mut ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            if self.position == self.bytes.len() {
                return Poll::Ready(Err(std::io::ErrorKind::UnexpectedEof.into()));
            }
            let count = output.remaining().min(self.bytes.len() - self.position);
            let end = self.position + count;
            output.put_slice(&self.bytes[self.position..end]);
            self.position = end;
            Poll::Ready(Ok(()))
        }
    }

    #[derive(Default)]
    struct VecWriter {
        bytes: Vec<u8>,
        shutdown: bool,
    }

    impl AsyncWrite for VecWriter {
        fn poll_write(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            input: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            self.bytes.extend_from_slice(input);
            Poll::Ready(Ok(input.len()))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<std::io::Result<()>> {
            self.shutdown = true;
            Poll::Ready(Ok(()))
        }
    }

    fn chunked_response(body: &[u8], terminal_chunk: bool) -> Vec<u8> {
        let mut response = format!(
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n",
            body.len(),
        )
        .into_bytes();
        response.extend_from_slice(body);
        response.extend_from_slice(b"\r\n");
        if terminal_chunk {
            response.extend_from_slice(b"0\r\n\r\n");
        }
        response
    }

    #[tokio::test]
    async fn complete_chunked_response_survives_unclean_tls_eof() {
        let response = chunked_response(&vec![b'x'; 16_088], true);
        let mut reader = UncleanEofReader {
            bytes: response.clone(),
            position: 0,
        };
        let mut writer = VecWriter::default();

        copy_redacting_response(&mut reader, &mut writer, &[vec![b's'; 1024]], false)
            .await
            .unwrap();

        assert_eq!(writer.bytes, response);
        assert!(writer.shutdown);
    }

    #[tokio::test]
    async fn incomplete_chunked_response_fails_closed_on_unclean_tls_eof() {
        let response = chunked_response(&vec![b'x'; 16_088], false);
        let mut reader = UncleanEofReader {
            bytes: response,
            position: 0,
        };
        let mut writer = VecWriter::default();

        let error = copy_redacting_response(&mut reader, &mut writer, &[vec![b's'; 1024]], false)
            .await
            .unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::UnexpectedEof);
        assert!(!writer.shutdown);
    }

    #[test]
    fn binds_authority_to_sni_and_removes_duplicates() {
        let request = b"GET /v1 HTTP/1.1\r\nHost: allowed.example\r\nhOsT: attacker.example\r\nAccept: */*\r\n\r\n".to_vec();
        let bound = bind_http1_authority(request, "api.example");
        let text = String::from_utf8(bound).unwrap();
        assert_eq!(text.to_ascii_lowercase().matches("host:").count(), 1);
        assert!(text.contains("Host: api.example\r\n"));
        assert!(!text.contains("attacker.example"));
    }

    #[test]
    fn rejects_absolute_form_request_targets() {
        let target = inject::request_line(
            b"GET https://attacker.example/v1 HTTP/1.1\r\nHost: api.example\r\n\r\n",
        )
        .unwrap()
        .1;
        assert!(!request_target_is_origin_form(&target));
        assert!(request_target_is_origin_form("/v1?value=ok"));
        assert!(request_target_is_origin_form("*"));
    }

    #[test]
    fn redacts_credentials_across_arbitrary_chunks() {
        let mut redactor = ByteRedactor::new(&[b"host-token".to_vec()]);
        let mut output = redactor.push(b"before-host-", false);
        output.extend(redactor.push(b"token-after", false));
        output.extend(redactor.push(&[], true));
        assert_eq!(output, b"before-**********-after");
    }

    #[test]
    fn rejects_encoded_credential_responses() {
        assert!(response_headers_are_inspectable(
            b"HTTP/1.1 200 OK\r\nContent-Encoding: identity"
        ));
        assert!(!response_headers_are_inspectable(
            b"HTTP/1.1 200 OK\r\nContent-Encoding: gzip"
        ));
    }

    #[test]
    fn substitutes_placeholders_across_h2_data_frames() {
        let secret = SecretEntry {
            placeholder: "guest-placeholder".into(),
            real_value: "host-secret".into(),
            allow: HostList::from_manifest(&["api.example".into()], &[]).unwrap(),
        };
        let secrets = [&secret];
        let mut transformer = PlaceholderTransformer::new("api.example", &secrets);
        let mut output = transformer.push(b"before-guest-", false).unwrap();
        output.extend(transformer.push(b"placeholder-after", false).unwrap());
        output.extend(transformer.push(&[], true).unwrap());
        assert_eq!(output, b"before-host-secret-after");
    }

    #[test]
    fn denies_google_credential_production_even_through_encoded_routes() {
        for host in [
            "sts.googleapis.com",
            "oauth2.googleapis.com",
            "accounts.google.com",
            "securetoken.googleapis.com",
            "iamcredentials.googleapis.com",
        ] {
            assert!(rejects_google_credential_request(host, "GET", "/"));
        }
        for operation in [
            ":generateAccessToken",
            ":generateIdToken",
            ":signBlob",
            ":signJwt",
        ] {
            assert!(rejects_google_credential_request(
                "iam.googleapis.com",
                "POST",
                &format!("/v1/projects/-/serviceAccounts/account@example.com{operation}")
            ));
        }
        assert!(rejects_google_credential_request(
            "compute.googleapis.com",
            "POST",
            "/v1/projects/p/serviceAccounts/a%253AgenerateAccessToken"
        ));
        assert!(rejects_google_credential_request(
            "iam.googleapis.com",
            "POST",
            "/v1/projects/p/serviceAccounts/account@example.com/keys"
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

    #[tokio::test]
    async fn h2_denial_and_slow_stream_do_not_block_an_allowed_stream() {
        let (guest_io, proxy_guest_io) = tokio::io::duplex(64 * 1024);
        let (proxy_upstream_io, upstream_io) = tokio::io::duplex(64 * 1024);
        let release_slow = Arc::new(tokio::sync::Notify::new());
        let upstream_release = release_slow.clone();

        let proxy = tokio::spawn(async move {
            let secret = SecretEntry {
                placeholder: "guest-placeholder".into(),
                real_value: "host-secret".into(),
                allow: HostList::from_manifest(&["api.example".into()], &[]).unwrap(),
            };
            let inject = InjectEntry {
                header_name: "authorization".into(),
                header_template: "Bearer {}".into(),
                allow: HostList::from_manifest(&["api.example".into()], &[]).unwrap(),
                policy: RequestPolicy {
                    methods: vec!["POST".into()],
                    path_globs: vec!["/slow".into(), "/fast".into()],
                    graphql: None,
                },
                mint_source: None,
                cred: RefreshableCred::new("host-token".into(), None),
            };
            run_h2(
                proxy_guest_io,
                proxy_upstream_io,
                H2Context {
                    sni: "api.example",
                    port: 443,
                    secrets: &[&secret],
                    injects: &[&inject],
                    observes: &[],
                    session_id: SessionId::new(),
                    sink: None,
                    refresher: None,
                },
            )
            .await
        });

        let upstream = tokio::spawn(async move {
            let mut server = h2::server::handshake(upstream_io).await.unwrap();
            for _ in 0..2 {
                let (request, mut respond) = server.accept().await.unwrap().unwrap();
                let release = upstream_release.clone();
                tokio::spawn(async move {
                    assert_eq!(request.uri().authority().unwrap(), "api.example");
                    assert_eq!(
                        request.headers().get("authorization").unwrap(),
                        "Bearer host-token"
                    );
                    let path = request.uri().path().to_string();
                    let mut body = request.into_body();
                    let mut request_data = Vec::new();
                    while let Some(data) = body.data().await {
                        let data = data.unwrap();
                        body.flow_control().release_capacity(data.len()).unwrap();
                        request_data.extend_from_slice(&data);
                    }
                    if path == "/slow" {
                        release.notified().await;
                    } else {
                        assert_eq!(path, "/fast");
                        assert_eq!(request_data, b"host-secret");
                    }
                    let response = http::Response::builder()
                        .status(200)
                        .header("x-reflected-token", "host-token")
                        .body(())
                        .unwrap();
                    let mut response_body = respond.send_response(response, false).unwrap();
                    response_body
                        .send_data(bytes::Bytes::from_static(b"host-secret|host-token"), true)
                        .unwrap();
                });
            }
            while server.accept().await.is_some() {}
        });

        let (mut guest, guest_connection) = h2::client::handshake(guest_io).await.unwrap();
        let guest_connection = tokio::spawn(guest_connection);

        guest = guest.ready().await.unwrap();
        let slow = http::Request::builder()
            .method("POST")
            .uri("https://attacker.example/slow")
            .header("authorization", "Bearer guest-token")
            .body(())
            .unwrap();
        let (slow_response, _) = guest.send_request(slow, true).unwrap();

        guest = guest.ready().await.unwrap();
        let denied = http::Request::builder()
            .method("POST")
            .uri("https://attacker.example/denied")
            .body(())
            .unwrap();
        let (denied_response, _) = guest.send_request(denied, true).unwrap();
        assert_eq!(denied_response.await.unwrap().status(), 403);

        guest = guest.ready().await.unwrap();
        let fast = http::Request::builder()
            .method("POST")
            .uri("https://attacker.example/fast")
            .header("authorization", "Bearer guest-token")
            .body(())
            .unwrap();
        let (fast_response, mut fast_body) = guest.send_request(fast, false).unwrap();
        fast_body
            .send_data(bytes::Bytes::from_static(b"guest-"), false)
            .unwrap();
        fast_body
            .send_data(bytes::Bytes::from_static(b"placeholder"), true)
            .unwrap();
        let fast_response = tokio::time::timeout(std::time::Duration::from_secs(1), fast_response)
            .await
            .expect("fast stream must not wait for the slow stream")
            .unwrap();
        assert_eq!(fast_response.status(), 200);
        assert_eq!(
            fast_response.headers().get("x-reflected-token").unwrap(),
            "**********"
        );
        let mut response_body = fast_response.into_body();
        let mut response_data = Vec::new();
        while let Some(data) = response_body.data().await {
            let data = data.unwrap();
            response_body
                .flow_control()
                .release_capacity(data.len())
                .unwrap();
            response_data.extend_from_slice(&data);
        }
        assert_eq!(response_data, b"***********|**********");

        release_slow.notify_waiters();
        assert_eq!(slow_response.await.unwrap().status(), 200);
        drop(guest);
        guest_connection.abort();
        upstream.abort();
        proxy.abort();
    }
}
