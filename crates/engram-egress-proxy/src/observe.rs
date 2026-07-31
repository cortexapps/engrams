//! ADR 0056 (Phase 4): response observation — the asset-emit engine.
//!
//! For a request matching an [`crate::registry::ObserveEntry`], the proxy
//! buffers the *response* (identity-encoded — `intercept::run` strips
//! `Accept-Encoding` and forces `Connection: close` on marked requests), parses
//! it, and evaluates the entry's `data`/`fetchable` map against the real
//! response bytes to build an [`ObservedAsset`]. It then hands the asset to an
//! [`ObserveSink`] (the host-agent forwards it to the coordinator, which appends
//! it as an `IntegrationAsset` session event — mirroring the harness-event path).
//!
//! The proxy is the *trusted observer*: assets come from the real response, never
//! a guest claim. **Robustness rule (ADR §4): emit a coarse asset even when
//! parsing fails** — a marked side effect must never occur without an event.
//!
//! Scope (ADR §5): HTTP/1.1 only; identity bodies (no gzip/br); a small
//! `$.resp.*` / `$.req.*` / `$.status` / `$.vars.*` extractor. Bodies are
//! bounded by the caller's response buffer.
//!
//! **GraphQL parity.** A REST create returns the full object, but a GraphQL
//! response echoes only the client's *selection set* (`gh pr create` selects
//! just `id`+`url`), so response-path extraction alone can never match REST.
//! Two first-class mechanisms close the gap:
//!   - `$.vars.<dotted>` extracts from the request's GraphQL `variables`. These
//!     are guest-authored, but they are the exact inputs the upstream accepted
//!     (the asset is only emitted after the success rule confirms the side
//!     effect) — the same values a REST response would echo back.
//!   - [`UrlFallback`] derives fields the response didn't carry from the
//!     *observed* fetchable URL (trusted response data), e.g. the PR number
//!     and repo out of `https://github.com/{owner}/{name}/pull/{number:int}`.
//!     Fallback only: it never overwrites an extracted field.

use std::sync::Arc;

use serde_json::{Map, Value};

use engram_core::SessionId;

use crate::registry::{ObserveEntry, SuccessRule};

/// A built asset, ready to ship to the coordinator. `surface` is opaque here
/// ("action" | "asset"); the coordinator maps it to its `AssetSurface`.
#[derive(Clone, Debug, PartialEq)]
pub struct ObservedAsset {
    pub provider: String,
    pub asset_kind: String,
    pub surface: String,
    pub data: Map<String, Value>,
    pub fetchable_url: Option<String>,
}

/// Fire-and-forget sink the proxy calls once per observed asset. Global to the
/// proxy (one host, many sessions), so it carries the `session_id`. The
/// host-agent's impl spawns the coordinator POST; tests collect into a buffer.
pub type ObserveSink = Arc<dyn Fn(SessionId, ObservedAsset) + Send + Sync>;

/// A parsed HTTP/1.1 response: status + de-chunked, identity-encoded body.
#[derive(Clone, Debug, PartialEq)]
pub struct ParsedResponse {
    pub status: u16,
    pub body: Vec<u8>,
}

/// Strip `Accept-Encoding` (force identity bodies) and any `Connection` header,
/// then add `Connection: close` (force a single response + prompt upstream EOF
/// so the observation completes without keep-alive bookkeeping). Operates on the
/// HTTP/1.1 header block; the body + `Content-Length` are untouched. ADR §5.
pub fn prepare_observed_request(prefix: Vec<u8>) -> Vec<u8> {
    rewrite_request_headers(prefix, true)
}

/// Force `Connection: close` on an intercepted request WITHOUT touching
/// `Accept-Encoding`. The gate/inject/substitute passes in `intercept::run`
/// see only the FIRST request on a connection — everything after the buffered
/// prefix streams verbatim (`copy_bidirectional`). A keep-alive client's
/// second request would therefore reach the upstream ungated and carrying the
/// guest's placeholder credential (GitHub answers that with `401 Bad
/// credentials` — the `gh pr create`/`gh pr checks` regression, 2026-07-21).
/// Closing after one response makes the client reconnect, so every request is
/// gated and injected.
///
/// `Upgrade` is stripped, never honored (PR #846 security review): the header
/// is guest-supplied, and an upstream that doesn't upgrade (any REST/GraphQL
/// API host) would ignore it and keep the connection persistent — letting a
/// guest reopen this exact bypass by decorating request #1. No intercepted
/// (credential/observe) host speaks websockets; supporting one would need an
/// explicit per-policy opt-in plus a 101-aware tunnel, not a client header.
pub fn force_connection_close(prefix: Vec<u8>) -> Vec<u8> {
    rewrite_request_headers(prefix, false)
}

fn rewrite_request_headers(prefix: Vec<u8>, strip_accept_encoding: bool) -> Vec<u8> {
    // Header block ends at the first CRLFCRLF; if absent (body not yet fully
    // buffered), rewrite up to whatever headers we have — the terminator will
    // arrive on the wire and our inserted `Connection: close` still lands in the
    // header region because we insert right after the request line.
    let req_line_end = match prefix.windows(2).position(|w| w == b"\r\n") {
        Some(p) => p + 2,
        None => return prefix, // no request line (defensive)
    };
    let headers_end = prefix
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|p| p + 2) // include the CRLF terminating the last header
        .unwrap_or(prefix.len());
    let header_region = &prefix[req_line_end..headers_end];

    let mut out = Vec::with_capacity(prefix.len() + 24);
    out.extend_from_slice(&prefix[..req_line_end]);
    // Re-emit each existing header line except the connection-management ones
    // (and Accept-Encoding when the caller needs an identity body to parse).
    for line in split_crlf(header_region) {
        if line.is_empty() {
            continue;
        }
        let name_lower = header_name_lower(line);
        if name_lower == "connection"
            || name_lower == "keep-alive"
            || name_lower == "proxy-connection"
            || name_lower == "upgrade"
            || (strip_accept_encoding && name_lower == "accept-encoding")
        {
            continue;
        }
        out.extend_from_slice(line);
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(b"Connection: close\r\n");
    // Re-attach the terminator + body (everything from headers_end onward,
    // which begins with the final CRLF of the header block when present).
    out.extend_from_slice(&prefix[headers_end..]);
    out
}

/// Split a byte region on CRLF into line slices (no trailing empties kept).
fn split_crlf(region: &[u8]) -> Vec<&[u8]> {
    let mut lines = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while i + 1 < region.len() {
        if &region[i..i + 2] == b"\r\n" {
            lines.push(&region[start..i]);
            i += 2;
            start = i;
        } else {
            i += 1;
        }
    }
    if start < region.len() {
        lines.push(&region[start..]);
    }
    lines
}

/// Lower-cased header name (the part before the first `:`) of a header line.
fn header_name_lower(line: &[u8]) -> String {
    let end = line.iter().position(|&b| b == b':').unwrap_or(line.len());
    String::from_utf8_lossy(&line[..end])
        .trim()
        .to_ascii_lowercase()
}

/// Parse a buffered HTTP/1.1 response into status + identity body. Returns
/// `None` only when there is no status line at all. Bodies are taken per
/// `Content-Length`, de-chunked for `Transfer-Encoding: chunked`, else
/// close-delimited (everything after the headers — correct under our forced
/// `Connection: close`).
pub fn parse_response(buf: &[u8]) -> Option<ParsedResponse> {
    let headers_end = buf.windows(4).position(|w| w == b"\r\n\r\n")?;
    let head = std::str::from_utf8(&buf[..headers_end]).ok()?;
    let mut lines = head.split("\r\n");

    // Status line: `HTTP/1.1 200 OK`.
    let status_line = lines.next()?;
    let status = status_line.split(' ').nth(1)?.parse::<u16>().ok()?;

    let mut content_length: Option<usize> = None;
    let mut chunked = false;
    for line in lines {
        let (name, value) = match line.split_once(':') {
            Some((n, v)) => (n.trim().to_ascii_lowercase(), v.trim()),
            None => continue,
        };
        match name.as_str() {
            "content-length" => content_length = value.parse::<usize>().ok(),
            "transfer-encoding" if value.to_ascii_lowercase().contains("chunked") => {
                chunked = true;
            }
            _ => {}
        }
    }

    let raw_body = &buf[headers_end + 4..];
    let body = if chunked {
        dechunk(raw_body)
    } else if let Some(len) = content_length {
        raw_body[..len.min(raw_body.len())].to_vec()
    } else {
        raw_body.to_vec()
    };
    Some(ParsedResponse { status, body })
}

/// De-chunk an HTTP/1.1 `Transfer-Encoding: chunked` body. Best-effort + bounded
/// by the input: a truncated stream yields whatever whole chunks were buffered.
fn dechunk(mut input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len());
    while let Some(crlf) = input.windows(2).position(|w| w == b"\r\n") {
        let size_line = &input[..crlf];
        // Chunk-extensions (`;name=val`) follow a `;` — ignore them.
        let hex = size_line
            .iter()
            .take_while(|&&b| b != b';')
            .copied()
            .collect::<Vec<u8>>();
        let size = match usize::from_str_radix(String::from_utf8_lossy(&hex).trim(), 16) {
            Ok(s) => s,
            Err(_) => break,
        };
        if size == 0 {
            break; // last chunk
        }
        let data_start = crlf + 2;
        let data_end = data_start + size;
        if data_end > input.len() {
            // Truncated — take what we have and stop.
            out.extend_from_slice(&input[data_start..]);
            break;
        }
        out.extend_from_slice(&input[data_start..data_end]);
        // Skip the trailing CRLF after the chunk data.
        input = input.get(data_end + 2..).unwrap_or(&[]);
    }
    out
}

/// ADR 0059: does a GraphQL response carry a non-empty top-level `errors` array?
/// Absent/empty `errors` → the operation succeeded. A body we couldn't parse
/// returns `false` (we can't prove failure); the coarse-emit path then still
/// records the side effect, preserving the side-effect ⟹ event invariant.
fn has_graphql_errors(body: Option<&Value>) -> bool {
    body.and_then(|b| b.get("errors"))
        .and_then(|e| e.as_array())
        .is_some_and(|a| !a.is_empty())
}

/// First-class URL-derived fallback for asset fields. A GraphQL response
/// carries only the client's selection set, so fields like the PR number may
/// be absent from the body while still being encoded in the returned URL —
/// which IS trusted response data. `pattern` is matched against the whole
/// extracted fetchable URL; `{name}` captures one non-empty run of characters
/// excluding `/`, `?`, `#`, and `{name:int}` additionally requires a base-10
/// integer (emitted as a JSON number). `fields` renders `(data field,
/// template)` pairs over the captures. Fail-soft everywhere: a malformed
/// pattern or a non-matching URL fills nothing (the asset still ships), and
/// derived values never overwrite a field the extractors already produced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UrlFallback {
    pub pattern: String,
    pub fields: Vec<(String, String)>,
}

/// One token of a parsed [`UrlFallback`] pattern or field template.
enum UrlTok<'a> {
    Lit(&'a str),
    Cap { name: &'a str, int: bool },
}

/// Tokenize a `{name}` / `{name:int}` template. `None` on a malformed template:
/// an unterminated `{`, an empty or non-`[A-Za-z0-9_]` name, or two adjacent
/// captures (ambiguous — there is no literal to delimit where one ends).
fn parse_url_template(template: &str) -> Option<Vec<UrlTok<'_>>> {
    let mut toks = Vec::new();
    let mut rest = template;
    while !rest.is_empty() {
        match rest.find('{') {
            None => {
                toks.push(UrlTok::Lit(rest));
                break;
            }
            Some(0) => {
                let end = rest.find('}')?;
                let inner = &rest[1..end];
                let (name, int) = match inner.strip_suffix(":int") {
                    Some(n) => (n, true),
                    None => (inner, false),
                };
                if name.is_empty()
                    || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
                    || matches!(toks.last(), Some(UrlTok::Cap { .. }))
                {
                    return None;
                }
                toks.push(UrlTok::Cap { name, int });
                rest = &rest[end + 1..];
            }
            Some(i) => {
                toks.push(UrlTok::Lit(&rest[..i]));
                rest = &rest[i..];
            }
        }
    }
    Some(toks)
}

impl UrlFallback {
    /// Match `url` against the pattern and render every field template whose
    /// captures resolved. `(field, value)` pairs; empty on any failure.
    fn derive(&self, url: &str) -> Vec<(String, Value)> {
        let Some(toks) = parse_url_template(&self.pattern) else {
            return Vec::new();
        };
        // Match: a capture spans up to the next literal's first occurrence
        // (captures can't contain `/`, so segment-shaped patterns stay exact).
        let mut caps: Vec<(&str, &str, bool)> = Vec::new();
        let mut pos = 0;
        for (i, tok) in toks.iter().enumerate() {
            match tok {
                UrlTok::Lit(l) => {
                    if !url[pos..].starts_with(l) {
                        return Vec::new();
                    }
                    pos += l.len();
                }
                UrlTok::Cap { name, int } => {
                    let end = match toks.get(i + 1) {
                        Some(UrlTok::Lit(next)) => match url[pos..].find(next) {
                            Some(off) => pos + off,
                            None => return Vec::new(),
                        },
                        // Adjacent captures are rejected at parse; a capture is
                        // otherwise last and takes the remainder.
                        Some(UrlTok::Cap { .. }) => return Vec::new(),
                        None => url.len(),
                    };
                    let val = &url[pos..end];
                    if val.is_empty()
                        || val.contains(['/', '?', '#'])
                        || (*int && val.parse::<u64>().is_err())
                    {
                        return Vec::new();
                    }
                    caps.push((name, val, *int));
                    pos = end;
                }
            }
        }
        if pos != url.len() {
            return Vec::new();
        }

        let mut out = Vec::new();
        'fields: for (field, template) in &self.fields {
            let Some(ttoks) = parse_url_template(template) else {
                continue;
            };
            // A template that is exactly one capture of an `:int` pattern
            // capture emits a JSON number; every other shape renders a string.
            if let [UrlTok::Cap { name, .. }] = ttoks.as_slice() {
                if let Some((_, val, int)) = caps.iter().find(|(n, _, _)| n == name) {
                    let value = if *int {
                        match val.parse::<u64>() {
                            Ok(n) => Value::from(n),
                            Err(_) => continue, // unreachable: validated at match
                        }
                    } else {
                        Value::String((*val).to_string())
                    };
                    out.push((field.clone(), value));
                }
                continue;
            }
            let mut rendered = String::new();
            for tok in &ttoks {
                match tok {
                    UrlTok::Lit(l) => rendered.push_str(l),
                    UrlTok::Cap { name, .. } => {
                        match caps.iter().find(|(n, _, _)| n == name) {
                            Some((_, val, _)) => rendered.push_str(val),
                            None => continue 'fields, // unknown capture → skip field
                        }
                    }
                }
            }
            out.push((field.clone(), Value::String(rendered)));
        }
        out
    }
}

/// Evaluate one extractor path against the request/response. Supports
/// `$.status`, `$.req.method`, `$.req.path`, `$.resp` (whole body) /
/// `$.resp.<dotted>` (nested object keys), and `$.vars` / `$.vars.<dotted>`
/// (the GraphQL request's `variables` — `None` for REST requests). Returns
/// `None` if the path doesn't resolve.
fn extract(
    path: &str,
    status: u16,
    method: &str,
    req_path: &str,
    body: Option<&Value>,
    gql_vars: Option<&Value>,
) -> Option<Value> {
    let rest = path.strip_prefix("$.")?;
    match rest {
        "status" => return Some(Value::from(status)),
        "req.method" => return Some(Value::String(method.to_string())),
        "req.path" => return Some(Value::String(req_path.to_string())),
        _ => {}
    }
    let (root, dotted) = if let Some(p) = rest.strip_prefix("vars") {
        (gql_vars?, p)
    } else {
        (body?, rest.strip_prefix("resp")?)
    };
    if dotted.is_empty() {
        return Some(root.clone());
    }
    let mut cur = root;
    for key in dotted.strip_prefix('.')?.split('.') {
        cur = cur.get(key)?;
    }
    Some(cur.clone())
}

/// Build the asset from a (possibly absent) parsed response. Returns `None` only
/// when the response parsed AND its status fails the entry's success rule (the
/// side effect did not occur — e.g. a 422 on issue-create). When the status
/// passes but the `data`/`fetchable` map extracts nothing (non-JSON body, missed
/// paths) — or when the response is wholly unparseable — a **coarse** asset is
/// emitted (`_status`/`_method`/`_path`) so the invariant holds.
///
/// `gql_vars` is the request's GraphQL `variables` (the `$.vars.*` extractor
/// source); `None` for REST requests. After extraction, the entry's
/// [`UrlFallback`] fills any still-missing `data` fields from the extracted
/// fetchable URL — fallback only, never overwriting an extracted value.
pub fn evaluate(
    entry: &ObserveEntry,
    parsed: Option<&ParsedResponse>,
    method: &str,
    req_path: &str,
    gql_vars: Option<&Value>,
) -> Option<ObservedAsset> {
    let mut data = Map::new();
    let mut fetchable_url = None;

    match parsed {
        Some(resp) => {
            if !entry.success.satisfied_by(resp.status) {
                return None;
            }
            let body_json: Option<Value> = serde_json::from_slice(&resp.body).ok();
            // ADR 0059: GraphQL success also requires no top-level `errors`. A 200
            // carrying a non-empty `errors` array means the operation failed — no
            // asset (mirrors the REST 422 path). An unparseable body can't be
            // checked, so it falls through to the coarse-emit below.
            if entry.success == SuccessRule::NoGraphqlErrors
                && has_graphql_errors(body_json.as_ref())
            {
                return None;
            }
            for (field, path) in &entry.data {
                if let Some(v) = extract(
                    path,
                    resp.status,
                    method,
                    req_path,
                    body_json.as_ref(),
                    gql_vars,
                ) {
                    data.insert(field.clone(), v);
                }
            }
            if let Some(p) = &entry.fetchable {
                fetchable_url = extract(
                    p,
                    resp.status,
                    method,
                    req_path,
                    body_json.as_ref(),
                    gql_vars,
                )
                .and_then(|v| v.as_str().map(str::to_string));
            }
            if let (Some(fb), Some(url)) = (&entry.url_fallback, fetchable_url.as_deref()) {
                for (field, value) in fb.derive(url) {
                    data.entry(field).or_insert(value);
                }
            }
            if data.is_empty() && fetchable_url.is_none() {
                // Coarse: parsed + success but nothing mapped.
                data.insert("_status".into(), Value::from(resp.status));
                data.insert("_method".into(), Value::String(method.to_string()));
                data.insert("_path".into(), Value::String(req_path.to_string()));
            }
        }
        None => {
            // Unparseable — can't check success. Coarse-emit to preserve the
            // side-effect ⟹ event invariant rather than silently drop.
            data.insert("_method".into(), Value::String(method.to_string()));
            data.insert("_path".into(), Value::String(req_path.to_string()));
        }
    }

    Some(ObservedAsset {
        provider: entry.provider.clone(),
        asset_kind: entry.asset_kind.clone(),
        surface: entry.surface.clone(),
        data,
        fetchable_url,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::HostList;
    use crate::registry::{RequestPolicy, SuccessRule};

    fn entry() -> ObserveEntry {
        ObserveEntry {
            allow: HostList::from_manifest(&["api.github.com".into()], &[]).unwrap(),
            policy: RequestPolicy {
                methods: vec!["POST".into()],
                path_globs: vec!["/repos/*/issues".into()],
                graphql: None,
            },
            provider: "github".into(),
            asset_kind: "issue".into(),
            surface: "asset".into(),
            success: SuccessRule::StatusClass2xx,
            data: vec![
                ("number".into(), "$.resp.number".into()),
                ("title".into(), "$.resp.title".into()),
            ],
            fetchable: Some("$.resp.html_url".into()),
            url_fallback: None,
        }
    }

    #[test]
    fn parses_content_length_body() {
        let raw = b"HTTP/1.1 201 Created\r\nContent-Length: 13\r\n\r\n{\"number\":42}";
        let r = parse_response(raw).unwrap();
        assert_eq!(r.status, 201);
        assert_eq!(r.body, b"{\"number\":42}");
    }

    #[test]
    fn parses_chunked_body() {
        // "{\"a\":1}" split into two chunks: "4\r\n{\"a:\r\n" then "3\r\n\":1}\r\n0\r\n\r\n"
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\n{\"a\r\n4\r\n\":1}\r\n0\r\n\r\n";
        let r = parse_response(raw).unwrap();
        assert_eq!(r.status, 200);
        assert_eq!(r.body, b"{\"a\":1}");
    }

    #[test]
    fn parse_returns_none_without_headers() {
        assert!(parse_response(b"HTTP/1.1 200 OK\r\nContent-Length: 0").is_none());
    }

    #[test]
    fn extract_paths() {
        let body: Value = serde_json::json!({"number": 42, "meta": {"page": {"total_count": 7}}});
        assert_eq!(
            extract("$.resp.number", 200, "POST", "/p", Some(&body), None),
            Some(Value::from(42))
        );
        assert_eq!(
            extract(
                "$.resp.meta.page.total_count",
                200,
                "POST",
                "/p",
                Some(&body),
                None
            ),
            Some(Value::from(7))
        );
        assert_eq!(
            extract("$.status", 201, "POST", "/p", Some(&body), None),
            Some(Value::from(201))
        );
        assert_eq!(
            extract("$.req.method", 200, "POST", "/p", Some(&body), None),
            Some(Value::String("POST".into()))
        );
        assert_eq!(
            extract("$.resp.missing", 200, "POST", "/p", Some(&body), None),
            None
        );
        assert_eq!(
            extract("$.resp.number", 200, "POST", "/p", None, None),
            None
        );
    }

    #[test]
    fn extract_vars_paths() {
        // `$.vars.*` reads the GraphQL request variables; absent vars (REST) → None.
        let vars: Value = serde_json::json!({"input": {"title": "Fix", "headRefName": "b"}});
        assert_eq!(
            extract(
                "$.vars.input.title",
                200,
                "POST",
                "/graphql",
                None,
                Some(&vars)
            ),
            Some(Value::String("Fix".into()))
        );
        assert_eq!(
            extract("$.vars", 200, "POST", "/graphql", None, Some(&vars)),
            Some(vars.clone())
        );
        assert_eq!(
            extract(
                "$.vars.input.missing",
                200,
                "POST",
                "/graphql",
                None,
                Some(&vars)
            ),
            None
        );
        assert_eq!(
            extract("$.vars.input.title", 200, "POST", "/graphql", None, None),
            None
        );
    }

    #[test]
    fn prepare_strips_accept_encoding_and_forces_close() {
        let req = b"POST /repos/x/issues HTTP/1.1\r\nHost: api.github.com\r\nAccept-Encoding: gzip, br\r\nConnection: keep-alive\r\nContent-Length: 2\r\n\r\n{}".to_vec();
        let out = prepare_observed_request(req);
        let s = String::from_utf8(out).unwrap();
        assert!(!s.to_ascii_lowercase().contains("accept-encoding"));
        assert!(!s.contains("keep-alive"));
        assert!(s.contains("Connection: close\r\n"));
        assert!(s.starts_with("POST /repos/x/issues HTTP/1.1\r\n"));
        assert!(s.contains("Host: api.github.com\r\n"));
        assert!(s.ends_with("\r\n\r\n{}")); // body + Content-Length preserved
        assert!(s.contains("Content-Length: 2\r\n"));
    }

    #[test]
    fn force_close_keeps_accept_encoding_and_drops_keepalive_headers() {
        let req = b"GET /repos/x HTTP/1.1\r\nHost: api.github.com\r\nAccept-Encoding: gzip\r\nConnection: keep-alive\r\nKeep-Alive: timeout=5\r\nProxy-Connection: keep-alive\r\n\r\n".to_vec();
        let out = force_connection_close(req);
        let s = String::from_utf8(out).unwrap();
        assert!(s.contains("Accept-Encoding: gzip\r\n"));
        assert!(!s.to_ascii_lowercase().contains("keep-alive"));
        assert!(!s.to_ascii_lowercase().contains("proxy-connection"));
        assert_eq!(s.matches("Connection: close\r\n").count(), 1);
        assert!(s.ends_with("\r\n\r\n"));
    }

    // PR #846 security review: `Upgrade` is guest-supplied — honoring it would
    // let the guest suppress the forced close (an upstream that doesn't
    // upgrade keeps the connection persistent) and reopen the keep-alive
    // bypass. It must be stripped and the close still forced.
    #[test]
    fn force_close_strips_guest_supplied_upgrade() {
        let req =
            b"GET /socket HTTP/1.1\r\nHost: h\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\r\n"
                .to_vec();
        let out = force_connection_close(req);
        let s = String::from_utf8(out).unwrap();
        assert!(!s.to_ascii_lowercase().contains("upgrade"));
        assert_eq!(s.matches("Connection: close\r\n").count(), 1);
        assert!(s.contains("Host: h\r\n"));
    }

    #[test]
    fn evaluate_rich_asset_on_success() {
        let raw = b"HTTP/1.1 201 Created\r\nContent-Length: 58\r\n\r\n{\"number\":42,\"title\":\"Bug\",\"html_url\":\"http://x/i/42\"}";
        let parsed = parse_response(raw);
        let a = evaluate(&entry(), parsed.as_ref(), "POST", "/repos/x/issues", None).unwrap();
        assert_eq!(a.provider, "github");
        assert_eq!(a.asset_kind, "issue");
        assert_eq!(a.data.get("number"), Some(&Value::from(42)));
        assert_eq!(a.data.get("title"), Some(&Value::String("Bug".into())));
        assert_eq!(a.fetchable_url.as_deref(), Some("http://x/i/42"));
    }

    #[test]
    fn evaluate_drops_on_failed_success_rule() {
        let raw = b"HTTP/1.1 422 Unprocessable\r\nContent-Length: 2\r\n\r\n{}";
        let parsed = parse_response(raw);
        assert!(evaluate(&entry(), parsed.as_ref(), "POST", "/repos/x/issues", None).is_none());
    }

    #[test]
    fn evaluate_coarse_on_success_but_unmapped_body() {
        // 200 but a non-JSON body → the data map extracts nothing → coarse asset.
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nhi";
        let parsed = parse_response(raw);
        let a = evaluate(&entry(), parsed.as_ref(), "POST", "/repos/x/issues", None).unwrap();
        assert_eq!(a.data.get("_status"), Some(&Value::from(200u16)));
        assert_eq!(
            a.data.get("_path"),
            Some(&Value::String("/repos/x/issues".into()))
        );
        assert!(a.fetchable_url.is_none());
    }

    #[test]
    fn evaluate_coarse_on_unparseable_response() {
        let a = evaluate(&entry(), None, "POST", "/repos/x/issues", None).unwrap();
        assert_eq!(a.data.get("_method"), Some(&Value::String("POST".into())));
        assert!(!a.data.contains_key("_status"));
    }

    fn pr_fallback() -> UrlFallback {
        UrlFallback {
            pattern: "https://github.com/{owner}/{name}/pull/{number:int}".into(),
            fields: vec![
                ("repo".into(), "{owner}/{name}".into()),
                ("number".into(), "{number}".into()),
            ],
        }
    }

    #[test]
    fn url_fallback_derives_typed_fields() {
        let got = pr_fallback().derive("https://github.com/octo/engrams/pull/97");
        assert_eq!(
            got,
            vec![
                ("repo".into(), Value::String("octo/engrams".into())),
                ("number".into(), Value::from(97u64)), // `:int` → JSON number
            ]
        );
    }

    #[test]
    fn url_fallback_rejects_non_matching_urls() {
        let fb = pr_fallback();
        // Wrong shape, non-integer capture, trailing garbage, query string.
        assert!(fb
            .derive("https://github.com/octo/engrams/issues/97")
            .is_empty());
        assert!(fb
            .derive("https://github.com/octo/engrams/pull/abc")
            .is_empty());
        assert!(fb
            .derive("https://github.com/octo/engrams/pull/97/files")
            .is_empty());
        assert!(fb
            .derive("https://github.com/octo/engrams/pull/97?w=1")
            .is_empty());
    }

    #[test]
    fn url_fallback_malformed_pattern_fills_nothing() {
        for pattern in ["https://x/{unclosed", "https://x/{}/y", "https://x/{a}{b}"] {
            let fb = UrlFallback {
                pattern: pattern.into(),
                fields: vec![("f".into(), "{a}".into())],
            };
            assert!(fb.derive("https://x/v/y").is_empty(), "pattern `{pattern}`");
        }
    }

    #[test]
    fn evaluate_graphql_parity_via_vars_and_url_fallback() {
        // The `gh pr create` shape: the mutation selected only id+url, so the
        // body carries neither number, title, nor repo. Parity comes from
        // `$.vars.*` (title, branches) + the URL fallback (repo, number).
        let e = ObserveEntry {
            success: SuccessRule::NoGraphqlErrors,
            data: vec![
                (
                    "number".into(),
                    "$.resp.data.createPullRequest.pullRequest.number".into(),
                ),
                ("title".into(), "$.vars.input.title".into()),
                (
                    "repo".into(),
                    "$.resp.data.createPullRequest.pullRequest.repository.nameWithOwner".into(),
                ),
                ("head_branch".into(), "$.vars.input.headRefName".into()),
                ("base_branch".into(), "$.vars.input.baseRefName".into()),
            ],
            fetchable: Some("$.resp.data.createPullRequest.pullRequest.url".into()),
            url_fallback: Some(pr_fallback()),
            ..entry()
        };
        let body = r#"{"data":{"createPullRequest":{"pullRequest":{"id":"PR_1","url":"https://github.com/octo/engrams/pull/97"}}}}"#;
        let raw = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let parsed = parse_response(raw.as_bytes());
        let vars = serde_json::json!({"input": {
            "repositoryId": "R_1",
            "title": "Fix the flux capacitor",
            "headRefName": "fix-flux",
            "baseRefName": "main",
        }});
        let a = evaluate(&e, parsed.as_ref(), "POST", "/graphql", Some(&vars)).unwrap();
        assert_eq!(
            a.data.get("title"),
            Some(&Value::String("Fix the flux capacitor".into()))
        );
        assert_eq!(
            a.data.get("head_branch"),
            Some(&Value::String("fix-flux".into()))
        );
        assert_eq!(
            a.data.get("base_branch"),
            Some(&Value::String("main".into()))
        );
        assert_eq!(
            a.data.get("repo"),
            Some(&Value::String("octo/engrams".into()))
        );
        assert_eq!(a.data.get("number"), Some(&Value::from(97u64)));
        assert_eq!(
            a.fetchable_url.as_deref(),
            Some("https://github.com/octo/engrams/pull/97")
        );
    }

    #[test]
    fn url_fallback_never_overwrites_extracted_fields() {
        // A client that DID select `number` gets the response value even when
        // the URL would derive something else (fallback fills, never clobbers).
        let e = ObserveEntry {
            data: vec![("number".into(), "$.resp.number".into())],
            fetchable: Some("$.resp.html_url".into()),
            url_fallback: Some(pr_fallback()),
            ..entry()
        };
        let body = r#"{"number":42,"html_url":"https://github.com/octo/engrams/pull/97"}"#;
        let raw = format!(
            "HTTP/1.1 201 Created\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let parsed = parse_response(raw.as_bytes());
        let a = evaluate(
            &e,
            parsed.as_ref(),
            "POST",
            "/repos/octo/engrams/pulls",
            None,
        )
        .unwrap();
        assert_eq!(a.data.get("number"), Some(&Value::from(42)));
        assert_eq!(
            a.data.get("repo"),
            Some(&Value::String("octo/engrams".into()))
        );
    }
}
