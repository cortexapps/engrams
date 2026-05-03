//! Placeholder leak detection.
//!
//! After we decide to NOT substitute (host doesn't match the
//! secret's `allow_hosts`), we still need to guard against the
//! placeholder bytes literally appearing in the request — that's a
//! signal that the agent is trying to send the placeholder somewhere
//! the secret's policy doesn't allow. Microsandbox calls this a
//! "violation" and closes the connection. We do the same: if any of
//! a session's placeholders appears in a request body or header
//! AND the destination doesn't match that secret's allow_hosts, drop
//! the connection.
//!
//! This protects against two real footguns:
//!
//! - User code copies `$OPENAI_API_KEY` (the placeholder) into a log
//!   message that gets POSTed to a different vendor's "report bug"
//!   endpoint. The placeholder is meaningless, but the *attempt to
//!   send it elsewhere* is a strong signal the user is leaking
//!   credentials.
//! - User code embeds the placeholder in a hostname / URL it constructs
//!   programmatically.
//!
//! `scan(buf, placeholders)` returns the first placeholder that
//! appears in `buf`, or None.

/// Match a slice for any of the given placeholders. Linear search;
/// placeholders are short (~30 bytes) and there are few per session
/// (typically <10), so this is cheap. Returns the first match.
pub fn first_match<'a>(buf: &[u8], placeholders: &'a [&str]) -> Option<&'a str> {
    placeholders
        .iter()
        .find(|ph| memmem(buf, ph.as_bytes()))
        .copied()
}

/// `memchr::memmem` would be a better choice for production, but we
/// don't want a new dep just for placeholder scan. Buffers are
/// small (request body cap is enforced upstream); stdlib slice search
/// via `windows` is fine.
fn memmem(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || haystack.len() < needle.len() {
        return false;
    }
    haystack.windows(needle.len()).any(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_placeholder_in_body() {
        let body = b"POST /v1/chat HTTP/1.1\r\nAuthorization: Bearer engram_ph_abc_def\r\n\r\n";
        let placeholders = ["engram_ph_abc_def", "engram_ph_xxx_yyy"];
        let placeholders: Vec<&str> = placeholders.to_vec();
        assert_eq!(first_match(body, &placeholders), Some("engram_ph_abc_def"));
    }

    #[test]
    fn returns_none_when_no_match() {
        let body = b"GET /healthz HTTP/1.1\r\nHost: example.com\r\n\r\n";
        assert!(first_match(body, &["engram_ph_xxx_yyy"]).is_none());
    }

    #[test]
    fn empty_haystack_or_needle() {
        assert!(first_match(b"", &["engram_ph_xxx_yyy"]).is_none());
        // Empty placeholder list — nothing to match.
        let empty: Vec<&str> = Vec::new();
        assert!(first_match(b"some data", &empty).is_none());
    }
}
