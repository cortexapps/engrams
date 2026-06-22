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
pub fn inject_headers(prefix: Vec<u8>, entries: &[&InjectEntry]) -> Vec<u8> {
    if entries.is_empty() {
        return prefix;
    }
    let Some(end) = prefix.windows(2).position(|w| w == b"\r\n") else {
        return prefix; // no request line (defensive) — leave unchanged
    };
    let insert_at = end + 2; // just past the CRLF terminating the request line
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
        let p = RequestPolicy {
            methods: vec!["GET".into()],
            path_prefixes: vec!["/api/v2/logs".into()],
        };
        assert!(p.allows("GET", "/api/v2/logs/events"));
        assert!(p.allows("get", "/api/v2/logs/events")); // case-insensitive method
        assert!(!p.allows("POST", "/api/v2/logs/events")); // method not allowed
        assert!(!p.allows("GET", "/api/v2/metrics")); // path prefix not allowed

        // Empty policy = any method / any path.
        let any = RequestPolicy::default();
        assert!(any.allows("DELETE", "/anything"));
    }
}
