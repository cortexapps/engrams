use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::ids::ImageVersionId;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ImageStatus {
    Building,
    Ready,
    Retired,
}

impl ImageStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Building => "building",
            Self::Ready => "ready",
            Self::Retired => "retired",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ImageVersion {
    pub id: ImageVersionId,
    pub repo: String,
    /// Tag of the form `warm-<timestamp>`.
    pub tag: String,
    pub blob_url: Option<String>,
    pub status: ImageStatus,
    pub created_at: DateTime<Utc>,
}

// ---------------------------------------------------------------------
// ImageManifest — the per-image declarative spec.
//
// Rendered from `<image_root>/manifest.toml`. Tells the coordinator
// what env to set, which secrets the image expects, what network it's
// allowed to reach, and what resources it wants. Identical schema for
// production (Firecracker rootfs.ext4) and dev (ProcessBackend
// directory) — only the rootfs format differs.
// ---------------------------------------------------------------------

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImageManifest {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,

    /// Non-secret environment variables applied to every sandbox
    /// spawned from this image. Session-supplied env wins on collision.
    #[serde(default)]
    pub env: HashMap<String, String>,

    /// Secrets this image expects. Session creation fails if a
    /// required secret is not provided by the SecretStore. Each
    /// secret's `allow_hosts` list constrains where the value may be
    /// substituted in `broker` mode (see SecretMode).
    #[serde(default)]
    pub secrets: HashMap<String, SecretSchema>,

    /// Defense-in-depth network policy. Today purely declarative;
    /// enforcement (egress filtering) lands with the Firecracker
    /// network namespace work in Phase 2.
    #[serde(default)]
    pub network: NetworkPolicy,

    /// Resource hints used as defaults when a session doesn't override.
    #[serde(default)]
    pub resources: ResourceHints,

    /// How secret values are surfaced inside the sandbox. See
    /// [`SecretMode`] for the security tradeoff.
    #[serde(default)]
    pub secret_mode: SecretMode,
}

/// How an image wants its resolved secret values delivered to the
/// guest. Per-image so the same coordinator can serve dev and prod
/// images side-by-side.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SecretMode {
    /// **Dev only.** Real secret values injected as plain env vars.
    /// Trivial to use; secrets are visible to anyone with code
    /// execution inside the sandbox. Acceptable on a developer's own
    /// laptop with their own credentials; never deploy.
    ///
    /// Default so dev images are usable out of the box; production
    /// images should set `secret_mode = "broker"`.
    #[default]
    Literal,
    /// **Production.** Random placeholders injected as env vars; a
    /// per-session network proxy substitutes the real value only on
    /// outbound requests to a secret's `allow_hosts`. The agent
    /// process never sees the real credential, so prompt-injection
    /// exfiltration attacks fail. Modeled on microsandbox's
    /// `Secret.env(..., allow_hosts=...)` design.
    Broker,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecretSchema {
    /// Hosts the secret may be substituted on. Used in Broker mode.
    /// Empty list = no destinations approved (secret value never
    /// substituted, only placeholder ever sent).
    #[serde(default)]
    pub allow_hosts: Vec<String>,

    /// Glob patterns matched against the request host. Useful for
    /// `*.githubusercontent.com`-style allowlists.
    #[serde(default)]
    pub allow_host_patterns: Vec<String>,

    /// Whether the session must have this secret available. Required
    /// secrets that aren't in the SecretStore at session-create time
    /// fail the request with a 400.
    #[serde(default = "yes")]
    pub required: bool,

    /// Free-form description for the secret-broker UI / audit log.
    #[serde(default)]
    pub description: Option<String>,

    /// Optional deployment-specific reference the configured
    /// `SecretStore` knows how to resolve. Opaque to the manifest —
    /// backends parse it as their format dictates (e.g.
    /// `gcp-sm://projects/cortex/secrets/github-token/versions/latest`,
    /// `vault://secret/data/cortex/github`,
    /// `k8s://namespace/secret-name/key`). When absent, the configured
    /// SecretStore uses its default namespacing (typically
    /// `<repo>/<secret_name>`).
    ///
    /// Including a `ref` couples the image to a particular deployment
    /// shape — useful when an image is built specifically for one
    /// environment, less useful for portable images. Default: omit
    /// and let the deployment decide.
    #[serde(default)]
    pub r#ref: Option<String>,
}

fn yes() -> bool {
    true
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NetworkDefault {
    Allow,
    /// Default-deny outbound: an image must opt in to anything it
    /// wants to reach. Mirrors zero-trust posture.
    #[default]
    Deny,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkPolicy {
    #[serde(default)]
    pub default: NetworkDefault,
    #[serde(default)]
    pub allow_hosts: Vec<String>,
    #[serde(default)]
    pub allow_host_patterns: Vec<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceHints {
    pub suggested_memory_mib: Option<u32>,
    pub suggested_vcpus: Option<u32>,
    pub suggested_disk_gib: Option<u32>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_parses_minimal_toml() {
        let src = r#"
            name = "cortex-api"
        "#;
        let m: ImageManifest = toml::from_str(src).unwrap();
        assert_eq!(m.name, "cortex-api");
        assert!(m.env.is_empty());
        assert!(m.secrets.is_empty());
        assert_eq!(m.secret_mode, SecretMode::Literal);
        assert_eq!(m.network.default, NetworkDefault::Deny);
    }

    #[test]
    fn manifest_parses_full_toml_with_secrets_and_network() {
        let src = r#"
            name = "cortex-api"
            description = "Backend API service"
            secret_mode = "broker"

            [env]
            PYTHONUNBUFFERED = "1"

            [secrets.GITHUB_TOKEN]
            allow_hosts = ["api.github.com"]
            allow_host_patterns = ["*.githubusercontent.com"]
            description = "Read+write to org cortex/"

            [secrets.OPENAI_API_KEY]
            allow_hosts = ["api.openai.com"]
            required = false

            [network]
            default = "deny"
            allow_hosts = ["api.github.com", "registry.npmjs.org"]
            allow_host_patterns = ["*.pypi.org"]

            [resources]
            suggested_memory_mib = 4096
            suggested_vcpus = 2
        "#;
        let m: ImageManifest = toml::from_str(src).unwrap();
        assert_eq!(m.name, "cortex-api");
        assert_eq!(m.description.as_deref(), Some("Backend API service"));
        assert_eq!(m.secret_mode, SecretMode::Broker);

        let gh = m.secrets.get("GITHUB_TOKEN").unwrap();
        assert_eq!(gh.allow_hosts, vec!["api.github.com"]);
        assert_eq!(gh.allow_host_patterns, vec!["*.githubusercontent.com"]);
        assert!(gh.required, "secrets default to required");

        let openai = m.secrets.get("OPENAI_API_KEY").unwrap();
        assert!(!openai.required, "explicit required=false honored");

        assert_eq!(m.network.allow_hosts.len(), 2);
        assert_eq!(m.resources.suggested_memory_mib, Some(4096));
    }

    #[test]
    fn manifest_rejects_harness_block() {
        // `[[harness]]` was removed when harness binaries moved to a
        // host-side directory mounted into every sandbox via
        // virtio-fs. `deny_unknown_fields` on `ImageManifest` makes
        // a stale engram.toml carrying `[[harness]]` fail-fast at
        // parse rather than silently shipping an inert image.
        let src = r#"
            name = "cortex-api"

            [[harness]]
            name = "claude"
            guest_path = "/sbin/engram-harness-claude"
        "#;
        let res: Result<ImageManifest, _> = toml::from_str(src);
        assert!(res.is_err(), "stale [[harness]] block must be rejected");
    }

    #[test]
    fn manifest_rejects_unknown_top_level_keys() {
        // deny_unknown_fields catches typos like `enviroment` or
        // forgotten config sections — surfaces them at parse time
        // rather than silently dropping config.
        let src = r#"
            name = "cortex-api"
            enviroment = { FOO = "bar" }
        "#;
        let res: Result<ImageManifest, _> = toml::from_str(src);
        assert!(res.is_err(), "typo `enviroment` must be rejected");
    }

    #[test]
    fn secret_mode_round_trips_through_serde() {
        for mode in [SecretMode::Literal, SecretMode::Broker] {
            let json = serde_json::to_string(&mode).unwrap();
            let back: SecretMode = serde_json::from_str(&json).unwrap();
            assert_eq!(mode, back);
        }
    }
}
