use std::collections::HashMap;

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------
// ImageConfig — the per-image runtime config (ADR 0080).
//
// Supplied OUT-OF-BAND via the ImageService (EnableImage / UpdateImage)
// and stored as JSONB on the `enabled_images` row — the bake carries no
// metadata (engram.toml is retired). Also parseable from a
// repo-versioned TOML file handed to `engram-cli image enable/update
// --config`. Tells the coordinator what env to set, what resources the
// guest gets, and how to run the capture-time warm hook (command +
// secrets + egress). Identical schema for every backend — only the
// rootfs format differs.
// ---------------------------------------------------------------------

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
// ADR 0080: `deny_unknown_fields` is back — the config never comes from a
// baked artifact anymore (ADR 0057's reason for dropping it), so strict
// parsing is a feature again: a typo in the CLI's --config TOML or a
// hand-edited JSONB value fails loudly instead of silently dropping a key.
#[serde(deny_unknown_fields)]
pub struct ImageConfig {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,

    /// Non-secret environment variables applied to every sandbox
    /// spawned from this image. The image's Dockerfile `ENV` arrives
    /// separately as [`OciRuntimeDefaults`] (extracted from the OCI
    /// image config at enable time) and is merged UNDER these by
    /// [`ImageConfig::merged_with`] — a config `[env]` key overrides the
    /// Dockerfile value for the same key, and a session-supplied env
    /// value in turn overrides this.
    #[serde(default)]
    pub env: HashMap<String, String>,

    /// Default working directory for processes launched in this image:
    /// the harness at `start_agent`, and `engram exec` when the request
    /// doesn't carry its own `workdir`.
    ///
    /// Resolution, highest precedence first: an explicit config
    /// `workdir`, else the Dockerfile `WORKDIR` (arriving as
    /// [`OciRuntimeDefaults`], merged in by [`ImageConfig::merged_with`]),
    /// else the sandbox default cwd `/` (`None`). The directory must
    /// already exist in the rootfs; like a bad `exec` path, an absent
    /// `workdir` fails the spawn.
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
/// per-session secrets (the capture VM carries the config `[env]` +
/// [`Self::env`]'s resolved capture entries, not a session's secrets) and,
/// absent [`Self::network`], **no network** — driven off baked, offline
/// caches. An image whose warm boot genuinely needs the network (eager
/// OIDC discovery, a gradle dependency fetch) opts in via `[warm.network]`.
/// ADR 0080: both knobs are first-class parts of this config — RPC-set and
/// editable (with a recapture), never baked.
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

    /// Capture-time environment for the hook: literals and secret refs
    /// (resolved through the `SecretStore` at capture; refs persisted,
    /// values never). ADR 0080: absorbs the retired standalone
    /// `capture_env` column/RPC field — warm secrets are part of the warm
    /// config, one shape, one edit surface. Resolution is FAIL-LOUD: an
    /// unresolvable ref aborts the capture (a silently-missing secret
    /// bakes a corrupt warm snapshot).
    ///
    /// NO `skip_serializing_if` here (the `WarmStageRecord` precedent —
    /// see capture_progress.rs): `WarmConfig` ALSO crosses the coord→host
    /// wire as bincode-positional `warm_bincode`, and skipping a
    /// non-trailing field silently corrupts that framing (the dev-brain
    /// enable failed decode with UnexpectedEof on exactly this). The
    /// entries themselves never ride the wire anyway — the coordinator
    /// strips `env`/`network` from the wire clone (internally-tagged
    /// `CaptureEnvValue` can't bincode-decode at all) and ships the
    /// resolved env separately.
    #[serde(default)]
    pub env: Vec<CaptureEnvEntry>,

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

/// Dockerfile-derived runtime defaults, extracted from the OCI image's
/// config blob (`ENV` + `WORKDIR`) at bake time and persisted alongside
/// the admin [`ImageConfig`] on the `enabled_images` row (ADR 0080).
/// Merged UNDER the admin config by [`ImageConfig::merged_with`] — kept
/// separate so a cheap config edit never has to re-read (or lose) what
/// the Dockerfile declared.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct OciRuntimeDefaults {
    /// Dockerfile `ENV`, already split into key/value pairs.
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// Dockerfile `WORKDIR` (`None` when unset or empty).
    #[serde(default)]
    pub workdir: Option<String>,
}

impl OciRuntimeDefaults {
    /// Build from a Docker/OCI image config's raw `ENV` (`KEY=VALUE`
    /// strings) + `WORKDIR`. Malformed env entries (no `=`) are
    /// skipped; an empty `WORKDIR` is treated as unset.
    pub fn from_docker_config(env: &[String], working_dir: Option<&str>) -> Self {
        let mut out = Self::default();
        for kv in env {
            if let Some((k, v)) = kv.split_once('=') {
                out.env.insert(k.to_string(), v.to_string());
            }
        }
        out.workdir = working_dir.filter(|w| !w.is_empty()).map(|w| w.to_string());
        out
    }
}

/// Fallback guest vCPU count for configs missing the declaration
/// (test / non-enabled paths — enable-time validation requires it).
pub const DEFAULT_VCPUS: u32 = 2;
/// Fallback guest memory for configs missing `suggested_memory_mib`.
pub const DEFAULT_MEMORY_MIB: u32 = 4096;

impl ImageConfig {
    /// Resolved guest memory (MiB) for an image: its
    /// `suggested_memory_mib` (or the default). The single source of
    /// truth shared by base-snapshot capture (`enabled_images`), session
    /// restore, AND placement reservation (sessions and captures alike,
    /// ADR 0046/0081) — FC requires the restore `mem_size_mib` to equal
    /// the snapshot's, so they MUST compute it identically. ADR 0055:
    /// the base snapshot is sized once per image and is skill-agnostic
    /// (skills bind via `patch_drive`, never resize memory), so
    /// memory-heavy tooling (e.g. browser) is an image-sizing concern —
    /// declare `suggested_memory_mib` on the image, not a per-session
    /// skill.
    pub fn resolved_memory_mib(&self) -> u32 {
        self.resources
            .suggested_memory_mib
            .unwrap_or(DEFAULT_MEMORY_MIB)
    }

    /// ADR 0048: resolved guest vCPU count for an image. Enable-time
    /// validation ([`ImageConfig::validate`]) guarantees the declaration
    /// is present for enabled images; [`DEFAULT_VCPUS`] is the defensive
    /// fallback for the test / non-enabled paths, mirroring
    /// [`ImageConfig::resolved_memory_mib`]. This is the budget
    /// placement reserves.
    pub fn resolved_vcpus(&self) -> u32 {
        self.resources.suggested_vcpus.unwrap_or(DEFAULT_VCPUS)
    }

    /// ADR 0112: resolved guest swap size (MiB) for an image. 0 ⇒ no
    /// swap device (the default — swap is opt-in via
    /// `suggested_swap_mib`). Unlike memory this never falls back to a
    /// non-zero default, and it does NOT need the capture/restore
    /// compute-identically dance: the value is captured into the
    /// sidecar's `SandboxSpec` and restore reads the recorded copy, so
    /// the device geometry cannot skew. Enable-time validation
    /// ([`ImageConfig::validate`]) bounds it at the guest's memory.
    pub fn resolved_swap_mib(&self) -> u32 {
        self.resources.suggested_swap_mib.unwrap_or(0)
    }

    /// The effective per-image config: this admin-authored config with
    /// the image's Dockerfile-derived [`OciRuntimeDefaults`] folded in
    /// as **defaults** — the admin config always wins:
    /// - env: a `KEY` already present in `[env]` is left untouched;
    ///   only Dockerfile-only keys are added.
    /// - workdir: an explicit config `workdir` overrides; otherwise the
    ///   Dockerfile `WORKDIR` applies; absent both, the sandbox default
    ///   (`/`) stands (left `None`).
    ///
    /// Every runtime consumer (enable/capture, create, resume, `/exec`)
    /// reads through this one merge, so the platform never re-reads the
    /// OCI image config after enable.
    pub fn merged_with(&self, defaults: &OciRuntimeDefaults) -> ImageConfig {
        let mut merged = self.clone();
        for (k, v) in &defaults.env {
            merged.env.entry(k.clone()).or_insert_with(|| v.clone());
        }
        if merged.workdir.is_none() {
            merged.workdir = defaults.workdir.clone();
        }
        merged
    }

    /// Validate an RPC/CLI-supplied config. Called at EnableImage /
    /// UpdateImage (the user-facing rejection point) and again by the
    /// enable scanner before capture (defense in depth).
    pub fn validate(&self) -> Result<(), String> {
        if self.name.trim().is_empty() {
            return Err("config `name` must be non-empty".into());
        }
        // ADR 0048: vCPUs are a DECLARATION placement reserves against
        // the host budget, not a hint — required.
        if self.resources.suggested_vcpus.is_none() {
            return Err(
                "config `[resources] suggested_vcpus` is required (ADR 0048: placement \
                 reserves it against the host CPU budget)"
                    .into(),
            );
        }
        // ADR 0112: a swap device larger than the guest's memory is
        // always a misconfiguration — the capture-time `swapoff` must
        // page everything back into RAM, so swap > memory can never
        // fully disarm. Reject at the user-facing boundary instead of
        // silently capping.
        if self.resolved_swap_mib() > self.resolved_memory_mib() {
            return Err(format!(
                "config `[resources] suggested_swap_mib` ({}) exceeds the guest's memory \
                 ({} MiB) — swap must fit back into RAM at capture (ADR 0112)",
                self.resolved_swap_mib(),
                self.resolved_memory_mib(),
            ));
        }
        if let Some(warm) = &self.warm {
            warm.validate()?;
        }
        Ok(())
    }
}

/// One capture-time environment entry for an image's `[warm]` hook
/// ([`WarmConfig::env`]). The value is either a literal (a non-secret
/// flag) or a secret ref resolved at capture through the same
/// `SecretStore` a session uses (e.g. an org-secret name). The
/// coordinator stores the ref, resolves it transiently at capture, and
/// never logs or persists the resolved value.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CaptureEnvEntry {
    /// Environment variable name the warm hook sees.
    pub name: String,
    pub value: CaptureEnvValue,
}

/// The value half of a [`CaptureEnvEntry`]: a literal, or a secret ref.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CaptureEnvValue {
    /// A literal, non-secret value (a flag, a host name).
    Literal { value: String },
    /// A secret ref resolved at capture via the `SecretStore`. The ref is
    /// what's persisted; the resolved value is transient.
    SecretRef { secret_ref: String },
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

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceHints {
    pub suggested_memory_mib: Option<u32>,
    /// ADR 0048: the guest's vCPU count. A DECLARATION, not a hint —
    /// enable-time validation rejects an image that omits it, and
    /// placement reserves it against the host's
    /// `total_vcpus × overcommit` budget so packing has a CPU bound.
    pub suggested_vcpus: Option<u32>,
    /// The session's disk budget in GiB. Feeds host placement
    /// (`DiskLimit`, the 2D packing bound) AND — ADR 0093 addendum —
    /// floors the packed ext4's size at enable-time materialization, so
    /// sessions actually get this much filesystem (the packer's default
    /// is content-sized: `max(2×content, content+128 MiB)`). Changing it
    /// changes the disk manifest, so a bump takes effect on the next
    /// refresh/enable (re-capture), not on live sessions.
    pub suggested_disk_gib: Option<u32>,
    /// ADR 0112: size of the guest's ephemeral swap device in MiB.
    /// Absent or 0 ⇒ no swap device — nothing changes for the image.
    /// Opting in attaches a host-file-backed RW drive whose contents
    /// are discarded at every capture (`swapoff` runs before the
    /// pause), so the value also bounds the worst-case capture-time
    /// page-back-in. [`recommended_swap_mib`] gives the default sizing
    /// formula; enable-time validation rejects a value larger than the
    /// guest's memory (a `swapoff` that can never complete). Like
    /// memory, the device geometry is frozen into the base snapshot's
    /// `state.bin`, so a change takes effect on re-capture.
    pub suggested_swap_mib: Option<u32>,
}

/// ADR 0112: the recommended swap size for a guest with `memory_mib`
/// of RAM — `clamp(memory/4, 1 GiB, 8 GiB)`. RAM/4 bounds three things
/// at once: the worst-case `swapoff` page-back-in at capture, the
/// worst-case host disk footprint, and the degree of overcommit worth
/// papering over before a bigger guest is the honest fix. Tooling that
/// suggests a value uses this; [`ResourceHints::suggested_swap_mib`]
/// always wins when set.
pub fn recommended_swap_mib(memory_mib: u32) -> u32 {
    (memory_mib / 4).clamp(1024, 8192)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ADR 0112 sizing: swap is opt-in (absent/0 ⇒ off), the
    /// recommended formula is clamp(mem/4, 1 GiB, 8 GiB), and
    /// enable-time validation rejects swap > memory.
    #[test]
    fn swap_sizing_resolves_and_validates() {
        let mk = |mem: Option<u32>, swap: Option<u32>| ImageConfig {
            name: "x".into(),
            resources: ResourceHints {
                suggested_memory_mib: mem,
                suggested_vcpus: Some(2),
                suggested_disk_gib: None,
                suggested_swap_mib: swap,
            },
            ..ImageConfig::default()
        };

        // Opt-in semantics.
        assert_eq!(mk(Some(24576), None).resolved_swap_mib(), 0);
        assert_eq!(mk(Some(24576), Some(0)).resolved_swap_mib(), 0);
        assert_eq!(mk(Some(24576), Some(6144)).resolved_swap_mib(), 6144);

        // The recommended formula: mem/4 clamped to [1 GiB, 8 GiB].
        assert_eq!(recommended_swap_mib(24576), 6144); // dev-brain
        assert_eq!(recommended_swap_mib(2048), 1024); // floor
        assert_eq!(recommended_swap_mib(65536), 8192); // ceiling
        assert_eq!(recommended_swap_mib(DEFAULT_MEMORY_MIB), 1024);

        // Validation: swap must fit back into RAM at capture.
        assert!(mk(Some(4096), Some(4096)).validate().is_ok());
        let err = mk(Some(4096), Some(4097)).validate().unwrap_err();
        assert!(err.contains("suggested_swap_mib"), "got: {err}");
        // The memory default applies when memory is unset.
        assert!(mk(None, Some(DEFAULT_MEMORY_MIB + 1)).validate().is_err());
        assert!(mk(None, Some(1024)).validate().is_ok());
    }

    #[test]
    fn config_parses_minimal_toml() {
        let src = r#"
            name = "cortex-api"
        "#;
        let c: ImageConfig = toml::from_str(src).unwrap();
        assert_eq!(c.name, "cortex-api");
        assert!(c.env.is_empty());
    }

    /// ADR 0080: the config is RPC/CLI-authored (never from a baked
    /// artifact), so `deny_unknown_fields` is back — retired manifest-era
    /// sections and typos are REJECTED, not ignored.
    #[test]
    fn config_rejects_retired_manifest_sections_and_typos() {
        for src in [
            "name = \"x\"\n[git]\nprovider = \"github\"\n",
            "name = \"x\"\nsecret_mode = \"broker\"\n",
            "name = \"x\"\n[network]\ndefault = \"deny\"\n",
            "name = \"x\"\nenviroment = { FOO = \"bar\" }\n",
        ] {
            assert!(
                toml::from_str::<ImageConfig>(src).is_err(),
                "must reject: {src}"
            );
        }
    }

    #[test]
    fn merged_with_folds_env_and_workdir_under_config() {
        let c: ImageConfig = toml::from_str(
            r#"
            name = "x"
            [env]
            RUSTC_WRAPPER = "sccache"
        "#,
        )
        .unwrap();
        let defaults = OciRuntimeDefaults::from_docker_config(
            &[
                "PATH=/opt/cargo/bin:/usr/bin".to_string(),
                "RUSTC_WRAPPER=should-not-win".to_string(), // config wins
                "malformed-no-equals".to_string(),          // skipped
            ],
            Some("/workspace/engrams"),
        );
        let m = c.merged_with(&defaults);
        // Dockerfile-only key added; config's value preserved.
        assert_eq!(
            m.env.get("PATH").map(String::as_str),
            Some("/opt/cargo/bin:/usr/bin")
        );
        assert_eq!(
            m.env.get("RUSTC_WRAPPER").map(String::as_str),
            Some("sccache")
        );
        assert!(!m.env.contains_key("malformed-no-equals"));
        // Dockerfile WORKDIR fills the unset config workdir.
        assert_eq!(m.workdir.as_deref(), Some("/workspace/engrams"));
        // The original config is untouched (merge is read-side).
        assert!(!c.env.contains_key("PATH"));
    }

    #[test]
    fn merged_with_respects_explicit_workdir_and_empty() {
        let c: ImageConfig = toml::from_str("name = \"x\"\nworkdir = \"/config-wins\"").unwrap();
        let d = OciRuntimeDefaults::from_docker_config(&[], Some("/from-docker"));
        assert_eq!(
            c.merged_with(&d).workdir.as_deref(),
            Some("/config-wins"),
            "explicit config workdir wins"
        );

        // Empty Dockerfile WORKDIR must not shadow the default `/`.
        let c: ImageConfig = toml::from_str(r#"name = "x""#).unwrap();
        assert_eq!(
            OciRuntimeDefaults::from_docker_config(&[], Some("")).workdir,
            None
        );
        assert_eq!(c.merged_with(&OciRuntimeDefaults::default()).workdir, None);
    }

    #[test]
    fn config_round_trips_through_toml_and_json() {
        // Guards the CLI's --config path (TOML) and the JSONB column
        // (JSON): a full config must survive both.
        let src = r#"
            name = "dev-brain"
            description = "Cortex development"
            workdir = "/workspace"

            [env]
            PATH = "/opt/cargo/bin:/usr/bin"

            [resources]
            suggested_memory_mib = 24576
            suggested_vcpus = 8

            [warm]
            command = ["bash", "-lc", "gradle --daemon help"]
            timeout_secs = 900
            workdir = "/workspace/brain-backend"

            [[warm.env]]
            name = "GITHUB_PASSWORD"
            value = { kind = "secret_ref", secret_ref = "github-image-bot-pat" }

            [[warm.env]]
            name = "GRADLE_OPTS"
            value = { kind = "literal", value = "-Xmx4g" }

            [warm.network]
            default = "deny"
            allow_hosts = ["repo.maven.apache.org"]
            allow_host_patterns = ["*.gradle.org"]
        "#;
        let c: ImageConfig = toml::from_str(src).unwrap();
        c.validate().expect("full config validates");
        let w = c.warm.as_ref().unwrap();
        assert_eq!(w.env.len(), 2);
        assert_eq!(w.env[0].name, "GITHUB_PASSWORD");
        assert!(matches!(
            &w.env[0].value,
            CaptureEnvValue::SecretRef { secret_ref } if secret_ref == "github-image-bot-pat"
        ));
        let net = w.network.as_ref().unwrap();
        assert_eq!(net.default, NetworkDefault::Deny);
        assert_eq!(net.allow_hosts, vec!["repo.maven.apache.org"]);

        let json = serde_json::to_string(&c).unwrap();
        let back: ImageConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.name, "dev-brain");
        assert_eq!(back.warm.as_ref().unwrap().env.len(), 2);
        assert_eq!(
            back.warm.as_ref().unwrap().network,
            c.warm.as_ref().unwrap().network
        );

        let rendered = toml::to_string(&c).unwrap();
        let back: ImageConfig = toml::from_str(&rendered).unwrap();
        assert_eq!(back.warm.unwrap().env.len(), 2);
    }

    #[test]
    fn validate_requires_name_and_vcpus() {
        let c: ImageConfig = toml::from_str(r#"name = "x""#).unwrap();
        let err = c.validate().unwrap_err();
        assert!(err.contains("suggested_vcpus"), "{err}");

        let c: ImageConfig =
            toml::from_str("name = \"  \"\n[resources]\nsuggested_vcpus = 2\n").unwrap();
        let err = c.validate().unwrap_err();
        assert!(err.contains("name"), "{err}");

        let c: ImageConfig =
            toml::from_str("name = \"x\"\n[resources]\nsuggested_vcpus = 2\n").unwrap();
        c.validate()
            .expect("name + vcpus is the minimal valid config");
    }

    #[test]
    fn config_parses_warm_block() {
        // No [warm] → none, and nothing rendered out.
        let plain: ImageConfig = toml::from_str(r#"name = "x""#).unwrap();
        assert!(plain.warm.is_none());
        assert!(
            !toml::to_string(&plain).unwrap().contains("warm"),
            "a warm-less image must not render a [warm] table"
        );

        // Minimal block: just a command; timeout/workdir/env default.
        let min: ImageConfig = toml::from_str(
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
        assert!(w.env.is_empty());
        assert_eq!(w.timeout().as_secs(), WarmConfig::DEFAULT_TIMEOUT_SECS);
    }

    #[test]
    fn warm_validate_rejects_empty_command() {
        // Empty argv: nothing to run.
        let empty = WarmConfig {
            command: vec![],
            timeout_secs: None,
            workdir: None,
            env: Vec::new(),
            network: None,
        };
        assert!(empty.validate().is_err(), "empty command must be rejected");

        // All-whitespace argv: also nothing to run.
        let blank = WarmConfig {
            command: vec!["  ".into(), "\t".into()],
            timeout_secs: None,
            workdir: None,
            env: Vec::new(),
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
            env: Vec::new(),
            network: None,
        }
        .validate()
        .expect("a real command is valid");
    }

    #[test]
    fn warm_block_rejects_unknown_field() {
        // deny_unknown_fields guards typos in the block.
        assert!(
            toml::from_str::<ImageConfig>(
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

    /// The JSONB shape of a capture-env entry is a wire contract (the
    /// proto conversion + the web form both build it) — pin it.
    #[test]
    fn capture_env_entry_jsonb_shape_round_trips() {
        let entries = vec![
            CaptureEnvEntry {
                name: "FLAG".into(),
                value: CaptureEnvValue::Literal { value: "1".into() },
            },
            CaptureEnvEntry {
                name: "TOKEN".into(),
                value: CaptureEnvValue::SecretRef {
                    secret_ref: "org-secret-name".into(),
                },
            },
        ];
        let json = serde_json::to_string(&entries).unwrap();
        assert!(json.contains("\"kind\":\"literal\""), "{json}");
        assert!(json.contains("\"kind\":\"secret_ref\""), "{json}");
        let back: Vec<CaptureEnvEntry> = serde_json::from_str(&json).unwrap();
        assert_eq!(back, entries);
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
