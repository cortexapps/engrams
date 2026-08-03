//! Google credential-minting surfaces the guest never reaches.
//!
//! ADR 0109 hands a session a short-lived, policy-bound Google access token.
//! The whole boundary rests on the guest being unable to trade that token for
//! something the proxy no longer bounds — a longer-lived token, a downscoped
//! token for another service account, or an exportable service-account key. The
//! surfaces that do that are listed in `policy/google-credential-denylist.json`,
//! which the orchestrator imports as well, so the two enforcement points cannot
//! drift apart again.
//!
//! Two kinds of rule, evaluated at two different moments:
//!
//! - **A denied host** is refused at admission ([`crate::registry::SessionState::decide`]),
//!   before any TLS. That is what covers a **bypass** connection: a broad
//!   `*.googleapis.com` network allow used to splice STS straight through,
//!   because the request-level check only ran on intercepted connections.
//! - **A denied operation** needs the request line, so it runs inside the
//!   intercept path. A host that carries such a rule is therefore never
//!   bypassed — [`requires_inspection`] upgrades it to an intercept so the rule
//!   can actually run.

use std::sync::OnceLock;

use serde::Deserialize;

/// The checked-in table. Shared verbatim with the orchestrator's endpoint
/// validator (see `docker/orchestrator.Dockerfile`, which copies this file into
/// the runtime image at the same relative path).
const DENYLIST_JSON: &str = include_str!("../policy/google-credential-denylist.json");

#[derive(Debug, Deserialize)]
struct Denylist {
    denied_hosts: Vec<String>,
    denied_operations: Vec<OperationRule>,
}

#[derive(Debug, Deserialize)]
struct OperationRule {
    host: String,
    #[serde(default)]
    methods: Vec<String>,
    #[serde(default)]
    path_prefixes: Vec<String>,
    #[serde(default)]
    path_contains: Vec<String>,
    #[serde(default)]
    path_suffixes: Vec<String>,
}

impl OperationRule {
    /// Every matcher family that the rule lists must hit. An omitted family is
    /// not a constraint, so a rule with only `path_suffixes` ignores the method.
    fn matches(&self, method: &str, path: &str) -> bool {
        if !self.methods.is_empty()
            && !self
                .methods
                .iter()
                .any(|allowed| allowed.eq_ignore_ascii_case(method))
        {
            return false;
        }
        let family = |patterns: &[String], hit: &dyn Fn(&str) -> bool| {
            patterns.is_empty() || patterns.iter().any(|pattern| hit(pattern))
        };
        family(&self.path_prefixes, &|pattern| path.starts_with(pattern))
            && family(&self.path_contains, &|pattern| path.contains(pattern))
            && family(&self.path_suffixes, &|pattern| path.ends_with(pattern))
    }
}

fn table() -> &'static Denylist {
    static TABLE: OnceLock<Denylist> = OnceLock::new();
    TABLE.get_or_init(|| {
        serde_json::from_str(DENYLIST_JSON)
            .expect("policy/google-credential-denylist.json is malformed")
    })
}

/// Normalize a host for matching: lower case, no trailing dot, and no `.mtls`
/// label. Google serves every credential endpoint at a mutual-TLS twin
/// (`sts.mtls.googleapis.com`), and an exact-host list that omits the twin is
/// a complete bypass of the list.
fn normalize_host(host: &str) -> String {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    match host.split_once(".mtls.") {
        Some((head, tail)) => format!("{head}.{tail}"),
        None => host,
    }
}

fn host_matches(pattern: &str, host: &str) -> bool {
    match pattern.strip_prefix("*.") {
        Some(suffix) => host.ends_with(suffix) && host.len() > suffix.len(),
        None => host == pattern,
    }
}

/// Is this host a credential-exchange surface? Refused at admission, so a
/// bypass connection cannot reach it either.
pub fn denies_host(sni: &str) -> bool {
    let host = normalize_host(sni);
    table()
        .denied_hosts
        .iter()
        .any(|pattern| host_matches(pattern, &host))
}

/// Does this host carry an operation rule that names it outright? Such a host
/// must be intercepted rather than spliced, or the rule never runs.
///
/// Wildcard rules are deliberately excluded here. Their verbs live on hosts
/// that [`denies_host`] already refuses; treating the wildcard as a reason to
/// intercept would put every `*.googleapis.com` host — object downloads
/// included — through TLS termination for no added protection.
pub fn requires_inspection(sni: &str) -> bool {
    let host = normalize_host(sni);
    table()
        .denied_operations
        .iter()
        .any(|rule| !rule.host.starts_with("*.") && host_matches(&rule.host, &host))
}

/// Does this request reach a credential-minting operation? The path is compared
/// with the query dropped and percent escapes decoded.
pub fn denies_operation(sni: &str, method: &str, path: &str) -> bool {
    let host = normalize_host(sni);
    let path = normalized_operation_path(path);
    table()
        .denied_operations
        .iter()
        .any(|rule| host_matches(&rule.host, &host) && rule.matches(method, &path))
}

/// Request headers that ask an upstream to treat the request as another method.
/// Google honours the first of these, so a guest could send `POST` — the verb
/// our policy gate reads — and have the upstream run `DELETE`. They are
/// stripped from every intercepted request, which makes the request line the
/// only statement of intent.
pub const METHOD_OVERRIDE_HEADERS: [&str; 3] = [
    "x-http-method-override",
    "x-method-override",
    "x-http-method",
];

pub fn is_method_override_header(name: &[u8]) -> bool {
    METHOD_OVERRIDE_HEADERS
        .iter()
        .any(|header| name.eq_ignore_ascii_case(header.as_bytes()))
}

/// Drop the query and decode percent escapes, then lower-case. A Google
/// frontend decodes escapes while it routes a transcoded API method, so the
/// check has to see what the router will see — including a doubly-encoded
/// `%253AgenerateAccessToken`.
fn normalized_operation_path(path: &str) -> String {
    let mut current = path.split('?').next().unwrap_or(path).as_bytes().to_vec();
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn the_table_parses_and_stays_populated() {
        let table = table();
        // A rule count that drops to zero, or a rule that lost its host, would
        // silently open the whole surface.
        assert!(table.denied_hosts.len() >= 5);
        assert!(table.denied_operations.len() >= 6);
        for rule in &table.denied_operations {
            assert!(!rule.host.is_empty(), "every rule names a host");
            assert!(
                !rule.path_prefixes.is_empty()
                    || !rule.path_contains.is_empty()
                    || !rule.path_suffixes.is_empty(),
                "a rule with no path matcher would deny the whole host: {}",
                rule.host,
            );
            for pattern in rule
                .path_prefixes
                .iter()
                .chain(&rule.path_contains)
                .chain(&rule.path_suffixes)
            {
                assert_eq!(
                    *pattern,
                    pattern.to_ascii_lowercase(),
                    "paths are compared in lower case",
                );
            }
        }
        // Each denied host is named once.
        let unique: BTreeSet<&String> = table.denied_hosts.iter().collect();
        assert_eq!(unique.len(), table.denied_hosts.len());
    }

    #[test]
    fn denies_every_credential_exchange_host() {
        for host in [
            "sts.googleapis.com",
            "oauth2.googleapis.com",
            "accounts.google.com",
            "securetoken.googleapis.com",
            "iamcredentials.googleapis.com",
        ] {
            assert!(denies_host(host), "{host}");
            // The mutual-TLS twin is the same surface.
            let mtls = host.replacen('.', ".mtls.", 1);
            assert!(denies_host(&mtls), "{mtls}");
            // A trailing dot is the same name.
            assert!(denies_host(&format!("{host}.")), "{host}.");
            assert!(denies_host(&host.to_ascii_uppercase()), "{host} upper");
        }
        assert!(!denies_host("compute.googleapis.com"));
        assert!(!denies_host("storage.googleapis.com"));
        // A suffix wildcard must not match the bare suffix itself.
        assert!(!denies_host("googleapis.com"));
    }

    #[test]
    fn denies_credential_operations_over_rest_and_grpc() {
        for operation in [
            ":generateAccessToken",
            ":generateIdToken",
            ":signBlob",
            ":signJwt",
        ] {
            assert!(denies_operation(
                "iam.googleapis.com",
                "POST",
                &format!("/v1/projects/-/serviceAccounts/account@example.com{operation}"),
            ));
        }
        // gRPC carries the service and method in the path, which no REST shape
        // above matches.
        for method_path in [
            "/google.iam.admin.v1.IAM/CreateServiceAccountKey",
            "/google.iam.admin.v1.IAM/UploadServiceAccountKey",
            "/google.iam.credentials.v1.IAMCredentials/GenerateAccessToken",
            "/google.identity.sts.v1.SecurityTokenService/ExchangeToken",
        ] {
            assert!(
                denies_operation("iam.googleapis.com", "POST", method_path),
                "{method_path}",
            );
        }
        assert!(denies_operation(
            "iam.googleapis.com",
            "POST",
            "/v1/projects/p/serviceAccounts/account@example.com/keys",
        ));
        assert!(denies_operation(
            "identitytoolkit.googleapis.com",
            "POST",
            "/v1/accounts:signInWithCustomToken",
        ));
        assert!(denies_operation(
            "www.googleapis.com",
            "GET",
            "/oauth2/v4/token",
        ));
        // A doubly-encoded escape still reaches the router as the real verb.
        assert!(denies_operation(
            "compute.googleapis.com",
            "POST",
            "/v1/projects/p/serviceAccounts/a%253AgenerateAccessToken",
        ));
        // The mutual-TLS twin of an operation-gated host is the same surface.
        assert!(denies_operation(
            "iam.mtls.googleapis.com",
            "POST",
            "/v1/projects/p/serviceAccounts/a@example.com/keys",
        ));
        // A query string never hides the operation, and never invents one.
        assert!(denies_operation(
            "iam.googleapis.com",
            "POST",
            "/v1/projects/p/serviceAccounts/a@example.com/keys?alt=json",
        ));
        assert!(!denies_operation(
            "compute.googleapis.com",
            "POST",
            "/compute/v1/projects/p/zones/z/instances/i/start",
        ));
    }

    #[test]
    fn only_operation_gated_hosts_force_inspection() {
        assert!(requires_inspection("iam.googleapis.com"));
        assert!(requires_inspection("iam.mtls.googleapis.com"));
        assert!(requires_inspection("www.googleapis.com"));
        assert!(requires_inspection("identitytoolkit.googleapis.com"));
        // A wildcard rule is not a reason to terminate TLS on a host whose real
        // credential surfaces are already refused outright.
        assert!(!requires_inspection("storage.googleapis.com"));
        assert!(!requires_inspection("compute.googleapis.com"));
    }

    #[test]
    fn recognises_every_method_override_header() {
        assert!(is_method_override_header(b"X-HTTP-Method-Override"));
        assert!(is_method_override_header(b"x-method-override"));
        assert!(is_method_override_header(b"X-Http-Method"));
        assert!(!is_method_override_header(b"x-request-id"));
    }
}
