//! HTTP/1.1 placeholder substitution on decrypted plaintext.
//!
//! After we MITM TLS for an `Intercept` decision, the request bytes
//! reach us in plaintext. We scan for placeholders that belong to
//! this session and replace them with the real value before forwarding
//! upstream. Body and headers are both fair game (microsandbox does
//! the same).
//!
//! Two pieces of logic, kept separate so they're each testable:
//!
//! - [`substitute`] — for placeholders whose `allow_hosts` includes
//!   the destination, replace placeholder bytes with the real value
//!   in-place.
//! - [`scan_for_violation`] — for placeholders whose `allow_hosts`
//!   does **not** include the destination, check if the placeholder
//!   appears at all. If yes, that's a leak attempt → caller closes.
//!
//! Both run on the request *as a whole* (we don't try to parse HTTP
//! headers vs body — the placeholder is unique enough that a literal
//! bytes-match is unambiguous). Microsandbox does parse, but their
//! parser exists for performance reasons we don't need yet.

use crate::registry::SecretEntry;

/// Substitute placeholders in `buf` whose secret allows `dest_host`.
/// Returns the modified buffer. If no substitution applies the
/// original buffer is returned unchanged (cheap; we still pay the
/// allocation for the Vec<u8> return).
pub fn substitute(buf: Vec<u8>, dest_host: &str, secrets: &[&SecretEntry]) -> Vec<u8> {
    let mut out = buf;
    for s in secrets {
        if !s.allow.matches(dest_host) {
            continue;
        }
        out = replace_all(&out, s.placeholder.as_bytes(), s.real_value.as_bytes());
    }
    out
}

/// Returns `Some(placeholder)` if any of `secrets`'s placeholders
/// appears in `buf` AND that secret does NOT allow `dest_host`. The
/// caller closes the connection on `Some`. Substitution-eligible
/// placeholders are skipped (they get rewritten by `substitute`, not
/// reported here).
pub fn scan_for_violation<'a>(
    buf: &[u8],
    dest_host: &str,
    secrets: &'a [&'a SecretEntry],
) -> Option<&'a str> {
    for s in secrets {
        if s.allow.matches(dest_host) {
            continue;
        }
        if memmem(buf, s.placeholder.as_bytes()) {
            return Some(s.placeholder.as_str());
        }
    }
    None
}

fn replace_all(buf: &[u8], needle: &[u8], replacement: &[u8]) -> Vec<u8> {
    if needle.is_empty() || buf.len() < needle.len() {
        return buf.to_vec();
    }
    let mut out = Vec::with_capacity(buf.len());
    let mut i = 0;
    while i + needle.len() <= buf.len() {
        if &buf[i..i + needle.len()] == needle {
            out.extend_from_slice(replacement);
            i += needle.len();
        } else {
            out.push(buf[i]);
            i += 1;
        }
    }
    out.extend_from_slice(&buf[i..]);
    out
}

fn memmem(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || haystack.len() < needle.len() {
        return false;
    }
    haystack.windows(needle.len()).any(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::HostList;

    fn entry(placeholder: &str, real: &str, allow: &[&str]) -> SecretEntry {
        SecretEntry {
            placeholder: placeholder.into(),
            real_value: real.into(),
            allow: HostList::from_manifest(
                &allow.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
                &[],
            )
            .unwrap(),
        }
    }

    #[test]
    fn substitutes_in_authorization_header() {
        let req =
            b"POST /v1/chat HTTP/1.1\r\nHost: api.openai.com\r\nAuthorization: Bearer engram_ph_abc\r\n\r\n".to_vec();
        let s = entry("engram_ph_abc", "sk-real", &["api.openai.com"]);
        let out = substitute(req, "api.openai.com", &[&s]);
        let s = std::str::from_utf8(&out).unwrap();
        assert!(s.contains("Authorization: Bearer sk-real"));
        assert!(!s.contains("engram_ph_abc"));
    }

    #[test]
    fn substitutes_in_body() {
        let req = b"POST / HTTP/1.1\r\n\r\n{\"key\":\"engram_ph_xxx\"}".to_vec();
        let s = entry("engram_ph_xxx", "real-value", &["api.example.com"]);
        let out = substitute(req, "api.example.com", &[&s]);
        let s = std::str::from_utf8(&out).unwrap();
        assert!(s.contains("\"key\":\"real-value\""));
    }

    #[test]
    fn substitutes_multiple_occurrences() {
        let req = b"a engram_ph_x b engram_ph_x c".to_vec();
        let s = entry("engram_ph_x", "Y", &["any.com"]);
        let out = substitute(req, "any.com", &[&s]);
        assert_eq!(out, b"a Y b Y c");
    }

    #[test]
    fn does_not_substitute_when_dest_host_disallowed() {
        let req = b"Authorization: Bearer engram_ph_abc".to_vec();
        let s = entry("engram_ph_abc", "sk-real", &["api.openai.com"]);
        // dest is api.evil.com, not in the secret's allow_hosts.
        let out = substitute(req.clone(), "api.evil.com", &[&s]);
        assert_eq!(out, req);
    }

    #[test]
    fn violation_when_placeholder_targets_disallowed_host() {
        let req = b"POST / HTTP/1.1\r\n\r\n{\"leak\":\"engram_ph_abc\"}";
        let s = entry("engram_ph_abc", "sk-real", &["api.openai.com"]);
        // Sending to attacker.com, but the placeholder is in the
        // body — flagged as violation.
        let secrets: Vec<&SecretEntry> = vec![&s];
        let v = scan_for_violation(req, "attacker.com", &secrets);
        assert_eq!(v, Some("engram_ph_abc"));
    }

    #[test]
    fn no_violation_when_dest_is_allowed() {
        let req = b"Authorization: Bearer engram_ph_abc";
        let s = entry("engram_ph_abc", "sk-real", &["api.openai.com"]);
        let secrets: Vec<&SecretEntry> = vec![&s];
        // Substitution will handle it; no violation reported.
        assert!(scan_for_violation(req, "api.openai.com", &secrets).is_none());
    }

    #[test]
    fn no_violation_when_placeholder_absent() {
        let req = b"POST / HTTP/1.1\r\n\r\n{}";
        let s = entry("engram_ph_abc", "sk-real", &["api.openai.com"]);
        let secrets: Vec<&SecretEntry> = vec![&s];
        assert!(scan_for_violation(req, "attacker.com", &secrets).is_none());
    }

    #[test]
    fn replace_all_handles_empty_needle() {
        // Defensive — empty needle should be a no-op rather than
        // panicking or looping forever.
        let out = replace_all(b"abc", b"", b"X");
        assert_eq!(out, b"abc");
    }
}
