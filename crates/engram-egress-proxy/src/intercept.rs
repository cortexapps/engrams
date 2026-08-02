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
use crate::google_denylist;
use crate::graphql::{self, ParsedGraphql};
use crate::inject;
use crate::observe::{self, ObserveSink};
use crate::registry::{InjectEntry, InjectRefresher, ObserveEntry, SecretEntry};
use crate::replayed::Replayed;
use crate::resolver::{ResolveError, UpstreamResolver};
use crate::substitute::substitute;

/// Cap on the HTTP/1 request header block. The block MUST terminate inside this
/// bound: every gate in this module (authority pinning, injected-header
/// overwrite, the credential scan) walks only the buffered head, so a header
/// that lands past the bound would reach the upstream unexamined. A guest that
/// pads its headers past the bound is rejected, not relayed. 64 KiB is far
/// above every real client — nginx, Google and GitHub all reject larger heads.
const REQUEST_HEAD_BUDGET: usize = 64 * 1024;

/// Cap on a request body we buffer in full so placeholder substitution can
/// rewrite `Content-Length`. A body past this cap streams **unsubstituted**:
/// the declared length then stays correct because the bytes are untouched.
/// (A placeholder is not itself a secret, so relaying one to a permitted host
/// is a failed API call, never a leak.)
const REQUEST_BODY_BUDGET: usize = 1024 * 1024;

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
    /// The HTTP/1 header block did not terminate inside [`REQUEST_HEAD_BUDGET`].
    /// The gate sees only the buffered head, so a longer head would let the
    /// remainder — a second `Host`, a guest `Authorization` — stream to the
    /// upstream unexamined. Fail closed instead.
    RequestHeadTooLarge,
    /// The HTTP/1 header block used a line shape the gate cannot walk safely:
    /// a bare LF terminator, an obsolete line fold, whitespace before the
    /// colon, or a line with no colon. Every walker here splits on CRLF, so
    /// these shapes are invisible to the gate but are still accepted as headers
    /// by many upstreams — a request-smuggling primitive. `reason` is a static
    /// tag for logs (never request bytes).
    MalformedHeaderBlock {
        reason: &'static str,
    },
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
            Self::RequestHeadTooLarge => {
                write!(f, "HTTP/1.1 header block exceeds the inspection limit")
            }
            Self::MalformedHeaderBlock { reason } => {
                write!(f, "malformed HTTP/1.1 header block: {reason}")
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

/// Everything the policy gate reads for one intercepted connection.
///
/// Both HTTP adapters take this same struct and run the same
/// [`evaluate_stream`] over it, so a change to the gate cannot reach one
/// protocol and miss the other. They used to carry two hand-copied versions of
/// the gate, which is how HTTP/2 kept its own drift.
pub struct StreamContext<'a> {
    pub sni: &'a str,
    pub port: u16,
    /// Secrets whose `allow` list covers this host, so their placeholders are
    /// substituted here.
    pub secrets: &'a [&'a SecretEntry],
    pub injects: &'a [&'a InjectEntry],
    pub observes: &'a [&'a ObserveEntry],
    /// Placeholders belonging to secrets this host is NOT allowed to receive.
    /// Seeing one in a request is a leak attempt and closes the connection.
    ///
    /// This has to be supplied separately because `decide()` already narrowed
    /// `secrets` to the host-matching ones. The old leak scan re-derived the
    /// disallowed set from that narrowed list, which is always empty — so the
    /// detector could never fire in production.
    pub foreign_placeholders: &'a [&'a str],
    pub session_id: SessionId,
    pub sink: Option<&'a ObserveSink>,
    pub refresher: Option<&'a dyn InjectRefresher>,
}

/// Drive a MITM intercept on `client_stream`. Bytes already peeked
/// during SNI extraction are stitched back at the front via
/// [`crate::replayed::Replayed`]. The upstream is dialed by SNI
/// through `resolver`, not by guest-supplied IP — see the
/// `resolver` module for why.
pub async fn run<C>(
    client_stream: C,
    peeked: Vec<u8>,
    resolver: Arc<dyn UpstreamResolver>,
    context: StreamContext<'_>,
    server_cfg: Arc<ServerConfig>,
    client_cfg: Arc<ClientConfig>,
) -> Result<(), InterceptError>
where
    C: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let StreamContext {
        sni,
        port,
        secrets,
        injects,
        observes,
        foreign_placeholders,
        session_id,
        sink,
        refresher,
    } = context;
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
            StreamContext {
                sni,
                port,
                secrets,
                injects,
                observes,
                foreign_placeholders,
                session_id,
                sink,
                refresher,
            },
        )
        .await;
    }
    let context = StreamContext {
        sni,
        port,
        secrets,
        injects,
        observes,
        foreign_placeholders,
        session_id,
        sink,
        refresher,
    };

    // Buffer the WHOLE request head, then prove it is a shape this module's
    // CRLF walkers read the same way the upstream does. Both steps fail closed:
    // a head that outruns the budget, or that carries a bare LF / an obsolete
    // fold, never reaches the upstream. Without them a guest could pad past the
    // buffered prefix (or terminate a line with a bare LF) and smuggle its own
    // `Host` or `Authorization` past the gate.
    let mut prefix = Vec::with_capacity(8192);
    let mut head_end = read_request_head(&mut client_tls, &mut prefix).await?;
    validate_header_block(&prefix[..head_end])?;

    // Policy and upstream TLS identity are selected by SNI. Bind the cleartext
    // HTTP authority to that same name before any credential is attached.
    let bound = bind_http1_authority(std::mem::take(&mut prefix), &upstream_authority(sni, port));
    head_end = head_end_of(&bound).ok_or(InterceptError::MalformedRequest)?;
    prefix = bound;

    let (method, path) = inject::request_line(&prefix).ok_or(InterceptError::MalformedRequest)?;
    if !request_target_is_origin_form(&path) {
        return Err(InterceptError::InvalidRequestTarget);
    }
    deny_credential_operation(sni, &method, &path)?;
    let head_request = method.eq_ignore_ascii_case("HEAD");

    // ADR 0059: for a GraphQL endpoint, read the FULL request body (bounded) and
    // parse it into its top-level operation/fields. Fail-closed: anything we can't
    // read or parse cleanly rejects the request before a byte reaches upstream.
    // `gh` (and standard GraphQL clients) send a Content-Length'd, identity-encoded
    // JSON body — chunked/compressed bodies are denied.
    let parsed_graphql: Option<ParsedGraphql> = if is_graphql_endpoint(injects, observes, &path) {
        let head = std::str::from_utf8(&prefix[..head_end]).map_err(|_| {
            InterceptError::GraphqlRejected {
                reason: "non-utf8 headers",
            }
        })?;
        let content_length = graphql_content_length(head)?;
        let body_start = head_end;
        let body_end = body_start + content_length;
        read_to_content_length(
            &mut client_tls,
            &mut prefix,
            body_end,
            InterceptError::GraphqlRejected {
                reason: "truncated graphql body",
            },
        )
        .await?;
        let body = &prefix[body_start..body_end];
        Some(
            graphql::parse_request_body(body).ok_or(InterceptError::GraphqlRejected {
                reason: "unparseable or unsupported graphql operation",
            })?,
        )
    } else {
        None
    };

    let plan = evaluate_stream(&context, &method, &path, parsed_graphql.as_ref()).await?;
    let StreamPlan {
        matched,
        firing,
        response_redactions,
    } = plan;
    if !matched.is_empty() {
        prefix = inject::inject_headers(prefix, &matched)?;
    }

    if let Some(placeholder) = crate::violation::first_match(&prefix, foreign_placeholders) {
        return Err(InterceptError::Violation {
            placeholder: placeholder.to_string(),
        });
    }
    prefix = substitute_request(&mut client_tls, prefix, sni, secrets).await?;

    if !firing.is_empty() {
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
    // The gated request is already written and checked above. What remains is a
    // body that continues past the buffered head, and it is best-effort: we
    // force `Connection: close`, so a correct upstream may answer and close its
    // read half while the guest is still writing. That is a normal end of the
    // exchange, not a proxy failure — reporting it as one made every such
    // request log an error and turned the e2e tests flaky (`broken pipe`). A
    // truncated request is still visible: the upstream answers with an error,
    // and that answer reaches the guest verbatim.
    //
    // Both halves stay concurrent. An upstream that withholds its response
    // until it has the whole body would otherwise deadlock against a guest that
    // waits for the response before finishing its own write.
    let request = async {
        let _ = tokio::io::copy(&mut client_read, &mut upstream_write).await;
        let _ = upstream_write.shutdown().await;
    };
    // The response half keeps failing closed: it carries the redaction and the
    // framing-completeness check.
    let response = copy_redacting_response(
        &mut upstream_read,
        &mut client_write,
        &response_redactions,
        head_request,
    );
    let (_, response_result) = tokio::join!(request, response);
    response_result?;
    Ok(())
}

/// Cap on the streams one HTTP/2 connection may run at once.
///
/// Each stream can buffer up to [`GRAPHQL_REQUEST_BODY_BUDGET`] of request body
/// and [`OBSERVE_RESPONSE_BUDGET`] of tapped response, so the per-connection
/// worst case is this number times those budgets. Without a cap that product is
/// unbounded: a guest could open thousands of streams and hold half a gigabyte
/// of host memory on one connection.
const H2_MAX_CONCURRENT_STREAMS: u32 = 64;

/// Flow-control windows. The connection window is the aggregate bound on data
/// in flight; the stream window bounds any single stream.
const H2_STREAM_WINDOW: u32 = 256 * 1024;
const H2_CONNECTION_WINDOW: u32 = 1024 * 1024;

/// Forward one HTTP/2 connection. A denied stream gets its own response. Other
/// streams continue, and slow streams do not block new policy decisions.
async fn run_h2<C, U>(
    client_tls: C,
    upstream_tls: U,
    context: StreamContext<'_>,
) -> Result<(), InterceptError>
where
    C: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    U: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let mut inbound = h2::server::Builder::new()
        .max_concurrent_streams(H2_MAX_CONCURRENT_STREAMS)
        .initial_window_size(H2_STREAM_WINDOW)
        .initial_connection_window_size(H2_CONNECTION_WINDOW)
        .handshake(client_tls)
        .await?;
    let (outbound, connection) = h2::client::Builder::new()
        .initial_window_size(H2_STREAM_WINDOW)
        .initial_connection_window_size(H2_CONNECTION_WINDOW)
        .handshake(upstream_tls)
        .await?;
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
    context: &StreamContext<'_>,
) -> Result<(), InterceptError> {
    let method = request.method().as_str().to_string();
    let path = request
        .uri()
        .path_and_query()
        .map_or_else(|| "/".to_string(), ToString::to_string);
    if let Err(error) = deny_credential_operation(context.sni, &method, &path) {
        deny_h2(&mut respond, http::StatusCode::FORBIDDEN)?;
        return Err(error);
    }

    let (mut parts, mut request_body) = request.into_parts();
    let (buffered_body, request_trailers, parsed_graphql) =
        if is_graphql_endpoint(context.injects, context.observes, &path) {
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

    // The SAME gate the HTTP/1 adapter runs. Both used to carry a hand-copied
    // version, so a policy change could reach one protocol and miss the other.
    let plan = match evaluate_stream(context, &method, &path, parsed_graphql.as_ref()).await {
        Ok(plan) => plan,
        Err(error) => {
            deny_h2(&mut respond, denial_status(&error))?;
            return Err(error);
        }
    };
    let StreamPlan {
        matched,
        firing,
        response_redactions,
    } = plan;

    transform_h2_headers(
        &mut parts.headers,
        context.sni,
        context.secrets,
        context.foreign_placeholders,
    )?;
    // The `:method` pseudo-header is what the gate above read, so no header may
    // ask the upstream to run a different verb.
    for header in google_denylist::METHOD_OVERRIDE_HEADERS {
        parts.headers.remove(header);
    }
    inject_h2_headers(&mut parts.headers, &matched)?;
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
            let body = transform_complete(
                body,
                context.sni,
                context.secrets,
                context.foreign_placeholders,
            )?;
            let end = request_trailers.is_none();
            send_h2_data(&mut upstream_body, body.into(), end).await?;
            if let Some(mut trailers) = request_trailers {
                transform_h2_headers(
                    &mut trailers,
                    context.sni,
                    context.secrets,
                    context.foreign_placeholders,
                )?;
                upstream_body.send_trailers(trailers)?;
            }
        } else if !request_end {
            let mut transformer =
                PlaceholderTransformer::new(context.secrets, context.foreign_placeholders);
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
                transform_h2_headers(
                    &mut trailers,
                    context.sni,
                    context.secrets,
                    context.foreign_placeholders,
                )?;
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
        // Tap the body ONLY when an observe spec will read it. The HTTP/1 path
        // has always guarded on that; HTTP/2 copied up to 256 KiB of every
        // response for a consumer that usually does not exist.
        let observing = !firing.is_empty();
        let mut observed_body = Vec::new();
        let tap = |output: &[u8], observed: &mut Vec<u8>| {
            if observing && observed.len() < OBSERVE_RESPONSE_BUDGET {
                let take = (OBSERVE_RESPONSE_BUDGET - observed.len()).min(output.len());
                observed.extend_from_slice(&output[..take]);
            }
        };
        if !response_end {
            while let Some(data) = response_body.data().await {
                let data = data?;
                let len = data.len();
                response_body.flow_control().release_capacity(len)?;
                let output = redactor.push(&data, false);
                tap(&output, &mut observed_body);
                if !output.is_empty() {
                    send_h2_data(&mut client_body, output.into(), false).await?;
                }
            }
            let tail = redactor.push(&[], true);
            tap(&tail, &mut observed_body);
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

/// What the policy gate decided for one request. Produced once, by
/// [`evaluate_stream`], and read the same way by both HTTP adapters.
struct StreamPlan<'a> {
    /// Injections whose credential this request may carry.
    matched: Vec<&'a InjectEntry>,
    /// Observe specs whose request shape matched AND for which a sink exists.
    firing: Vec<&'a ObserveEntry>,
    /// Every host-side credential this request can send, which the response
    /// must therefore not return.
    response_redactions: Vec<Vec<u8>>,
}

/// Google credential minting is never a guest surface, whatever endpoint an
/// administrator selects. Checked before anything is buffered.
fn deny_credential_operation(sni: &str, method: &str, path: &str) -> Result<(), InterceptError> {
    if google_denylist::denies_operation(sni, method, path) {
        return Err(InterceptError::CredentialRequestRejected {
            method: method.to_string(),
            path: path_without_query(path).to_string(),
        });
    }
    Ok(())
}

/// ADR 0059: is this request going to a declared GraphQL endpoint? Only then is
/// the body buffered and parsed; a REST host keeps the header-only early stop.
fn is_graphql_endpoint(injects: &[&InjectEntry], observes: &[&ObserveEntry], path: &str) -> bool {
    injects
        .iter()
        .any(|entry| entry.policy.graphql.is_some() && entry.policy.path_matches(path))
        || observes
            .iter()
            .any(|entry| entry.policy.graphql.is_some() && entry.policy.path_matches(path))
}

/// Run the integration policy for one request and return the plan both adapters
/// act on.
///
/// ADR 0056/0059 (Plane B): an inject-gated host must satisfy a request policy.
/// REST gates by (method, path) glob; GraphQL requires every top-level field to
/// be covered by some granted GraphQL inject (set coverage). A request matching
/// no injection is rejected — the operation is not permitted on this host.
async fn evaluate_stream<'a>(
    context: &StreamContext<'a>,
    method: &str,
    path: &str,
    parsed_graphql: Option<&ParsedGraphql>,
) -> Result<StreamPlan<'a>, InterceptError> {
    let matched: Vec<&'a InjectEntry> = if context.injects.is_empty() {
        Vec::new()
    } else if let Some(document) = parsed_graphql {
        gate_graphql_injects(context.injects, method, path, document).ok_or(
            InterceptError::GraphqlRejected {
                reason: "operation not permitted by integration policy",
            },
        )?
    } else {
        let matched: Vec<&'a InjectEntry> = context
            .injects
            .iter()
            .copied()
            .filter(|entry| entry.policy.graphql.is_none() && entry.policy.allows(method, path))
            .collect();
        if matched.is_empty() {
            return Err(InterceptError::RequestRejected {
                method: method.to_string(),
                path: path.to_string(),
            });
        }
        matched
    };

    // WS4: re-mint any near-expiry minted credential BEFORE injecting it, so a
    // long-lived session never sends a stale (expired ~1h post-boot)
    // installation token — the campaign's reads-401/writes-succeed asymmetry.
    // Single-flighted per entry; on refresh failure the stale secret is kept
    // (see `InjectEntry::refresh_if_stale`). Static entries are a no-op.
    if let Some(refresher) = context.refresher {
        for entry in &matched {
            entry.refresh_if_stale(context.session_id, refresher).await;
        }
    }

    // ADR 0056 (Phase 4) / 0059: observe the response for any observe spec whose
    // request shape matches — REST by (method, path); GraphQL by a top-level
    // (operation, field) in the parsed body. Only when a sink is wired (the
    // host-agent's bridge to the coordinator) — without a consumer there is no
    // point buffering the response.
    let firing: Vec<&'a ObserveEntry> = if context.sink.is_none() {
        Vec::new()
    } else {
        context
            .observes
            .iter()
            .copied()
            .filter(|entry| match (&entry.policy.graphql, parsed_graphql) {
                (Some(matcher), Some(document)) => {
                    entry.policy.method_matches(method)
                        && entry.policy.path_matches(path)
                        && document
                            .top_level
                            .iter()
                            .any(|(operation, field)| matcher.matches(*operation, field))
                }
                (None, _) => entry.policy.allows(method, path),
                // A GraphQL observe spec never fires on a non-GraphQL request.
                (Some(_), None) => false,
            })
            .collect()
    };

    Ok(StreamPlan {
        response_redactions: response_redactions(context.sni, context.secrets, &matched),
        matched,
        firing,
    })
}

/// The status an HTTP/2 stream answers a policy denial with. HTTP/1 never
/// synthesizes a response — it closes the connection, so the guest's client
/// reports a transport error and can never read a denial as a 2xx. HTTP/2 has
/// no such option: a stream is one frame sequence inside a multiplexed
/// connection, so closing it needs an explicit status. A 4xx is safe here for
/// the same reason the HTTP/1 close is: it is a REFUSAL, never a synthesized
/// success, and the guest's client surfaces it as an error.
fn denial_status(error: &InterceptError) -> http::StatusCode {
    match error {
        InterceptError::GraphqlRejected { .. } => http::StatusCode::BAD_REQUEST,
        _ => http::StatusCode::FORBIDDEN,
    }
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
    foreign_placeholders: &[&str],
) -> Result<Vec<u8>, InterceptError> {
    if let Some(placeholder) = crate::violation::first_match(&value, foreign_placeholders) {
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
    foreign_placeholders: &[&str],
) -> Result<(), InterceptError> {
    for (name, value) in headers.iter_mut() {
        let transformed = transform_complete(
            value.as_bytes().to_vec(),
            sni,
            secrets,
            foreign_placeholders,
        )?;
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
    secrets: &'a [&'a SecretEntry],
    /// Placeholders this host may not receive. Seeing one closes the stream.
    foreign_placeholders: &'a [&'a str],
    pending: Vec<u8>,
    keep: usize,
}

impl<'a> PlaceholderTransformer<'a> {
    fn new(secrets: &'a [&'a SecretEntry], foreign_placeholders: &'a [&'a str]) -> Self {
        // The hold-back window has to cover the LONGEST needle of either kind,
        // or a foreign placeholder split across two DATA frames slips past the
        // scan.
        let keep = secrets
            .iter()
            .map(|entry| entry.placeholder.len())
            .chain(foreign_placeholders.iter().map(|value| value.len()))
            .filter(|length| *length > 0)
            .max()
            .unwrap_or(1)
            .saturating_sub(1);
        Self {
            secrets,
            foreign_placeholders,
            pending: Vec::new(),
            keep,
        }
    }

    fn push(&mut self, input: &[u8], final_chunk: bool) -> Result<Vec<u8>, InterceptError> {
        self.pending.extend_from_slice(input);
        let process_limit = if final_chunk {
            self.pending.len()
        } else {
            self.pending.len().saturating_sub(self.keep)
        };
        if let Some(placeholder) =
            crate::violation::first_match(&self.pending[..process_limit], self.foreign_placeholders)
        {
            return Err(InterceptError::Violation {
                placeholder: placeholder.to_string(),
            });
        }
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
            output.extend_from_slice(entry.real_value.as_bytes());
            cursor = start + entry.placeholder.len();
        }
        self.pending.drain(..cursor);
        Ok(output)
    }
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

/// Offset just past the CRLFCRLF that ends the HTTP/1 header block.
fn head_end_of(request: &[u8]) -> Option<usize> {
    request
        .windows(4)
        .position(|value| value == b"\r\n\r\n")
        .map(|start| start + 4)
}

/// Read until the whole HTTP/1 header block is buffered. Returns the offset
/// just past the terminator; bytes already read past it stay in `prefix` as the
/// start of the body.
///
/// Fails closed when the block does not terminate inside
/// [`REQUEST_HEAD_BUDGET`], or when the guest closes the connection first. The
/// old code stopped at a fixed 64 KiB and relayed the remainder verbatim, so a
/// guest that padded its headers past that mark could put its own `Host` or
/// `Authorization` beyond the gate's view.
async fn read_request_head<C>(
    client_tls: &mut C,
    prefix: &mut Vec<u8>,
) -> Result<usize, InterceptError>
where
    C: AsyncRead + Unpin,
{
    // A terminator can straddle two reads, so each pass rescans the last three
    // bytes of the previous one. Without the cursor a byte-at-a-time guest
    // would make this quadratic.
    let mut searched = 0_usize;
    loop {
        if let Some(start) = prefix[searched..]
            .windows(4)
            .position(|value| value == b"\r\n\r\n")
        {
            return Ok(searched + start + 4);
        }
        searched = prefix.len().saturating_sub(3);
        if prefix.len() >= REQUEST_HEAD_BUDGET {
            return Err(InterceptError::RequestHeadTooLarge);
        }
        let mut chunk = [0_u8; 8192];
        let read = client_tls.read(&mut chunk).await?;
        if read == 0 {
            return Err(InterceptError::RequestHeadTooLarge);
        }
        prefix.extend_from_slice(&chunk[..read]);
    }
}

/// Prove the header block is a shape this module's CRLF walkers read the same
/// way the upstream will. Every rejection here is a smuggling primitive:
///
/// - A **bare LF** terminator is invisible to a CRLF walker but is accepted as
///   a line end by many servers, so `…\nHost: attacker\r\n` slips a second
///   authority past the gate.
/// - An **obsolete line fold** (a line that starts with SP or HTAB) continues
///   the previous header for the upstream while the walker reads it as its own
///   line — the two disagree about how many headers exist.
/// - A **line with no colon**, or **whitespace before the colon**, is rejected
///   by RFC 9112 §5.1 for the same reason.
/// - `Content-Length` **and** `Transfer-Encoding` together, or two disagreeing
///   `Content-Length` values, let the two ends disagree about where the request
///   body ends.
fn validate_header_block(head: &[u8]) -> Result<(), InterceptError> {
    let mut previous = 0_u8;
    for byte in head {
        if *byte == b'\n' && previous != b'\r' {
            return Err(InterceptError::MalformedHeaderBlock {
                reason: "bare line feed",
            });
        }
        previous = *byte;
    }

    let mut content_length: Option<u64> = None;
    let mut chunked = false;
    // Skip the request line; it is checked by `request_line` + origin-form.
    let mut cursor = match head.windows(2).position(|value| value == b"\r\n") {
        Some(end) => end + 2,
        None => {
            return Err(InterceptError::MalformedHeaderBlock {
                reason: "no request line",
            })
        }
    };
    while cursor < head.len() {
        let Some(relative) = head[cursor..].windows(2).position(|value| value == b"\r\n") else {
            break;
        };
        let line = &head[cursor..cursor + relative];
        cursor += relative + 2;
        if line.is_empty() {
            break; // the blank line that ends the block
        }
        if line[0] == b' ' || line[0] == b'\t' {
            return Err(InterceptError::MalformedHeaderBlock {
                reason: "obsolete line fold",
            });
        }
        let Some(colon) = line.iter().position(|byte| *byte == b':') else {
            return Err(InterceptError::MalformedHeaderBlock {
                reason: "header line without a colon",
            });
        };
        let name = &line[..colon];
        if name.last().is_some_and(|byte| byte.is_ascii_whitespace()) {
            return Err(InterceptError::MalformedHeaderBlock {
                reason: "whitespace before the header colon",
            });
        }
        let value = line[colon + 1..].trim_ascii();
        if name.eq_ignore_ascii_case(b"transfer-encoding") {
            chunked = true;
        } else if name.eq_ignore_ascii_case(b"content-length") {
            let parsed = std::str::from_utf8(value)
                .ok()
                .and_then(|value| value.parse::<u64>().ok())
                .ok_or(InterceptError::MalformedHeaderBlock {
                    reason: "unparseable content-length",
                })?;
            if content_length
                .replace(parsed)
                .is_some_and(|first| first != parsed)
            {
                return Err(InterceptError::MalformedHeaderBlock {
                    reason: "conflicting content-length",
                });
            }
        }
    }
    if chunked && content_length.is_some() {
        return Err(InterceptError::MalformedHeaderBlock {
            reason: "content-length with transfer-encoding",
        });
    }
    Ok(())
}

/// How the request declares the end of its body. Read from an already-validated
/// header block, so the shapes that disagree with the upstream are gone.
enum RequestBodyFraming {
    None,
    ContentLength(usize),
    Chunked,
}

fn request_body_framing(head: &[u8]) -> RequestBodyFraming {
    let mut cursor = match head.windows(2).position(|value| value == b"\r\n") {
        Some(end) => end + 2,
        None => return RequestBodyFraming::None,
    };
    while cursor < head.len() {
        let Some(relative) = head[cursor..].windows(2).position(|value| value == b"\r\n") else {
            break;
        };
        let line = &head[cursor..cursor + relative];
        cursor += relative + 2;
        if line.is_empty() {
            break;
        }
        let Some(colon) = line.iter().position(|byte| *byte == b':') else {
            continue;
        };
        let name = &line[..colon];
        let value = line[colon + 1..].trim_ascii();
        if name.eq_ignore_ascii_case(b"transfer-encoding") {
            return RequestBodyFraming::Chunked;
        }
        if name.eq_ignore_ascii_case(b"content-length") {
            if let Some(length) = std::str::from_utf8(value)
                .ok()
                .and_then(|value| value.parse::<usize>().ok())
            {
                return RequestBodyFraming::ContentLength(length);
            }
        }
    }
    RequestBodyFraming::None
}

/// Substitute placeholders across the head AND the body, and keep the declared
/// body length honest.
///
/// A real secret rarely has the same length as its placeholder, so a
/// substitution inside the body changes the body length. HTTP/2 has a
/// structural length and simply drops `Content-Length`; HTTP/1 does not, so a
/// stale length truncates the request or hangs the connection (the broker-mode
/// symptom this closes). The body is therefore buffered in full and the
/// declared length is rewritten from what we actually send.
///
/// A body we cannot bound (`Transfer-Encoding: chunked`) or cannot afford
/// (past [`REQUEST_BODY_BUDGET`]) streams **untouched**, which keeps the
/// original length correct. A placeholder is not itself a secret, so relaying
/// one to a permitted host costs an API call, not a credential.
async fn substitute_request<C>(
    client_tls: &mut C,
    prefix: Vec<u8>,
    sni: &str,
    secrets: &[&SecretEntry],
) -> Result<Vec<u8>, InterceptError>
where
    C: AsyncRead + Unpin,
{
    if secrets.is_empty() {
        return Ok(prefix);
    }
    let head_end = head_end_of(&prefix).ok_or(InterceptError::MalformedRequest)?;
    let mut prefix = prefix;
    let buffered_body = match request_body_framing(&prefix[..head_end]) {
        RequestBodyFraming::ContentLength(length) if length <= REQUEST_BODY_BUDGET => {
            read_to_content_length(
                client_tls,
                &mut prefix,
                head_end + length,
                InterceptError::MalformedRequest,
            )
            .await?;
            true
        }
        _ => false,
    };
    let body = prefix.split_off(head_end);
    let head = substitute(prefix, sni, secrets);
    if !buffered_body {
        let mut out = head;
        out.extend_from_slice(&body);
        return Ok(out);
    }
    let body = substitute(body, sni, secrets);
    let mut out = rewrite_content_length(&head, body.len())?;
    out.extend_from_slice(&body);
    Ok(out)
}

/// Rewrite the request's `Content-Length` to `length`. The header block is
/// already validated, so exactly one `Content-Length` is present.
fn rewrite_content_length(head: &[u8], length: usize) -> Result<Vec<u8>, InterceptError> {
    let request_line_end = head
        .windows(2)
        .position(|value| value == b"\r\n")
        .ok_or(InterceptError::MalformedRequest)?
        + 2;
    let mut out = Vec::with_capacity(head.len() + 8);
    out.extend_from_slice(&head[..request_line_end]);
    let mut cursor = request_line_end;
    let mut rewritten = false;
    while cursor < head.len() {
        let Some(relative) = head[cursor..].windows(2).position(|value| value == b"\r\n") else {
            out.extend_from_slice(&head[cursor..]);
            break;
        };
        let line_end = cursor + relative;
        let line = &head[cursor..line_end];
        let name = line
            .iter()
            .position(|byte| *byte == b':')
            .map_or(line, |colon| &line[..colon]);
        if name.eq_ignore_ascii_case(b"content-length") {
            out.extend_from_slice(b"Content-Length: ");
            out.extend_from_slice(length.to_string().as_bytes());
            out.extend_from_slice(b"\r\n");
            rewritten = true;
        } else {
            out.extend_from_slice(&head[cursor..line_end + 2]);
        }
        cursor = line_end + 2;
    }
    if !rewritten {
        return Err(InterceptError::MalformedRequest);
    }
    Ok(out)
}

/// Bind the cleartext request to the identity the policy was chosen for.
///
/// Two rewrites, one walk of the header block:
///
/// - Every guest-supplied `Host` is replaced by the SNI-authenticated
///   authority, so a duplicate or mixed-case `Host` cannot select a different
///   virtual host after the policy decision.
/// - Every method-override header is dropped. Google honours
///   `X-HTTP-Method-Override`, so a guest could send the `POST` our gate reads
///   and have the upstream run `DELETE` — the request line has to be the only
///   statement of intent.
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
        if !name.eq_ignore_ascii_case(b"host") && !google_denylist::is_method_override_header(name)
        {
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

    /// Consume `input`, advancing the framing state, and record the ranges of
    /// `input` that carry text a credential can appear in: the header block,
    /// the chunk data, the trailers, and a close-delimited body. Chunk-size
    /// lines and the CRLF after each chunk are **excluded** — those are
    /// framing, and redacting inside them would corrupt the response.
    ///
    /// The recorded ranges are what makes redaction chunk-aware. Joined in
    /// order they form the decoded response text, so a credential split across
    /// a chunk boundary is one contiguous match again.
    fn classify(&mut self, input: &[u8], scannable: &mut Vec<(usize, usize)>) {
        let mut pos = 0;
        while pos < input.len() {
            match &mut self.state {
                Http1ResponseState::Headers(buffer) => {
                    let room = RESPONSE_HEADER_BUDGET.saturating_sub(buffer.len());
                    if room == 0 {
                        self.state = Http1ResponseState::Invalid;
                        return;
                    }
                    let take = (input.len() - pos).min(room);
                    let before = buffer.len();
                    buffer.extend_from_slice(&input[pos..pos + take]);
                    // A terminator can straddle two reads, so rescan the last
                    // three bytes of the previous pass.
                    let search_from = before.saturating_sub(3);
                    let found = buffer[search_from..]
                        .windows(4)
                        .position(|value| value == b"\r\n\r\n")
                        .map(|offset| search_from + offset);
                    let Some(end) = found else {
                        push_range(scannable, pos, take);
                        pos += take;
                        if buffer.len() >= RESPONSE_HEADER_BUDGET {
                            self.state = Http1ResponseState::Invalid;
                            return;
                        }
                        continue;
                    };
                    let consumed = (end + 4).saturating_sub(before);
                    if consumed == 0 {
                        self.state = Http1ResponseState::Invalid;
                        return;
                    }
                    push_range(scannable, pos, consumed);
                    pos += consumed;
                    let head = buffer[..end].to_vec();
                    self.state = response_body_framing(&head, self.head_request)
                        .unwrap_or(Http1ResponseState::Invalid);
                }
                Http1ResponseState::ContentLength(remaining) => {
                    let take = (input.len() - pos).min(*remaining);
                    if take == 0 {
                        self.state = Http1ResponseState::Invalid;
                        return;
                    }
                    push_range(scannable, pos, take);
                    *remaining -= take;
                    pos += take;
                    if *remaining == 0 {
                        self.state = if pos == input.len() {
                            Http1ResponseState::Complete
                        } else {
                            Http1ResponseState::Invalid
                        };
                    }
                }
                Http1ResponseState::Chunked(chunked) => {
                    let consumed = chunked.classify(&input[pos..], pos, scannable);
                    let complete = matches!(chunked, ChunkedFraming::Complete);
                    let invalid = matches!(chunked, ChunkedFraming::Invalid) || consumed == 0;
                    pos += consumed;
                    if invalid {
                        self.state = Http1ResponseState::Invalid;
                        return;
                    }
                    if complete {
                        self.state = if pos == input.len() {
                            Http1ResponseState::Complete
                        } else {
                            Http1ResponseState::Invalid
                        };
                    }
                }
                Http1ResponseState::CloseDelimited => {
                    push_range(scannable, pos, input.len() - pos);
                    return;
                }
                Http1ResponseState::Complete | Http1ResponseState::Invalid => {
                    push_range(scannable, pos, input.len() - pos);
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

/// Append `(start, length)`, joining it to the previous range when the two
/// touch. Trailers arrive a byte at a time, so joining keeps the range list
/// short.
fn push_range(ranges: &mut Vec<(usize, usize)>, start: usize, length: usize) {
    if length == 0 {
        return;
    }
    if let Some((last_start, last_length)) = ranges.last_mut() {
        if *last_start + *last_length == start {
            *last_length += length;
            return;
        }
    }
    ranges.push((start, length));
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
    /// Consume `input`, recording the ranges that carry decoded body text.
    /// Offsets are reported relative to the caller's buffer through `base`.
    fn classify(
        &mut self,
        input: &[u8],
        base: usize,
        scannable: &mut Vec<(usize, usize)>,
    ) -> usize {
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
                    push_range(scannable, base + consumed, take);
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
                    // A trailer can reflect a credential, so it is scanned too.
                    push_range(scannable, base + consumed, 1);
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

/// Redacts host credentials from a streaming HTTP/1 response.
///
/// The scan must run over the DECODED response text. A `Transfer-Encoding:
/// chunked` upstream can split a credential across a chunk boundary, where the
/// wire bytes carry `…\r\n<size>\r\n…` between the two halves and a raw scan
/// finds nothing — the credential then reaches the guest. This type drives
/// [`Http1ResponseFraming`], joins the scannable regions into one continuous
/// stream, redacts that, and writes each match back at its wire position.
/// Redaction replaces every byte with `*`, so lengths never change and the
/// chunk sizes stay exact — no re-framing is needed.
///
/// An empty needle set makes this a pass-through that only tracks framing,
/// which is what the response path needs to accept an unclean TLS EOF.
struct Http1ResponseRedactor {
    framing: Http1ResponseFraming,
    needles: Vec<Vec<u8>>,
    keep: usize,
    /// Wire bytes buffered but not yet released to the guest.
    pending: Vec<u8>,
    /// Offsets into `pending` that carry scannable text, in wire order.
    scannable: Vec<usize>,
}

impl Http1ResponseRedactor {
    fn new(needles: &[Vec<u8>], head_request: bool) -> Self {
        Self {
            framing: Http1ResponseFraming::new(head_request),
            needles: needles.to_vec(),
            keep: needles
                .iter()
                .map(Vec::len)
                .max()
                .unwrap_or(1)
                .saturating_sub(1),
            pending: Vec::new(),
            scannable: Vec::new(),
        }
    }

    fn is_complete(&self) -> bool {
        self.framing.is_complete()
    }

    fn push(&mut self, input: &[u8], final_chunk: bool) -> Vec<u8> {
        let mut ranges = Vec::new();
        self.framing.classify(input, &mut ranges);
        let base = self.pending.len();
        self.pending.extend_from_slice(input);
        for (start, length) in ranges {
            self.scannable.extend(base + start..base + start + length);
        }

        let mut joined: Vec<u8> = self.scannable.iter().map(|at| self.pending[*at]).collect();
        redact_bytes(&mut joined, &self.needles);
        for (index, at) in self.scannable.iter().enumerate() {
            self.pending[*at] = joined[index];
        }

        // Hold back the last `keep` scannable bytes so a credential that
        // straddles this read and the next is still one contiguous match on the
        // next pass. Everything before the held region — including the framing
        // between the held bytes — is released now.
        let release_at = if final_chunk || self.keep == 0 {
            self.pending.len()
        } else if self.scannable.len() > self.keep {
            self.scannable[self.scannable.len() - self.keep]
        } else {
            self.scannable
                .first()
                .copied()
                .unwrap_or(self.pending.len())
        };
        let released: Vec<u8> = self.pending.drain(..release_at).collect();
        self.scannable.retain(|at| *at >= release_at);
        for at in &mut self.scannable {
            *at -= release_at;
        }
        released
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

    let mut redactor = Http1ResponseRedactor::new(needles, head_request);
    let first = redactor.push(&prefix, false);
    if !first.is_empty() {
        writer.write_all(&first).await?;
    }
    let mut buffer = [0_u8; 8192];
    loop {
        let read = match reader.read(&mut buffer).await {
            Ok(read) => read,
            Err(error)
                if error.kind() == std::io::ErrorKind::UnexpectedEof && redactor.is_complete() =>
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
/// declared length) with `on_truncation`. `body_end` is already known
/// `<= headers + cap`.
async fn read_to_content_length<C>(
    client_tls: &mut C,
    prefix: &mut Vec<u8>,
    body_end: usize,
    on_truncation: InterceptError,
) -> Result<(), InterceptError>
where
    C: AsyncRead + Unpin,
{
    while prefix.len() < body_end {
        let mut chunk = [0u8; 8192];
        let n = client_tls.read(&mut chunk).await?;
        if n == 0 {
            return Err(on_truncation);
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
        let mut redactor = Http1ResponseRedactor::new(response_redactions, head_request);
        if !response_redactions.is_empty() {
            let prefix = read_response_prefix(&mut up_rd).await?;
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
                        && redactor.is_complete() =>
                {
                    break;
                }
                Err(error) => return Err(error.into()),
            };
            let output = redactor.push(&tmp[..n], false);
            if client_wr.write_all(&output).await.is_err() {
                break;
            }
            if resp_buf.len() < budget {
                let take = (budget - resp_buf.len()).min(output.len());
                resp_buf.extend_from_slice(&output[..take]);
            }
        }
        let tail = redactor.push(&[], true);
        if !tail.is_empty() {
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

    #[tokio::test]
    async fn rejects_a_header_block_past_the_inspection_budget() {
        // A guest that pads its headers past the budget used to have the
        // remainder relayed verbatim, which put a second `Host` beyond the gate.
        let mut request = b"GET /v1 HTTP/1.1\r\nHost: api.example\r\n".to_vec();
        while request.len() < REQUEST_HEAD_BUDGET {
            request.extend_from_slice(b"X-Pad: 012345678901234567890123456789012345\r\n");
        }
        request.extend_from_slice(b"Host: attacker.example\r\n\r\n");
        let mut reader: &[u8] = &request;
        let mut prefix = Vec::new();

        let error = read_request_head(&mut reader, &mut prefix)
            .await
            .unwrap_err();

        assert!(matches!(error, InterceptError::RequestHeadTooLarge));
    }

    #[tokio::test]
    async fn reads_a_header_block_that_straddles_two_reads() {
        struct Dribble(Vec<Vec<u8>>);
        impl AsyncRead for Dribble {
            fn poll_read(
                mut self: Pin<&mut Self>,
                _cx: &mut Context<'_>,
                output: &mut ReadBuf<'_>,
            ) -> Poll<std::io::Result<()>> {
                if self.0.is_empty() {
                    return Poll::Ready(Ok(()));
                }
                let next = self.0.remove(0);
                output.put_slice(&next);
                Poll::Ready(Ok(()))
            }
        }
        // The CRLFCRLF terminator is split across the read boundary.
        let mut reader = Dribble(vec![
            b"GET /v1 HTTP/1.1\r\nHost: api.example\r".to_vec(),
            b"\n\r\nbody".to_vec(),
        ]);
        let mut prefix = Vec::new();

        let head_end = read_request_head(&mut reader, &mut prefix).await.unwrap();

        assert_eq!(
            &prefix[..head_end],
            b"GET /v1 HTTP/1.1\r\nHost: api.example\r\n\r\n"
        );
        assert_eq!(&prefix[head_end..], b"body");
    }

    #[test]
    fn rejects_smuggled_header_shapes() {
        assert!(validate_header_block(b"GET /v1 HTTP/1.1\r\nHost: api.example\r\n\r\n").is_ok());
        for (head, expected) in [
            (
                &b"GET /v1 HTTP/1.1\r\nHost: api.example\nHost: attacker.example\r\n\r\n"[..],
                "bare line feed",
            ),
            (
                &b"GET /v1 HTTP/1.1\r\nHost: api.example\r\n Host: attacker.example\r\n\r\n"[..],
                "obsolete line fold",
            ),
            (
                &b"GET /v1 HTTP/1.1\r\nHost : api.example\r\n\r\n"[..],
                "whitespace before the header colon",
            ),
            (
                &b"GET /v1 HTTP/1.1\r\nHost: api.example\r\nnonsense\r\n\r\n"[..],
                "header line without a colon",
            ),
            (
                &b"POST /v1 HTTP/1.1\r\nContent-Length: 5\r\nTransfer-Encoding: chunked\r\n\r\n"[..],
                "content-length with transfer-encoding",
            ),
            (
                &b"POST /v1 HTTP/1.1\r\nContent-Length: 5\r\nContent-Length: 6\r\n\r\n"[..],
                "conflicting content-length",
            ),
        ] {
            match validate_header_block(head) {
                Err(InterceptError::MalformedHeaderBlock { reason }) => {
                    assert_eq!(reason, expected);
                }
                other => panic!("expected `{expected}`, got {other:?}"),
            }
        }
    }

    fn placeholder_secret() -> SecretEntry {
        SecretEntry {
            placeholder: "engram_ph_x".into(),
            real_value: "sk-a-much-longer-real-value".into(),
            allow: HostList::from_manifest(&["api.example".into()], &[]).unwrap(),
        }
    }

    #[tokio::test]
    async fn substitution_rewrites_the_declared_body_length() {
        // A real secret is longer than its placeholder, so a stale
        // Content-Length truncates the request or hangs the connection.
        let secret = placeholder_secret();
        let secrets = [&secret];
        let body = br#"{"key":"engram_ph_x"}"#;
        let mut request = format!(
            "POST /v1 HTTP/1.1\r\nHost: api.example\r\nContent-Length: {}\r\n\r\n",
            body.len()
        )
        .into_bytes();
        request.extend_from_slice(body);
        let mut reader: &[u8] = &[];

        let out = substitute_request(&mut reader, request, "api.example", &secrets)
            .await
            .unwrap();

        let text = String::from_utf8(out).unwrap();
        let expected = br#"{"key":"sk-a-much-longer-real-value"}"#.len();
        assert!(
            text.ends_with(r#"{"key":"sk-a-much-longer-real-value"}"#),
            "{text}"
        );
        assert!(
            text.contains(&format!("Content-Length: {expected}\r\n")),
            "{text}"
        );
    }

    #[tokio::test]
    async fn substitution_buffers_a_body_that_arrives_after_the_head() {
        let secret = placeholder_secret();
        let secrets = [&secret];
        let body = br#"{"key":"engram_ph_x"}"#;
        let head = format!(
            "POST /v1 HTTP/1.1\r\nHost: api.example\r\nContent-Length: {}\r\n\r\n",
            body.len()
        )
        .into_bytes();
        let mut reader: &[u8] = body;

        let out = substitute_request(&mut reader, head, "api.example", &secrets)
            .await
            .unwrap();

        let text = String::from_utf8(out).unwrap();
        assert!(
            text.ends_with(r#"{"key":"sk-a-much-longer-real-value"}"#),
            "{text}"
        );
        assert!(text.contains("Content-Length: 37\r\n"), "{text}");
    }

    #[tokio::test]
    async fn a_chunked_body_streams_untouched_so_its_framing_stays_valid() {
        // We cannot rewrite chunk sizes mid-stream, so the body is relayed as
        // sent. A placeholder is not a secret, so this costs an API call at
        // worst — never a credential.
        let secret = placeholder_secret();
        let secrets = [&secret];
        let request =
            b"POST /v1 HTTP/1.1\r\nHost: api.example\r\nTransfer-Encoding: chunked\r\n\r\n\
              15\r\n{\"key\":\"engram_ph_x\"}\r\n0\r\n\r\n"
                .to_vec();
        let mut reader: &[u8] = &[];

        let out = substitute_request(&mut reader, request.clone(), "api.example", &secrets)
            .await
            .unwrap();

        assert_eq!(out, request);
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
    fn strips_method_override_headers_from_the_request() {
        // Google honours these, so a guest could send the POST our gate reads
        // and have the upstream run DELETE.
        let request = b"POST /v1/resource HTTP/1.1\r\nHost: guest.example\r\n\
                        X-HTTP-Method-Override: DELETE\r\nx-method-override: PUT\r\n\
                        X-Http-Method: PATCH\r\nAccept: */*\r\n\r\n"
            .to_vec();

        let bound = bind_http1_authority(request, "iam.googleapis.com");

        let text = String::from_utf8(bound).unwrap().to_ascii_lowercase();
        assert!(!text.contains("method-override"), "{text}");
        assert!(!text.contains("x-http-method:"), "{text}");
        assert!(text.contains("accept: */*"), "{text}");
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

    #[tokio::test]
    async fn redacts_a_credential_split_across_a_chunk_boundary() {
        // The wire bytes hold `host-\r\nb\r\ntoken-value` — the framing sits
        // between the two halves, so a raw scan finds nothing and the
        // credential used to reach the guest whole.
        let mut response = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n".to_vec();
        response.extend_from_slice(b"5\r\nhost-\r\n");
        response.extend_from_slice(b"b\r\ntoken-value\r\n");
        response.extend_from_slice(b"0\r\n\r\n");
        let mut reader: &[u8] = &response;
        let mut writer = VecWriter::default();

        copy_redacting_response(
            &mut reader,
            &mut writer,
            &[b"host-token-value".to_vec()],
            false,
        )
        .await
        .unwrap();

        let out = String::from_utf8(writer.bytes).unwrap();
        assert!(!out.contains("host-token-value"), "{out}");
        assert!(!out.contains("token-value"), "{out}");
        // Redaction is length-preserving, so the chunk sizes still describe the
        // bytes that follow them.
        assert!(out.contains("5\r\n*****\r\n"), "{out}");
        assert!(out.contains("b\r\n***********\r\n"), "{out}");
    }

    #[tokio::test]
    async fn redacts_a_credential_reflected_in_a_chunked_trailer() {
        let mut response =
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nTrailer: x-token\r\n\r\n".to_vec();
        response.extend_from_slice(b"2\r\nok\r\n");
        response.extend_from_slice(b"0\r\nx-token: host-token\r\n\r\n");
        let mut reader: &[u8] = &response;
        let mut writer = VecWriter::default();

        copy_redacting_response(&mut reader, &mut writer, &[b"host-token".to_vec()], false)
            .await
            .unwrap();

        let out = String::from_utf8(writer.bytes).unwrap();
        assert!(out.contains("x-token: **********\r\n"), "{out}");
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
        let mut transformer = PlaceholderTransformer::new(&secrets, &[]);
        let mut output = transformer.push(b"before-guest-", false).unwrap();
        output.extend(transformer.push(b"placeholder-after", false).unwrap());
        output.extend(transformer.push(&[], true).unwrap());
        assert_eq!(output, b"before-host-secret-after");
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
                StreamContext {
                    sni: "api.example",
                    port: 443,
                    secrets: &[&secret],
                    injects: &[&inject],
                    observes: &[],
                    foreign_placeholders: &[],
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
