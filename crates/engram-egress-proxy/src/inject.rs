//! ADR 0056: Plane-B credential injection + request-shape gating.
//!
//! For an `Intercept` decision carrying inject entries, the proxy parses
//! the HTTP/1.1 request line, the caller enforces each applicable entry's
//! [`crate::registry::RequestPolicy`] (method + path prefix), and on a
//! match this module adds the entry's auth header — carrying the host-side
//! secret the guest never sees. (A request to an inject-gated host whose
//! shape matches no entry is rejected by the caller.)

use crate::registry::InjectEntry;

/// Parse `METHOD SP request-target SP HTTP/x` from the buffered request
/// prefix → `(method, path)`. `None` when the prefix has no complete
/// request line (shouldn't happen — `intercept::run` buffers to the header
/// terminator before calling this).
pub fn request_line(prefix: &[u8]) -> Option<(String, String)> {
    let end = prefix.windows(2).position(|w| w == b"\r\n")?;
    let line = std::str::from_utf8(prefix.get(..end)?).ok()?;
    let mut parts = line.split(' ');
    let method = parts.next()?;
    let path = parts.next()?;
    if method.is_empty() || path.is_empty() {
        return None;
    }
    Some((method.to_string(), path.to_string()))
}

/// Add each entry's `header_name: <template with {} → secret>` line after
/// the request line. HTTP/1.1: headers follow the request line, so
/// inserting right after the first CRLF is always valid and leaves
/// `Content-Length` / the body untouched. Entries are assumed already
/// gated by the caller (their `RequestPolicy` allowed this request).
///
/// ADR 0056 P3 (overwrite semantics): any existing request header whose name
/// matches one we're about to inject is **stripped first**, so the brokered
/// credential always wins — no duplicate `Authorization`, and a guest-supplied
/// header can't shadow the injected one. This matters now that a mint provider's
/// token rides this plane (P1) and the guest may issue authenticated API calls
/// itself (e.g. `gh` / curl with its own header).
pub fn inject_headers(prefix: Vec<u8>, entries: &[&InjectEntry]) -> Vec<u8> {
    if entries.is_empty() {
        return prefix;
    }
    let Some(end) = prefix.windows(2).position(|w| w == b"\r\n") else {
        return prefix; // no request line (defensive) — leave unchanged
    };
    let insert_at = end + 2; // just past the CRLF terminating the request line
                             // Drop any existing header line whose name we're about to inject. The request
                             // line is untouched, so `insert_at` is stable across the strip.
    let prefix = strip_named_headers(&prefix, insert_at, entries);
    let mut out = Vec::with_capacity(prefix.len() + 96 * entries.len());
    out.extend_from_slice(&prefix[..insert_at]);
    for e in entries {
        out.extend_from_slice(e.header_name.as_bytes());
        out.extend_from_slice(b": ");
        out.extend_from_slice(e.header_template.replace("{}", &e.secret).as_bytes());
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(&prefix[insert_at..]);
    out
}

/// Rebuild the request with any header line whose name (case-insensitively)
/// matches an inject entry removed. Walks the header block line-by-line from
/// `insert_at` and copies the empty line + body verbatim. Defensive on a
/// malformed/partial prefix: copies the remainder unchanged.
fn strip_named_headers(prefix: &[u8], insert_at: usize, entries: &[&InjectEntry]) -> Vec<u8> {
    let mut out = Vec::with_capacity(prefix.len());
    out.extend_from_slice(&prefix[..insert_at]);
    let mut i = insert_at;
    loop {
        let Some(rel) = prefix[i..].windows(2).position(|w| w == b"\r\n") else {
            out.extend_from_slice(&prefix[i..]); // no CRLF left — copy verbatim
            break;
        };
        let line_end = i + rel; // index of the `\r`
        let line = &prefix[i..line_end];
        if line.is_empty() {
            // Empty line = end of headers; copy it and the body verbatim.
            out.extend_from_slice(&prefix[i..]);
            break;
        }
        let name = match line.iter().position(|&b| b == b':') {
            Some(c) => &line[..c],
            None => line, // headerless line (shouldn't happen) — keep it
        };
        let drop = entries
            .iter()
            .any(|e| e.header_name.as_bytes().eq_ignore_ascii_case(name));
        if !drop {
            out.extend_from_slice(&prefix[i..line_end + 2]); // keep, incl. CRLF
        }
        i = line_end + 2;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::HostList;
    use crate::registry::RequestPolicy;

    fn entry(header: &str, template: &str, secret: &str) -> InjectEntry {
        InjectEntry {
            secret: secret.into(),
            header_name: header.into(),
            header_template: template.into(),
            allow: HostList::from_manifest(&["api.datadoghq.com".into()], &[]).unwrap(),
            policy: RequestPolicy::default(),
        }
    }

    #[test]
    fn parses_request_line() {
        let p = b"GET /api/v2/logs/events?x=1 HTTP/1.1\r\nHost: x\r\n\r\n";
        assert_eq!(
            request_line(p),
            Some(("GET".into(), "/api/v2/logs/events?x=1".into()))
        );
    }

    #[test]
    fn request_line_none_without_crlf() {
        assert_eq!(request_line(b"GET /x HTTP/1.1"), None);
    }

    #[test]
    fn injects_header_after_request_line_preserving_body() {
        let req =
            b"POST /q HTTP/1.1\r\nHost: api.datadoghq.com\r\nContent-Length: 2\r\n\r\n{}".to_vec();
        let e = entry("DD-API-KEY", "{}", "dd-secret");
        let out = inject_headers(req, &[&e]);
        let s = std::str::from_utf8(&out).unwrap();
        // Header lands right after the request line, before Host.
        assert!(s.starts_with("POST /q HTTP/1.1\r\nDD-API-KEY: dd-secret\r\nHost:"));
        // Body + Content-Length untouched.
        assert!(s.ends_with("\r\n\r\n{}"));
    }

    #[test]
    fn overwrites_an_existing_same_name_header() {
        // ADR 0056 P3: a guest-supplied (lowercase) Authorization is stripped;
        // the brokered one wins — exactly one Authorization, no duplicate.
        let req = b"GET /repos/x HTTP/1.1\r\nHost: api.github.com\r\n\
                    authorization: Bearer guest\r\nAccept: */*\r\n\r\n"
            .to_vec();
        let e = entry("Authorization", "Bearer {}", "brokered");
        let out = inject_headers(req, &[&e]);
        let s = std::str::from_utf8(&out).unwrap();
        assert_eq!(s.to_ascii_lowercase().matches("authorization:").count(), 1);
        assert!(s.contains("Authorization: Bearer brokered"));
        assert!(!s.contains("Bearer guest"));
        // Untouched headers survive.
        assert!(s.contains("Host: api.github.com"));
        assert!(s.contains("Accept: */*"));
    }

    #[test]
    fn template_substitutes_brace_placeholder() {
        let req = b"GET /x HTTP/1.1\r\n\r\n".to_vec();
        let e = entry("Authorization", "Bearer {}", "tok123");
        let out = inject_headers(req, &[&e]);
        assert!(std::str::from_utf8(&out)
            .unwrap()
            .contains("Authorization: Bearer tok123\r\n"));
    }

    #[test]
    fn request_policy_allows_method_and_path() {
        // A trailing `*` lets the glob swallow the query string + sub-paths —
        // the shape the orchestrator emits for read ops (e.g. datadog logs).
        let p = RequestPolicy {
            methods: vec!["GET".into()],
            path_prefixes: vec!["/api/v2/logs*".into()],
        };
        assert!(p.allows("GET", "/api/v2/logs/events"));
        assert!(p.allows("GET", "/api/v2/logs/events?query=x")); // query swallowed by `*`
        assert!(p.allows("get", "/api/v2/logs/events")); // case-insensitive method
        assert!(!p.allows("POST", "/api/v2/logs/events")); // method not allowed
        assert!(!p.allows("GET", "/api/v2/metrics")); // path doesn't match

        // The granularity fix: a write op pinned to `/repos/*/pulls` must match
        // the create-PR call but NOT a sibling `/repos/o/r/git/refs` (the coarse
        // `/repos/` prefix used to over-match both — injecting the token and
        // emitting a junk asset on branch creation).
        let pulls = RequestPolicy {
            methods: vec!["POST".into()],
            path_prefixes: vec!["/repos/*/pulls".into()],
        };
        assert!(pulls.allows("POST", "/repos/octo/repo/pulls"));
        assert!(!pulls.allows("POST", "/repos/octo/repo/git/refs"));
        assert!(!pulls.allows("POST", "/repos/octo/repo/pulls/1/merge")); // deeper, no trailing `*`

        // Empty policy = any method / any path.
        let any = RequestPolicy::default();
        assert!(any.allows("DELETE", "/anything"));
    }
}
