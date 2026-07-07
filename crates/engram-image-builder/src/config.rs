//! `engram.toml` — the source-side per-repo BUILD config the baker reads.
//!
//! ADR 0080: this file now carries ONLY the `[build]` section (Dockerfile
//! path, context, build args, build-time secrets) and is OPTIONAL — a repo
//! whose Dockerfile sits at the default path needs no engram.toml at all.
//! Everything the runtime used to read from here (name/description/env/
//! workdir/resources/warm) is supplied out-of-band at enable time via
//! `engram image enable --config <image-config.toml>` (the ImageService
//! RPC) and never rides the baked artifact. A leftover manifest-era key at
//! the top level fails the bake loudly with a pointer to the new home.

use std::collections::HashMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::BuildError;

/// What `engram.toml` looks like to the baker after parsing: just the
/// `[build]` directives (defaults when the file or section is absent).
#[derive(Clone, Debug, Default)]
pub struct EngramRepoConfig {
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
    /// are NOT the same as runtime secrets (which are session policy).
    /// Build-time secrets stay in the build cache, never in the
    /// produced image.
    #[serde(default)]
    pub build_secrets: Vec<String>,
}

impl Default for BuildConfig {
    fn default() -> Self {
        // Manual impl rather than #[derive(Default)] — the serde
        // field-level `default = "..."` callbacks only fire on
        // deserialization, not when std `::default()` is called.
        // This is the case where the source `engram.toml` omits the
        // entire `[build]` section (or doesn't exist at all).
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
    /// Read `engram.toml` if present; a missing file is the all-defaults
    /// config (ADR 0080: a plain-Dockerfile repo needs no engram.toml).
    pub fn read_from(path: &Path) -> Result<Self, BuildError> {
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(BuildError::Config(format!("read {}: {e}", path.display()))),
        };
        let text = std::str::from_utf8(&bytes)
            .map_err(|e| BuildError::Config(format!("{} is not UTF-8: {e}", path.display())))?;
        Self::parse(text)
    }

    /// Parse the raw TOML. ADR 0080: only `[build]` is legal — any other
    /// top-level key is a retired manifest-era field and fails loudly,
    /// pointing the author at the enable-time config that replaced it.
    pub fn parse(toml_str: &str) -> Result<Self, BuildError> {
        let mut value: toml::Value =
            toml::from_str(toml_str).map_err(|e| BuildError::Config(format!("parse: {e}")))?;
        let build = match value.as_table_mut().and_then(|t| t.remove("build")) {
            Some(b) => b
                .try_into::<BuildConfig>()
                .map_err(|e| BuildError::Config(format!("[build]: {e}")))?,
            None => BuildConfig::default(),
        };
        if let Some(table) = value.as_table() {
            if let Some(key) = table.keys().next() {
                return Err(BuildError::Config(format!(
                    "engram.toml key `{key}` is retired (ADR 0080): the bake carries no \
                     runtime config anymore. Move name/description/env/workdir/resources/warm \
                     into an image-config TOML and apply it at enable time with \
                     `engram image enable --uri <uri> --config <image-config.toml>`. \
                     engram.toml now holds only the [build] section (and is optional)."
                )));
            }
        }
        Ok(Self { build })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_empty_engram_toml_uses_defaults() {
        let cfg = EngramRepoConfig::parse("").unwrap();
        assert_eq!(cfg.build.dockerfile, "Dockerfile");
        assert_eq!(cfg.build.context, ".");
        assert!(cfg.build.args.is_empty());
        assert!(cfg.build.build_secrets.is_empty());
    }

    #[test]
    fn parse_with_build_section() {
        let cfg = EngramRepoConfig::parse(
            r#"
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
    fn parse_rejects_unknown_field_in_build_section() {
        // deny_unknown_fields on BuildConfig catches typos.
        let res = EngramRepoConfig::parse(
            r#"
            [build]
            dockerfile_typo = "Dockerfile"
            "#,
        );
        assert!(res.is_err(), "unknown build field must be rejected");
    }

    /// ADR 0080: a manifest-era key (name/env/warm/…) at the top level is
    /// a stale config that would silently do nothing — fail the bake with
    /// a pointer to `image enable --config` instead.
    #[test]
    fn parse_rejects_retired_manifest_keys_with_migration_pointer() {
        for src in [
            "name = \"x\"\n",
            "[env]\nNODE_ENV = \"production\"\n",
            "[resources]\nsuggested_vcpus = 4\n",
            "[warm]\ncommand = [\"true\"]\n",
        ] {
            let err = format!("{:?}", EngramRepoConfig::parse(src).unwrap_err());
            assert!(
                err.contains("image enable") && err.contains("ADR 0080"),
                "must point at the enable-time config: {src} → {err}"
            );
        }
    }

    #[test]
    fn missing_file_is_all_defaults() {
        let cfg =
            EngramRepoConfig::read_from(Path::new("/nonexistent/engram.toml")).expect("optional");
        assert_eq!(cfg.build.dockerfile, "Dockerfile");
    }
}
