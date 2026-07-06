use std::collections::HashMap;

use serde::{Deserialize, Serialize};

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
// ADR 0057: NO `deny_unknown_fields`. Manifests baked before the strip still
// carry `[secrets]`/`[network]`/`secret_mode`; the coordinator parses those
// manifests fine and ignores those sections — session network/secrets now come
// from the profile-compiled policy, not the image.
pub struct ImageManifest {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,

    /// Non-secret environment variables applied to every sandbox
    /// spawned from this image. Includes the image's Dockerfile `ENV`,
    /// folded in at bake time as a base (see
    /// [`ImageManifest::apply_image_config_defaults`]); an `engram.toml`
    /// `[env]` key overrides the Dockerfile value for the same key, and
    /// a session-supplied env value in turn overrides this.
    #[serde(default)]
    pub env: HashMap<String, String>,

    /// Default working directory for processes launched in this image:
    /// the harness at `start_agent`, and `engram exec` when the request
    /// doesn't carry its own `workdir`.
    ///
    /// Resolution, highest precedence first: an explicit `engram.toml`
    /// `workdir`, else the Dockerfile `WORKDIR` (the baker reads the
    /// image config and folds it in via
    /// [`ImageManifest::apply_image_config_defaults`]), else the sandbox
    /// default cwd `/` (`None`). The directory must already exist in the
    /// rootfs; like a bad `exec` path, an absent `workdir` fails the
    /// spawn.
    #[serde(default)]
    pub workdir: Option<String>,

    // ADR 0057: `secrets`, `network`, and `secret_mode` are REMOVED from the
    // manifest. Egress network + injected secrets are now session policy, set on
    // the profile and compiled into the per-session IntegrationPolicy (the
    // coordinator's sole source). The image declares only what it *is* (env,
    // workdir, resources); not what a session may *reach* or *hold*.
    /// Resource hints used as defaults when a session doesn't override.
    #[serde(default)]
    pub resources: ResourceHints,

    /// Optional capture-time prewarm hook. When set, base-snapshot
    /// capture runs `[warm] command` inside the capture VM AFTER agentd
    /// is ready and BEFORE the memory snapshot is frozen — so any
    /// long-lived process the command leaves running (a `gradle
    /// --daemon`, a language server, a warmed JIT) is captured live into
    /// the base snapshot and is already running when EVERY session of
    /// this image restores. See [`WarmConfig`].
    ///
    /// This is the general counterpart to the harness warm-capture: the
    /// harness needs per-session late-bind, but a warm-hook process is
    /// session-agnostic (shared verbatim across all restores), so there's
    /// no restore-side wiring — it just comes back live with the snapshot.
    /// Only consumed by `build_base_snapshot` (image enable); cold-boot
    /// `create` ignores it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warm: Option<WarmConfig>,
}

/// Capture-time prewarm hook for an image's base snapshot.
///
/// The `command` is run inside the capture VM (via the same `exec` path
/// `engram exec` uses) once agentd is ready, just before the memory
/// snapshot is frozen. It must START its long-lived process **detached**
/// and then **exit** — e.g. `gradle --daemon help` launches the Gradle
/// daemon as a separate process and returns; that daemon stays alive and
/// is captured into the base snapshot, so every restored session inherits
/// a warm, cache-hot daemon with no cold-start.
///
/// **Fail-loud:** a non-zero exit, a stall, a blown per-stage deadline, or
/// the global timeout all abort the capture and therefore the whole image
/// enable (issue #539 — see [`docs/warm-hooks.md`](../../../../docs/warm-hooks.md)
/// for the full contract: what the hook may assume, deadline semantics, and
/// the `::engram-warm::` progress-line protocol a hook can emit for
/// observability). A declared warm hook that can't run is a real defect
/// (bad command, cold cache, OOM); we never silently ship a "cold" base
/// snapshot that claims to be warm.
///
/// **Hermetic by default, egress opt-in:** the warm command runs without
/// per-session secrets (the capture VM carries the manifest `[env]` +
/// `capture_env`, not a session's `[secrets]`) and, absent [`Self::network`],
/// **no network** — driven off baked, offline caches. An image whose warm
/// boot genuinely needs the network (eager OIDC discovery, an `op inject`)
/// opts in via `[warm.network]`.
///
/// **Backend note:** `build_base_snapshot` (where the hook runs) is
/// implemented only on `PooledBackend`'s memory-snapshot path
/// (Firecracker); VZ and Process opt out of base-snapshot capture entirely
/// via the trait's default (a hard error) — there's no VZ-specific gate on
/// the `[warm]` hook itself, capture just isn't offered on that backend.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WarmConfig {
    /// argv of the warm command (non-empty). Run inside the capture VM.
    pub command: Vec<String>,

    /// Max wall-clock for the warm command before the capture gives up
    /// and fails (fail-loud). Defaults to [`WarmConfig::DEFAULT_TIMEOUT_SECS`].
    #[serde(default)]
    pub timeout_secs: Option<u64>,

    /// Working directory for the warm command. Defaults to the image's
    /// `workdir` (and ultimately the sandbox default `/`) when omitted.
    #[serde(default)]
    pub workdir: Option<String>,

    /// Network policy for the capture VM while the `[warm]` hook runs.
    /// Absent (the default) → the capture stays **egress-less**: it gets a
    /// tap + guest IP, but the egress proxy denies its traffic as an unknown
    /// guest because no policy is registered. An image whose warm boot needs
    /// the network (e.g. eager OIDC discovery, an `op inject`) opts in here,
    /// and the host-agent registers a matching egress policy for the capture
    /// VM's guest IP for the duration of the hook (torn down after):
    ///
    /// - `[warm.network] default = "allow"` → **allow-all** (the dev posture:
    ///   no agent runs at capture, so the in-session threat model doesn't
    ///   apply).
    /// - `[warm.network] default = "deny"` + `allow_hosts`/`allow_host_patterns`
    ///   → a scoped **allowlist** (for security-conscious images).
    ///
    /// Reuses the same [`NetworkPolicy`] a session uses, so the warm-boot
    /// egress posture reads identically to a session's.
    #[serde(default)]
    pub network: Option<NetworkPolicy>,
}

impl WarmConfig {
    /// Default warm-command timeout: warming a daemon off baked caches
    /// should be quick, but a first-run JIT/daemon spin-up can take a
    /// while — be generous before failing the enable.
    pub const DEFAULT_TIMEOUT_SECS: u64 = 600;

    /// Validate a source-authored `[warm]` table: the command must be
    /// non-empty (an empty argv has nothing to run). Called at bake so a
    /// malformed block fails the build up front rather than shipping an
    /// image whose enable will abort at capture.
    pub fn validate(&self) -> Result<(), String> {
        if self.command.iter().all(|a| a.trim().is_empty()) {
            return Err("[warm] command must be a non-empty argv".into());
        }
        Ok(())
    }

    /// The effective timeout, applying [`Self::DEFAULT_TIMEOUT_SECS`].
    pub fn timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.timeout_secs.unwrap_or(Self::DEFAULT_TIMEOUT_SECS))
    }
}

impl ImageManifest {
    /// Fold a built image's Docker config (its `ENV` as raw
    /// `KEY=VALUE` strings + `WORKDIR`) in as **defaults** — the
    /// author's `engram.toml` always wins:
    /// - env: a `KEY` already present in `[env]` is left untouched;
    ///   only Dockerfile-only keys are added.
    /// - workdir: an explicit manifest `workdir` overrides; otherwise
    ///   the Dockerfile `WORKDIR` applies; absent both, the sandbox
    ///   default (`/`) stands (left `None`).
    ///
    /// The baker calls this before rendering `manifest.toml`, so every
    /// downstream consumer (enable, create, resume, `/exec`) sees one
    /// merged manifest and the platform never has to re-read the OCI
    /// image config. Malformed env entries (no `=`) are skipped; an
    /// empty `WORKDIR` is treated as unset by the caller.
    pub fn apply_image_config_defaults(&mut self, env: &[String], working_dir: Option<&str>) {
        for kv in env {
            if let Some((k, v)) = kv.split_once('=') {
                self.env
                    .entry(k.to_string())
                    .or_insert_with(|| v.to_string());
            }
        }
        if self.workdir.is_none() {
            if let Some(wd) = working_dir.filter(|w| !w.is_empty()) {
                self.workdir = Some(wd.to_string());
            }
        }
    }
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

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
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
    /// ADR 0048: the guest's vCPU count. A DECLARATION, not a hint —
    /// enable-time validation rejects an image that omits it, and
    /// placement reserves it against the host's
    /// `total_vcpus × overcommit` budget so packing has a CPU bound.
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
    }

    /// ADR 0057: `[git]` was retired (D2) along with `secrets`/`secret_mode`/
    /// `network` (B2b) — all session policy now. A manifest baked BEFORE the
    /// strip with any of those sections must still parse (the fields are gone +
    /// `deny_unknown_fields` is off), the sections simply ignored.
    #[test]
    fn manifest_ignores_retired_git_block() {
        let m: ImageManifest = toml::from_str(
            r#"
            name = "dev-engrams"
            [git]
            provider = "github"
            owner = "cortexapps"
        "#,
        )
        .expect("a pre-strip [git] block must still parse (ignored)");
        assert_eq!(m.name, "dev-engrams");
    }

    /// ADR 0057: `secrets`/`secret_mode`/`network` were removed from the
    /// manifest (they're session policy now). A manifest baked BEFORE the strip
    /// still carries those sections; with `deny_unknown_fields` dropped, such a
    /// manifest must still parse — the coordinator ignores the stripped sections
    /// (session network/secrets come from the profile-compiled policy). The kept
    /// fields (name/description/env/resources) still parse normally.
    #[test]
    fn manifest_ignores_pre_strip_secrets_and_network_sections() {
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

            [network]
            default = "deny"
            allow_hosts = ["api.github.com", "registry.npmjs.org"]

            [resources]
            suggested_memory_mib = 4096
            suggested_vcpus = 2
        "#;
        // The stripped sections are ignored, not rejected (no deny_unknown_fields).
        let m: ImageManifest = toml::from_str(src).unwrap();
        assert_eq!(m.name, "cortex-api");
        assert_eq!(m.description.as_deref(), Some("Backend API service"));
        assert_eq!(m.env.get("PYTHONUNBUFFERED").map(String::as_str), Some("1"));
        assert_eq!(m.resources.suggested_memory_mib, Some(4096));
        assert_eq!(m.resources.suggested_vcpus, Some(2));
    }

    #[test]
    fn apply_image_config_defaults_folds_env_and_workdir() {
        // engram.toml that overrides one var + sets nothing for workdir.
        let mut m: ImageManifest = toml::from_str(
            r#"
            name = "x"
            [env]
            RUSTC_WRAPPER = "sccache"
        "#,
        )
        .unwrap();
        m.apply_image_config_defaults(
            &[
                "PATH=/opt/cargo/bin:/usr/bin".to_string(),
                "RUSTC_WRAPPER=should-not-win".to_string(), // manifest wins
                "malformed-no-equals".to_string(),          // skipped
            ],
            Some("/workspace/engrams"),
        );
        // Dockerfile-only key added; manifest's value preserved.
        assert_eq!(
            m.env.get("PATH").map(String::as_str),
            Some("/opt/cargo/bin:/usr/bin")
        );
        assert_eq!(
            m.env.get("RUSTC_WRAPPER").map(String::as_str),
            Some("sccache")
        );
        assert!(!m.env.contains_key("malformed-no-equals"));
        // Dockerfile WORKDIR fills the unset manifest workdir.
        assert_eq!(m.workdir.as_deref(), Some("/workspace/engrams"));
    }

    #[test]
    fn apply_image_config_defaults_respects_explicit_workdir_and_empty() {
        let mut m: ImageManifest =
            toml::from_str("name = \"x\"\nworkdir = \"/manifest-wins\"").unwrap();
        m.apply_image_config_defaults(&[], Some("/from-docker"));
        assert_eq!(
            m.workdir.as_deref(),
            Some("/manifest-wins"),
            "explicit manifest workdir wins"
        );

        // Empty Dockerfile WORKDIR must not shadow the default `/`.
        let mut m: ImageManifest = toml::from_str(r#"name = "x""#).unwrap();
        m.apply_image_config_defaults(&[], Some(""));
        assert_eq!(m.workdir, None);
        m.apply_image_config_defaults(&[], None);
        assert_eq!(m.workdir, None);
    }

    #[test]
    fn manifest_with_env_and_workdir_round_trips_through_toml() {
        // Guards the baker's re-render: a non-empty [env] table *and* a
        // top-level `workdir` scalar must serialize + parse back intact
        // (toml must tolerate the scalar after the table).
        let mut m: ImageManifest = toml::from_str(r#"name = "x""#).unwrap();
        m.apply_image_config_defaults(
            &["PATH=/opt/cargo/bin:/usr/bin".to_string()],
            Some("/workspace"),
        );
        let rendered = toml::to_string(&m).unwrap();
        let back: ImageManifest = toml::from_str(&rendered).unwrap();
        assert_eq!(back.workdir.as_deref(), Some("/workspace"));
        assert_eq!(
            back.env.get("PATH").map(String::as_str),
            Some("/opt/cargo/bin:/usr/bin")
        );
    }

    #[test]
    fn manifest_ignores_unknown_top_level_keys() {
        // ADR 0057: `deny_unknown_fields` was dropped so the coordinator can
        // parse manifests baked BEFORE the secrets/network/secret_mode strip
        // (those carry the now-removed sections) during the rollout window. The
        // tradeoff: unknown/legacy top-level keys are silently ignored, not
        // rejected — so a baker-side typo no longer fails the parse here.
        let src = r#"
            name = "cortex-api"
            secret_mode = "broker"
            enviroment = { FOO = "bar" }
        "#;
        let m: ImageManifest = toml::from_str(src).expect("legacy/unknown keys are ignored");
        assert_eq!(m.name, "cortex-api");
    }

    #[test]
    fn manifest_parses_warm_block() {
        // No [warm] → none, and nothing rendered out.
        let plain: ImageManifest = toml::from_str(r#"name = "x""#).unwrap();
        assert!(plain.warm.is_none());
        assert!(
            !toml::to_string(&plain).unwrap().contains("warm"),
            "a warm-less image must not render a [warm] table"
        );

        // Full block: command + timeout + workdir.
        let m: ImageManifest = toml::from_str(
            r#"
            name = "dev-brain"
            [warm]
            command = ["bash", "-lc", "gradle --daemon help"]
            timeout_secs = 900
            workdir = "/workspace/brain-backend"
        "#,
        )
        .unwrap();
        let w = m.warm.expect("warm table parsed");
        assert_eq!(w.command, vec!["bash", "-lc", "gradle --daemon help"]);
        assert_eq!(w.timeout_secs, Some(900));
        assert_eq!(w.workdir.as_deref(), Some("/workspace/brain-backend"));
        assert_eq!(w.timeout().as_secs(), 900);
        w.validate().expect("non-empty command is valid");

        // Minimal block: just a command; timeout/workdir default.
        let min: ImageManifest = toml::from_str(
            r#"
            name = "x"
            [warm]
            command = ["/usr/local/bin/engram-warm"]
        "#,
        )
        .unwrap();
        let w = min.warm.unwrap();
        assert!(w.timeout_secs.is_none());
        assert!(w.workdir.is_none());
        assert_eq!(w.timeout().as_secs(), WarmConfig::DEFAULT_TIMEOUT_SECS);
    }

    #[test]
    fn warm_validate_rejects_empty_command() {
        // Empty argv: nothing to run.
        let empty = WarmConfig {
            command: vec![],
            timeout_secs: None,
            workdir: None,
            network: None,
        };
        assert!(empty.validate().is_err(), "empty command must be rejected");

        // All-whitespace argv: also nothing to run.
        let blank = WarmConfig {
            command: vec!["  ".into(), "\t".into()],
            timeout_secs: None,
            workdir: None,
            network: None,
        };
        assert!(
            blank.validate().is_err(),
            "all-whitespace command must be rejected"
        );

        WarmConfig {
            command: vec!["echo".into(), "ok".into()],
            timeout_secs: None,
            workdir: None,
            network: None,
        }
        .validate()
        .expect("a real command is valid");
    }

    #[test]
    fn warm_block_rejects_unknown_field() {
        // deny_unknown_fields guards typos in the block.
        assert!(
            toml::from_str::<ImageManifest>(
                r#"
                name = "x"
                [warm]
                command = ["echo"]
                timeout = 10
            "#
            )
            .is_err(),
            "typo `timeout` (vs timeout_secs) must be rejected"
        );
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
