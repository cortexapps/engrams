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
//! `$.resp.*` / `$.req.*` / `$.status` extractor. Bodies are bounded by the
//! caller's response buffer.

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

/// Evaluate one extractor path against the response. Supports `$.status`,
/// `$.req.method`, `$.req.path`, `$.resp` (whole body), and `$.resp.<dotted>`
/// (nested object keys). Returns `None` if the path doesn't resolve.
fn extract(
    path: &str,
    status: u16,
    method: &str,
    req_path: &str,
    body: Option<&Value>,
) -> Option<Value> {
    let rest = path.strip_prefix("$.")?;
    match rest {
        "status" => return Some(Value::from(status)),
        "req.method" => return Some(Value::String(method.to_string())),
        "req.path" => return Some(Value::String(req_path.to_string())),
        _ => {}
    }
    let resp_path = rest.strip_prefix("resp")?;
    let body = body?;
    if resp_path.is_empty() {
        return Some(body.clone());
    }
    let mut cur = body;
    for key in resp_path.strip_prefix('.')?.split('.') {
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
pub fn evaluate(
    entry: &ObserveEntry,
    parsed: Option<&ParsedResponse>,
    method: &str,
    req_path: &str,
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
                if let Some(v) = extract(path, resp.status, method, req_path, body_json.as_ref()) {
                    data.insert(field.clone(), v);
                }
            }
            if let Some(p) = &entry.fetchable {
                fetchable_url = extract(p, resp.status, method, req_path, body_json.as_ref())
                    .and_then(|v| v.as_str().map(str::to_string));
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
            extract("$.resp.number", 200, "POST", "/p", Some(&body)),
            Some(Value::from(42))
        );
        assert_eq!(
            extract(
                "$.resp.meta.page.total_count",
                200,
                "POST",
                "/p",
                Some(&body)
            ),
            Some(Value::from(7))
        );
        assert_eq!(
            extract("$.status", 201, "POST", "/p", Some(&body)),
            Some(Value::from(201))
        );
        assert_eq!(
            extract("$.req.method", 200, "POST", "/p", Some(&body)),
            Some(Value::String("POST".into()))
        );
        assert_eq!(
            extract("$.resp.missing", 200, "POST", "/p", Some(&body)),
            None
        );
        assert_eq!(extract("$.resp.number", 200, "POST", "/p", None), None);
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
        let a = evaluate(&entry(), parsed.as_ref(), "POST", "/repos/x/issues").unwrap();
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
        assert!(evaluate(&entry(), parsed.as_ref(), "POST", "/repos/x/issues").is_none());
    }

    #[test]
    fn evaluate_coarse_on_success_but_unmapped_body() {
        // 200 but a non-JSON body → the data map extracts nothing → coarse asset.
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nhi";
        let parsed = parse_response(raw);
        let a = evaluate(&entry(), parsed.as_ref(), "POST", "/repos/x/issues").unwrap();
        assert_eq!(a.data.get("_status"), Some(&Value::from(200u16)));
        assert_eq!(
            a.data.get("_path"),
            Some(&Value::String("/repos/x/issues".into()))
        );
        assert!(a.fetchable_url.is_none());
    }

    #[test]
    fn evaluate_coarse_on_unparseable_response() {
        let a = evaluate(&entry(), None, "POST", "/repos/x/issues").unwrap();
        assert_eq!(a.data.get("_method"), Some(&Value::String("POST".into())));
        assert!(!a.data.contains_key("_status"));
    }
}
