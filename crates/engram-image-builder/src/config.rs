//! `engram.toml` — the source-side per-repo config the baker reads.
//!
//! Carries everything the rendered `manifest.toml` carries (env,
//! secrets schema, network policy, resources, secret_mode), plus a
//! `[build]` section the runtime doesn't need (Dockerfile path,
//! context, build args, build-time secrets).
//!
//! ImageManifest in `engram-core` has `deny_unknown_fields` for tight
//! validation. We can't `#[serde(flatten)]` it here because that
//! breaks unknown-field detection. Instead we read the raw TOML once,
//! pop the `[build]` table out, and parse the remainder as
//! ImageManifest separately. Both halves get strict validation; no
//! field duplication; ImageManifest stays the single source of truth
//! for runtime fields.

use std::collections::HashMap;
use std::path::Path;

use engram_core::types::ImageManifest;
use serde::{Deserialize, Serialize};

use crate::BuildError;

/// What `engram.toml` looks like to the baker after parsing. The
/// `manifest` portion is what gets written to `<image_dir>/manifest.toml`;
/// `build` is consumed by the baker and discarded.
#[derive(Clone, Debug)]
pub struct EngramRepoConfig {
    pub manifest: ImageManifest,
    pub build: BuildConfig,
}

/// Source-only build directives.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BuildConfig {
    /// Path to the Dockerfile, relative to the source root. Default `Dockerfile`.
    #[serde(default = "default_dockerfile")]
    pub dockerfile: String,

    /// Build context, relative to the source root. Default `.`.
    #[serde(default = "default_context")]
    pub context: String,

    /// `docker build --build-arg KEY=VAL` pairs. Useful for parameterising
    /// the Dockerfile (release vs debug, language version, etc.).
    #[serde(default)]
    pub args: HashMap<String, String>,

    /// Names of secrets to pass to the build via BuildKit's
    /// `--secret id=...` mechanism. Looked up via the configured
    /// SecretStore at bake time. Note: these are *build-time* secrets
    /// (e.g. private NPM tokens needed during `pnpm install`); they
    /// are NOT the same as runtime `[secrets]` (which the agent uses
    /// at runtime). Build-time secrets stay in the build cache, never
    /// in the produced image.
    #[serde(default)]
    pub build_secrets: Vec<String>,
}

impl Default for BuildConfig {
    fn default() -> Self {
        // Manual impl rather than #[derive(Default)] — the serde
        // field-level `default = "..."` callbacks only fire on
        // deserialization, not when std `::default()` is called.
        // This is the case where the source `engram.toml` omits the
        // entire `[build]` section.
        Self {
            dockerfile: default_dockerfile(),
            context: default_context(),
            args: HashMap::new(),
            build_secrets: Vec::new(),
        }
    }
}

fn default_dockerfile() -> String {
    "Dockerfile".into()
}

fn default_context() -> String {
    ".".into()
}

impl EngramRepoConfig {
    pub fn read_from(path: &Path) -> Result<Self, BuildError> {
        let bytes = std::fs::read(path)
            .map_err(|e| BuildError::Config(format!("read {}: {e}", path.display())))?;
        let text = std::str::from_utf8(&bytes)
            .map_err(|e| BuildError::Config(format!("{} is not UTF-8: {e}", path.display())))?;
        Self::parse(text)
    }

    /// Parse the raw TOML. Splits `[build]` from the manifest fields
    /// without duplicating ImageManifest's schema.
    pub fn parse(toml_str: &str) -> Result<Self, BuildError> {
        let mut value: toml::Value =
            toml::from_str(toml_str).map_err(|e| BuildError::Config(format!("parse: {e}")))?;
        let build = match value.as_table_mut().and_then(|t| t.remove("build")) {
            Some(b) => b
                .try_into::<BuildConfig>()
                .map_err(|e| BuildError::Config(format!("[build]: {e}")))?,
            None => BuildConfig::default(),
        };
        let manifest = value
            .try_into::<ImageManifest>()
            .map_err(|e| BuildError::Config(format!("manifest fields: {e}")))?;
        Ok(Self { manifest, build })
    }

    /// Return the rendered manifest (everything but `[build]`). This
    /// is what `<image_dir>/manifest.toml` contains.
    pub fn to_manifest(&self) -> ImageManifest {
        self.manifest.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_minimal_engram_toml() {
        let cfg = EngramRepoConfig::parse(r#"name = "x""#).unwrap();
        assert_eq!(cfg.manifest.name, "x");
        assert_eq!(cfg.build.dockerfile, "Dockerfile");
        assert_eq!(cfg.build.context, ".");
        assert!(cfg.build.args.is_empty());
    }

    #[test]
    fn parse_with_build_section() {
        let cfg = EngramRepoConfig::parse(
            r#"
            name = "cortex-api"

            [build]
            dockerfile = "deploy/Dockerfile.runtime"
            context = "."
            args = { BUILD_FLAVOR = "release" }
            build_secrets = ["NPM_TOKEN"]
            "#,
        )
        .unwrap();
        assert_eq!(cfg.build.dockerfile, "deploy/Dockerfile.runtime");
        assert_eq!(
            cfg.build.args.get("BUILD_FLAVOR").map(String::as_str),
            Some("release"),
        );
        assert_eq!(cfg.build.build_secrets, vec!["NPM_TOKEN"]);
    }

    #[test]
    fn parse_with_full_manifest_fields_and_build_section() {
        // Combined config: every manifest field + a [build] section.
        // Locks down that the split logic doesn't drop any manifest
        // field on the way through.
        let cfg = EngramRepoConfig::parse(
            r#"
            name = "cortex-api"
            description = "API service"
            secret_mode = "broker"

            [env]
            NODE_ENV = "production"

            [secrets.GITHUB_TOKEN]
            allow_hosts = ["api.github.com"]
            required = true

            [network]
            default = "deny"
            allow_hosts = ["api.github.com"]

            [resources]
            suggested_memory_mib = 4096

            [build]
            dockerfile = "Dockerfile"
            "#,
        )
        .unwrap();
        assert_eq!(cfg.manifest.description.as_deref(), Some("API service"));
        assert_eq!(
            cfg.manifest.secret_mode,
            engram_core::types::SecretMode::Broker
        );
        assert_eq!(
            cfg.manifest.env.get("NODE_ENV").map(String::as_str),
            Some("production")
        );
        assert!(cfg.manifest.secrets.contains_key("GITHUB_TOKEN"));
        assert_eq!(cfg.manifest.network.allow_hosts, vec!["api.github.com"]);
        assert_eq!(cfg.manifest.resources.suggested_memory_mib, Some(4096));
    }

    #[test]
    fn parse_rejects_unknown_field_in_build_section() {
        // deny_unknown_fields on BuildConfig catches typos.
        let res = EngramRepoConfig::parse(
            r#"
            name = "x"
            [build]
            dockerfile_typo = "Dockerfile"
            "#,
        );
        assert!(res.is_err(), "unknown build field must be rejected");
    }

    #[test]
    fn parse_rejects_unknown_field_in_manifest_section() {
        // ImageManifest's deny_unknown_fields still catches manifest
        // typos because we round-trip through it.
        let res = EngramRepoConfig::parse(
            r#"
            name = "x"
            enviroment = { FOO = "bar" }
            "#,
        );
        assert!(res.is_err(), "unknown manifest field must be rejected");
    }

    #[test]
    fn missing_build_section_uses_defaults() {
        let cfg = EngramRepoConfig::parse(r#"name = "x""#).unwrap();
        assert_eq!(cfg.build.dockerfile, "Dockerfile");
        assert_eq!(cfg.build.context, ".");
        assert!(cfg.build.build_secrets.is_empty());
    }

    #[test]
    fn parse_engram_toml_rejects_stale_harness_block() {
        // `[[harness]]` was retired when harness binaries moved to a
        // host-side directory mounted into every sandbox via
        // virtio-fs. ImageManifest's `deny_unknown_fields` catches
        // a stale engram.toml that still carries the block, failing
        // at parse rather than silently shipping an inert image.
        let res = EngramRepoConfig::parse(
            r#"
            name = "claude-oauth"

            [[harness]]
            name = "claude"
            guest_path = "/sbin/engram-harness-claude"
            "#,
        );
        assert!(res.is_err(), "stale [[harness]] block must be rejected");
    }

    #[test]
    fn rendered_manifest_does_not_include_build_section() {
        let cfg = EngramRepoConfig::parse(
            r#"
            name = "x"
            [build]
            dockerfile = "Dockerfile.alt"
            "#,
        )
        .unwrap();
        let rendered = toml::to_string(&cfg.to_manifest()).unwrap();
        assert!(
            !rendered.contains("[build]"),
            "rendered manifest must not carry the source-only [build] section",
        );
        assert!(
            !rendered.contains("Dockerfile.alt"),
            "rendered manifest must not carry build directives",
        );
    }
}
