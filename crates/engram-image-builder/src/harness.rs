//! Built-in harness injection (ADR 0021 P0).
//!
//! When an image's `engram.toml` carries a built-in `[harness]` block
//! — `builtin = "claude"` + `version = "v1.2.3"` — the baker resolves
//! `(name, version, target platform)` to a GHCR OCI artifact via the
//! [`BuiltinCatalog`], pulls the artifact's single tar.gz layer with
//! the existing [`engram_oci::OciClient`], and extracts it directly
//! into the rootfs at the canonical path `/opt/engram/harness/`.
//!
//! The artifact tarball contains exactly the contents of that
//! directory — typically a `harness` binary, optional bundled runtime
//! sidecars (e.g. the `claude` CLI), and a small `artifact.toml`
//! describing how to launch it. The baker reads that file post-extract
//! and renders the resolved [`HarnessManifest`] (name, exec, args, and
//! a `version` field carrying both the requested tag and the resolved
//! OCI digest) into the image's `manifest.toml`.
//!
//! Custom harnesses are out of scope here: they ship via the author's
//! own Dockerfile `COPY`, and the baker only validates that the
//! declared `exec` path exists on the rootfs.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use engram_core::types::{HarnessManifest, ImageManifest};
use engram_oci::OciClient;
use serde::Deserialize;

use crate::BuildError;

/// Rootfs-relative path the baker plants the harness tree at.
/// No leading slash so it composes with [`Path::join`]; pair with
/// [`CANONICAL_HARNESS_DIR_ABS`] for the in-guest absolute view.
pub const CANONICAL_HARNESS_DIR_REL: &str = "opt/engram/harness";

/// Absolute path the harness lives at as seen from inside the
/// running VM. The author's launch contract (`exec`) is rendered
/// relative to this root, e.g. `/opt/engram/harness/harness`.
pub const CANONICAL_HARNESS_DIR_ABS: &str = "/opt/engram/harness";

/// Filename of the per-artifact descriptor the baker reads after
/// extraction. Lives at the *root* of the extracted tree (i.e.
/// `<rootfs>/opt/engram/harness/artifact.toml`).
pub const ARTIFACT_DESCRIPTOR: &str = "artifact.toml";

/// Target platform for a built-in harness artifact. Today only
/// `linux/x86_64` ships; arm64 is a follow-up the catalog will gain
/// once we publish those tags. The variant maps to the tag suffix
/// used in the OCI reference (e.g. `:v1.2.3-linux-x86_64`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Platform {
    LinuxX86_64,
}

impl Platform {
    /// Tag suffix appended after the version, joined with `-`.
    pub fn tag_suffix(self) -> &'static str {
        match self {
            Platform::LinuxX86_64 => "linux-x86_64",
        }
    }
}

/// One curated harness in the [`BuiltinCatalog`]. Today the entry is
/// just the GHCR repo — version + platform compose the full tag.
#[derive(Clone, Debug)]
pub struct BuiltinEntry {
    /// GHCR repo, e.g. `"ghcr.io/cortexapps/engrams/harness-claude"`.
    pub repo: &'static str,
}

/// Curated `(name) -> repo` map for built-in harnesses. Hardcoded in
/// the baker rather than in `engram-cli` so the CLI stays a thin
/// front-end and the builder owns "how to bake" end-to-end.
#[derive(Clone, Debug)]
pub struct BuiltinCatalog {
    entries: HashMap<&'static str, BuiltinEntry>,
}

impl BuiltinCatalog {
    /// The default catalog engram ships with. Add a new entry here
    /// when a new built-in harness goes live in CI.
    pub fn default_catalog() -> Self {
        let mut entries = HashMap::new();
        entries.insert(
            "claude",
            BuiltinEntry {
                repo: "ghcr.io/cortexapps/engrams/harness-claude",
            },
        );
        Self { entries }
    }

    /// Resolve `(name, version, platform)` to the full OCI reference
    /// the baker will pull from. Errors if the name isn't curated —
    /// the validator already enforced shape, so the only way to land
    /// here is a typo in `builtin = "..."`.
    pub fn resolve(
        &self,
        name: &str,
        version: &str,
        platform: Platform,
    ) -> Result<String, BuildError> {
        let entry = self.entries.get(name).ok_or_else(|| {
            BuildError::Config(format!(
                "[harness] builtin {name:?} is not a known built-in harness (known: {})",
                self.known_names().copied().collect::<Vec<_>>().join(", ")
            ))
        })?;
        Ok(format!(
            "{}:{}-{}",
            entry.repo,
            version,
            platform.tag_suffix()
        ))
    }

    /// Names of the curated built-ins, for error messages and CLI
    /// listings.
    pub fn known_names(&self) -> impl Iterator<Item = &&'static str> {
        self.entries.keys()
    }
}

/// The per-artifact descriptor shipped *inside* the layer at
/// `<extracted>/artifact.toml`. Tells the baker how to launch the
/// harness and lets it cross-check what it pulled against what the
/// author asked for.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactToml {
    /// Curated name, must match the `builtin = "..."` in `engram.toml`.
    pub name: String,
    /// Semver tag, must match the `version = "..."` in `engram.toml`.
    pub version: String,
    /// Path to the entry-point binary, relative to
    /// [`CANONICAL_HARNESS_DIR_ABS`]. Typically just `"harness"`.
    pub entry: String,
    /// Extra argv appended after the SDK-standard flags.
    #[serde(default)]
    pub args: Vec<String>,
    /// Free-form description for surfaces that list curated harnesses.
    #[serde(default)]
    pub description: Option<String>,
}

/// Resolve the source-side built-in `[harness]` block on `manifest`,
/// pull the artifact, extract into the rootfs, and return the
/// *resolved* [`HarnessManifest`] (with `name`/`exec`/`args` filled
/// and `version` carrying `"<tag>@<sha256-digest>"`). Caller writes
/// this back into `manifest.harness` before rendering `manifest.toml`.
///
/// Pre-conditions: `manifest.harness` is `Some` with a non-empty
/// `builtin` and `version`. The baker checked this via
/// [`HarnessManifest::validate_source`] when it parsed `engram.toml`.
pub async fn inject_builtin_harness(
    rootfs_dir: &Path,
    oci: &OciClient,
    catalog: &BuiltinCatalog,
    source: &HarnessManifest,
    platform: Platform,
) -> Result<HarnessManifest, BuildError> {
    let builtin = source
        .builtin
        .as_deref()
        .ok_or_else(|| BuildError::Config("inject_builtin_harness: missing builtin".into()))?;
    let version = source
        .version
        .as_deref()
        .ok_or_else(|| BuildError::Config("inject_builtin_harness: missing version".into()))?;

    let uri = catalog.resolve(builtin, version, platform)?;

    // Extract straight into the rootfs subdir — the layer is shaped
    // as exactly the contents of /opt/engram/harness/.
    let dest = rootfs_dir.join(CANONICAL_HARNESS_DIR_REL);
    tokio::fs::create_dir_all(&dest).await?;

    let pulled = oci
        .pull_harness(&uri, &dest)
        .await
        .map_err(|e| BuildError::Config(format!("pull built-in harness {uri}: {e}")))?;

    let descriptor_path = dest.join(ARTIFACT_DESCRIPTOR);
    let raw = tokio::fs::read_to_string(&descriptor_path)
        .await
        .map_err(|e| {
            BuildError::Config(format!(
                "built-in harness {uri} missing {ARTIFACT_DESCRIPTOR}: {e}"
            ))
        })?;
    let art: ArtifactToml = toml::from_str(&raw)
        .map_err(|e| BuildError::Config(format!("{descriptor_path:?}: {e}")))?;

    if art.name != builtin {
        return Err(BuildError::Config(format!(
            "built-in artifact name mismatch: requested {builtin:?}, artifact says {:?}",
            art.name
        )));
    }
    if art.version != version {
        return Err(BuildError::Config(format!(
            "built-in artifact version mismatch: requested {version:?}, artifact says {:?}",
            art.version
        )));
    }
    if art.entry.starts_with('/') || art.entry.contains("..") {
        return Err(BuildError::Config(format!(
            "{ARTIFACT_DESCRIPTOR}: `entry` must be a relative path inside the harness dir (got {:?})",
            art.entry
        )));
    }

    // Sanity-check the entry-point actually shipped in the layer.
    let entry_on_disk: PathBuf = dest.join(&art.entry);
    if !tokio::fs::try_exists(&entry_on_disk).await.unwrap_or(false) {
        return Err(BuildError::Config(format!(
            "built-in artifact {uri} declares entry {:?} but it's not in the layer",
            art.entry
        )));
    }

    let exec = format!("{CANONICAL_HARNESS_DIR_ABS}/{}", art.entry);
    // Pin both the human tag and the immutable digest so audit /
    // residency / warm-snapshot identity all key off the same string.
    let version_pinned = format!("{}@{}", version, pulled.manifest_digest.as_str());

    Ok(HarnessManifest {
        builtin: Some(builtin.to_string()),
        name: Some(art.name),
        exec: Some(exec),
        args: art.args,
        version: Some(version_pinned),
    })
}

/// Validate that a *custom* harness declared in `engram.toml` actually
/// has its `exec` present on the rootfs the author built. The custom
/// path doesn't pull anything — the author's Dockerfile COPY'd the
/// binary in — but the baker still owes them an early "you forgot to
/// COPY it" error rather than shipping an image that can't launch.
pub async fn validate_custom_harness(
    rootfs_dir: &Path,
    manifest: &ImageManifest,
) -> Result<(), BuildError> {
    let Some(harness) = manifest.harness.as_ref() else {
        return Ok(());
    };
    if harness.builtin.is_some() {
        // Built-in path handles its own validation via inject.
        return Ok(());
    }
    let Some(exec) = harness.exec.as_deref() else {
        return Ok(()); // validate_source already errored.
    };
    let rel = exec.trim_start_matches('/');
    let on_disk = rootfs_dir.join(rel);
    if !tokio::fs::try_exists(&on_disk).await.unwrap_or(false) {
        return Err(BuildError::Config(format!(
            "[harness] exec {exec:?} not found in the built rootfs — did your Dockerfile COPY it?"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_resolves_claude_linux_x86_64() {
        let cat = BuiltinCatalog::default_catalog();
        let uri = cat
            .resolve("claude", "v1.2.3", Platform::LinuxX86_64)
            .unwrap();
        assert_eq!(
            uri,
            "ghcr.io/cortexapps/engrams/harness-claude:v1.2.3-linux-x86_64"
        );
    }

    #[test]
    fn catalog_rejects_unknown_builtin() {
        let cat = BuiltinCatalog::default_catalog();
        let err = cat
            .resolve("not-a-thing", "v0.1.0", Platform::LinuxX86_64)
            .unwrap_err();
        let msg = format!("{err:?}");
        assert!(
            msg.contains("not a known built-in") && msg.contains("claude"),
            "error should name the typo and list known: {msg}"
        );
    }

    #[test]
    fn artifact_toml_parses_minimal_and_full() {
        let minimal = r#"
            name = "claude"
            version = "v1.2.3"
            entry = "harness"
        "#;
        let art: ArtifactToml = toml::from_str(minimal).unwrap();
        assert_eq!(art.name, "claude");
        assert_eq!(art.entry, "harness");
        assert!(art.args.is_empty());

        let full = r#"
            name = "claude"
            version = "v1.2.3"
            entry = "harness"
            args = ["--serve"]
            description = "Anthropic Claude Code, persistent server mode"
        "#;
        let art: ArtifactToml = toml::from_str(full).unwrap();
        assert_eq!(art.args, vec!["--serve"]);
        assert_eq!(
            art.description.as_deref(),
            Some("Anthropic Claude Code, persistent server mode")
        );
    }

    #[test]
    fn artifact_toml_rejects_unknown_fields() {
        // deny_unknown_fields catches a stale CI publishing pipeline
        // that drifts ahead of the consumer.
        let drifted = r#"
            name = "claude"
            version = "v1.2.3"
            entry = "harness"
            extra_field = "future drift"
        "#;
        let res: Result<ArtifactToml, _> = toml::from_str(drifted);
        assert!(res.is_err(), "unknown field must be rejected");
    }
}
