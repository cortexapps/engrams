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
//! - **HTTP/1.1 only.** ALPN advertises only `http/1.1`; an upstream
//!   that wants H2 will fall back. H2 substitution lands later.
//! - **Response safety.** A host-issued credential must not reach the guest,
//!   even when an upstream reflects it. The adapter strips content negotiation,
//!   rejects encoded responses, and redacts credential bytes across chunks.

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
}

impl std::fmt::Display for InterceptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "io: {e}"),
            Self::Tls(e) => write!(f, "tls: {e}"),
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
    let cfg = ServerConfig::builder()
        .with_no_client_auth()
        .with_cert_resolver(Arc::new(resolver));
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
    let cfg = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
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
    let response =
        copy_redacting_response(&mut upstream_read, &mut client_write, &response_redactions);
    tokio::try_join!(request, response)?;
    Ok(())
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

    let mut redactor = ByteRedactor::new(needles);
    let first = redactor.push(&prefix, false);
    if !first.is_empty() {
        writer.write_all(&first).await?;
    }
    let mut buffer = [0_u8; 8192];
    loop {
        let read = reader.read(&mut buffer).await?;
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
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
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
}
