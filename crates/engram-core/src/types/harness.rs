//! The harness descriptor (`harness.toml`) — ADR 0063.
//!
//! A harness-agnostic declaration each harness ships inside its bundle,
//! describing its *credential contract*: an org/programmatic env-var plus an
//! optional user env-var or OAuth connection, and the model /
//! effort enums, where each option maps to the env var(s) that select it.
//! ADR 0062's harness catalog stores this verbatim so the orchestrator/web can
//! render pickers and derive the (formerly Claude-hardcoded) env wiring without
//! reading the squashfs.
//!
//! Design choice (ADR 0063 §1): model/effort options carry a *map* of env vars
//! rather than a single shared var name + value list, so a harness needing more
//! than one var per option (e.g. a provider + model-name pair) is expressible.
//! Credential slots stay single strings — a credential is one var.

use std::collections::{BTreeMap, HashSet};

use serde::{Deserialize, Serialize};

/// Parsed `harness.toml`. I/O-free: the coordinator parses it the same way it
/// parses [`ImageManifest`](crate::types::ImageManifest), and the orchestrator
/// consumes the proto projection of it (see the coordinator's convert layer).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessDescriptor {
    /// Stable harness id. Must equal the catalog key and the value carried on
    /// `CreateSessionRequest.harness`.
    pub name: String,

    /// Human label for pickers; falls back to `name` when unset
    /// (see [`HarnessDescriptor::display_label`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,

    /// Human-readable one-liner for the admin Harnesses tab (ADR 0063 §6).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,

    /// **Launch contract (coordinator-internal; omitted from the proto
    /// projection).** Entry path within this harness's catalog subtree —
    /// `argv[0]` is `/opt/engram/dyn/0/<name>/<exec>` (ADR 0062 §3). Defaults to
    /// `harness` when unset (see [`HarnessDescriptor::exec_path`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exec: Option<String>,

    /// **Launch contract (coordinator-internal).** Extra argv appended after the
    /// SDK-standard dial flags when the host launches the harness.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,

    /// Credential env-var names (see [`HarnessAuth`]).
    #[serde(default)]
    pub auth: HarnessAuth,

    /// Model enum. Each option declares the env var(s) that select it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<HarnessOption>,

    /// Optional effort/reasoning enum; same shape as `models`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub effort: Vec<HarnessOption>,

    /// Session modes this harness supports (ADR 0107), e.g. `plan`. Pure
    /// declaration — a mode carries no env map. Mode selection rides prompts
    /// (`harness_mode`) and each harness maps its own mode to native behavior;
    /// the declaration only drives the create/composer pickers and coordinator
    /// validation. A harness that declares no modes never shows the affordance.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub modes: Vec<HarnessMode>,

    /// Egress hosts the harness *itself* must reach to function — its model
    /// API and telemetry endpoints (ADR 0063 addendum). Concatenated into the
    /// session's deny-default egress allowlist at create, so profiles never
    /// enumerate LLM-provider hosts just to keep their selected harness alive.
    #[serde(default, skip_serializing_if = "HarnessEgress::is_empty")]
    pub egress: HarnessEgress,
}

/// The `[egress]` block: hosts the harness needs the session's egress proxy to
/// allow. Same shapes as the profile network's `allow_hosts` /
/// `allow_host_patterns` (exact hostnames / `*.domain` patterns).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessEgress {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow_hosts: Vec<String>,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow_host_patterns: Vec<String>,
}

impl HarnessEgress {
    pub fn is_empty(&self) -> bool {
        self.allow_hosts.is_empty() && self.allow_host_patterns.is_empty()
    }
}

/// Credential contract. A harness may expose one human credential mechanism:
/// an env-var secret or a reusable OAuth connection (ADR 0106).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessAuth {
    /// Env var for the **programmatic / org** credential (e.g. an API key).
    /// Injected host-side from an org secret on programmatic runs (ADR 0063 §4).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub org_env: Option<String>,

    /// Env var for the **human / interactive** credential (e.g. an OAuth
    /// token). Injected from the per-user token store on human runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_env: Option<String>,

    /// OAuth connection for human sessions. Mutually exclusive with
    /// [`Self::user_env`]; OAuth payloads never enter the harness env map.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_oauth: Option<HarnessOAuth>,

    /// Free-text setup instructions for the org credential, surfaced to admins.
    /// Not validated — human guidance, never a secret.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub org_env_hint: Option<String>,

    /// Free-text setup instructions for the human credential (e.g. "Run
    /// `claude setup-token`"), surfaced on the settings + create surfaces so a
    /// user knows how to obtain it. Not validated, never a secret.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_env_hint: Option<String>,
}

/// A harness's reusable human OAuth requirement. `provider` addresses a
/// trusted coordinator driver; the bundle is opaque outside that driver and
/// its bound harness.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessOAuth {
    /// Stable driver id (for example `openai-codex`).
    pub provider: String,

    /// How the credential is delivered to the harness. V1 intentionally has
    /// one mode so future connector/MCP consumers share the same vocabulary.
    pub delivery: OAuthDelivery,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OAuthDelivery {
    OpaqueBundle,
}

/// One model or effort option. `env` is the set of env vars (with values) that
/// select this option.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessOption {
    /// Stable option id — what a profile/session stores and the wire carries
    /// (e.g. `"opus"`).
    pub id: String,

    /// Human label for pickers; falls back to `id`
    /// (see [`HarnessOption::display_label`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,

    /// Whether this is the default option when none is selected. At most one
    /// option per enum may set this.
    #[serde(default, skip_serializing_if = "is_false")]
    pub default: bool,

    /// Env vars this option sets. `BTreeMap` for deterministic ordering (the
    /// orchestrator dict-merges these into the session's harness env).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
}

/// One session mode (ADR 0107). Unlike [`HarnessOption`] there is no env map:
/// a mode is not an env selection — it rides prompts and the harness itself
/// maps it to native behavior.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessMode {
    /// Stable mode id — what `SendPromptRequest.harness_mode` carries
    /// (e.g. `"plan"`).
    pub id: String,

    /// Human label for pickers; falls back to `id`
    /// (see [`HarnessMode::display_label`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,

    /// Whether this is the mode a session starts in when none is selected. At
    /// most one mode may set this.
    #[serde(default, skip_serializing_if = "is_false")]
    pub default: bool,
}

impl HarnessMode {
    /// Display label, falling back to the mode id.
    pub fn display_label(&self) -> &str {
        self.label.as_deref().unwrap_or(&self.id)
    }
}

fn is_false(b: &bool) -> bool {
    !*b
}

impl HarnessDescriptor {
    /// Default entry name within a harness's catalog subtree when `exec` is
    /// unset.
    pub const DEFAULT_EXEC: &'static str = "harness";

    /// Parse + validate a `harness.toml` source string.
    pub fn parse(toml_src: &str) -> Result<Self, String> {
        let descriptor: HarnessDescriptor = toml::from_str(toml_src).map_err(|e| e.to_string())?;
        descriptor.validate()?;
        Ok(descriptor)
    }

    /// Shape rules: non-empty name; every declared env-var name is POSIX-ish;
    /// option ids are unique within each enum; at most one default per enum.
    pub fn validate(&self) -> Result<(), String> {
        if self.name.trim().is_empty() {
            return Err("harness.toml: name must not be empty".into());
        }
        // The exec entry is joined under the harness's catalog subtree, so it
        // must be a relative path that can't escape it.
        if let Some(exec) = &self.exec {
            if exec.is_empty() || exec.starts_with('/') || exec.split('/').any(|c| c == "..") {
                return Err(format!(
                    "harness.toml: exec must be a non-empty relative path within the harness tree, got {exec:?}"
                ));
            }
        }
        if let Some(v) = &self.auth.org_env {
            validate_env_name(v).map_err(|e| format!("harness.toml [auth] org_env: {e}"))?;
        }
        if let Some(v) = &self.auth.user_env {
            validate_env_name(v).map_err(|e| format!("harness.toml [auth] user_env: {e}"))?;
        }
        if self.auth.user_env.is_some() && self.auth.user_oauth.is_some() {
            return Err(
                "harness.toml [auth]: user_env and user_oauth are mutually exclusive".into(),
            );
        }
        if let Some(oauth) = &self.auth.user_oauth {
            validate_stable_id(&oauth.provider)
                .map_err(|e| format!("harness.toml [auth.user_oauth] provider: {e}"))?;
        }
        validate_options("models", &self.models)?;
        validate_options("effort", &self.effort)?;
        validate_modes(&self.modes)?;
        Ok(())
    }

    /// The model option with this id, if any.
    pub fn model(&self, id: &str) -> Option<&HarnessOption> {
        self.models.iter().find(|o| o.id == id)
    }

    /// The effort option with this id, if any.
    pub fn effort(&self, id: &str) -> Option<&HarnessOption> {
        self.effort.iter().find(|o| o.id == id)
    }

    /// The mode with this id, if any (ADR 0107).
    pub fn mode(&self, id: &str) -> Option<&HarnessMode> {
        self.modes.iter().find(|m| m.id == id)
    }

    /// The default mode: the one flagged `default`, else the first listed,
    /// else `None` (a harness may declare no modes).
    pub fn default_mode(&self) -> Option<&HarnessMode> {
        self.modes
            .iter()
            .find(|m| m.default)
            .or_else(|| self.modes.first())
    }

    /// The default model: the option flagged `default`, else the first listed,
    /// else `None` (a harness may decline to expose a model enum).
    pub fn default_model(&self) -> Option<&HarnessOption> {
        self.models
            .iter()
            .find(|o| o.default)
            .or_else(|| self.models.first())
    }

    /// The default effort: the option flagged `default`, else the first listed,
    /// else `None`.
    pub fn default_effort(&self) -> Option<&HarnessOption> {
        self.effort
            .iter()
            .find(|o| o.default)
            .or_else(|| self.effort.first())
    }

    /// Display label, falling back to the harness id.
    pub fn display_label(&self) -> &str {
        self.label.as_deref().unwrap_or(&self.name)
    }

    /// The entry path within the harness's catalog subtree (the launch
    /// contract's `exec`, or `harness` by default). Joined under
    /// `/opt/engram/dyn/0/<name>/` to form `argv[0]` (ADR 0062 §3).
    pub fn exec_path(&self) -> &str {
        self.exec.as_deref().unwrap_or(Self::DEFAULT_EXEC)
    }
}

impl HarnessOption {
    /// Display label, falling back to the option id.
    pub fn display_label(&self) -> &str {
        self.label.as_deref().unwrap_or(&self.id)
    }
}

fn validate_options(field: &str, opts: &[HarnessOption]) -> Result<(), String> {
    let mut seen: HashSet<&str> = HashSet::new();
    let mut defaults = 0usize;
    for o in opts {
        if o.id.trim().is_empty() {
            return Err(format!("harness.toml [[{field}]]: id must not be empty"));
        }
        if !seen.insert(o.id.as_str()) {
            return Err(format!("harness.toml [[{field}]]: duplicate id {:?}", o.id));
        }
        if o.default {
            defaults += 1;
        }
        for k in o.env.keys() {
            validate_env_name(k)
                .map_err(|e| format!("harness.toml [[{field}]] {:?} env: {e}", o.id))?;
        }
    }
    if defaults > 1 {
        return Err(format!(
            "harness.toml [[{field}]]: at most one option may be default ({defaults} found)"
        ));
    }
    Ok(())
}

fn validate_modes(modes: &[HarnessMode]) -> Result<(), String> {
    let mut seen: HashSet<&str> = HashSet::new();
    let mut defaults = 0usize;
    for m in modes {
        if m.id.trim().is_empty() {
            return Err("harness.toml [[modes]]: id must not be empty".into());
        }
        if !seen.insert(m.id.as_str()) {
            return Err(format!("harness.toml [[modes]]: duplicate id {:?}", m.id));
        }
        if m.default {
            defaults += 1;
        }
    }
    if defaults > 1 {
        return Err(format!(
            "harness.toml [[modes]]: at most one mode may be default ({defaults} found)"
        ));
    }
    Ok(())
}

/// A POSIX-ish env var name: `[A-Za-z_][A-Za-z0-9_]*` (the same shape the
/// orchestrator validates env-var names against).
fn validate_env_name(name: &str) -> Result<(), String> {
    match name.chars().next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => {
            return Err(format!(
                "invalid env var name {name:?} (must start with a letter or underscore)"
            ));
        }
    }
    if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err(format!(
            "invalid env var name {name:?} (only [A-Za-z0-9_] allowed)"
        ));
    }
    Ok(())
}

fn validate_stable_id(value: &str) -> Result<(), String> {
    if value.is_empty()
        || !value
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        || !value.as_bytes()[0].is_ascii_lowercase()
    {
        return Err(format!(
            "invalid stable id {value:?} (expected [a-z][a-z0-9-]*)"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLAUDE: &str = r#"
name  = "claude"
label = "Claude Code"

[auth]
org_env  = "ANTHROPIC_API_KEY"
user_env = "CLAUDE_CODE_OAUTH_TOKEN"

[[models]]
id = "opus"
label = "Claude Opus 4.8"
default = true
env = { ANTHROPIC_MODEL = "claude-opus-4-8" }

[[models]]
id = "sonnet"
env = { ANTHROPIC_MODEL = "claude-sonnet-4-6" }

[[effort]]
id = "high"
env = { MAX_THINKING_TOKENS = "32000" }
"#;

    #[test]
    fn parses_the_claude_descriptor() {
        let d = HarnessDescriptor::parse(CLAUDE).expect("valid");
        assert_eq!(d.name, "claude");
        assert_eq!(d.display_label(), "Claude Code");
        assert_eq!(d.auth.org_env.as_deref(), Some("ANTHROPIC_API_KEY"));
        assert_eq!(d.auth.user_env.as_deref(), Some("CLAUDE_CODE_OAUTH_TOKEN"));
        assert_eq!(d.models.len(), 2);
        assert_eq!(d.effort.len(), 1);
    }

    #[test]
    fn parses_the_egress_block_and_defaults_empty() {
        let src = r#"
name = "x"
[egress]
allow_hosts = ["api.example.com", "telemetry.example.com"]
allow_host_patterns = ["*.example.dev"]
"#;
        let d = HarnessDescriptor::parse(src).unwrap();
        assert_eq!(
            d.egress.allow_hosts,
            vec!["api.example.com", "telemetry.example.com"]
        );
        assert_eq!(d.egress.allow_host_patterns, vec!["*.example.dev"]);

        // Absent block → empty (older descriptors stay valid).
        let d = HarnessDescriptor::parse(CLAUDE).unwrap();
        assert!(d.egress.is_empty());
    }

    #[test]
    fn parses_auth_hints() {
        let src = r#"
name = "x"
[auth]
user_env = "TOK"
user_env_hint = "Run `claude setup-token`."
org_env = "KEY"
org_env_hint = "Set an org secret KEY."
"#;
        let d = HarnessDescriptor::parse(src).unwrap();
        assert_eq!(
            d.auth.user_env_hint.as_deref(),
            Some("Run `claude setup-token`.")
        );
        assert_eq!(
            d.auth.org_env_hint.as_deref(),
            Some("Set an org secret KEY.")
        );
    }

    #[test]
    fn parses_oauth_and_rejects_two_human_mechanisms() {
        let oauth = HarnessDescriptor::parse(
            r#"
name = "codex"
[auth.user_oauth]
provider = "openai-codex"
delivery = "opaque_bundle"
"#,
        )
        .unwrap();
        assert_eq!(
            oauth.auth.user_oauth,
            Some(HarnessOAuth {
                provider: "openai-codex".into(),
                delivery: OAuthDelivery::OpaqueBundle,
            })
        );

        let err = HarnessDescriptor::parse(
            r#"
name = "bad"
[auth]
user_env = "TOKEN"
[auth.user_oauth]
provider = "openai-codex"
delivery = "opaque_bundle"
"#,
        )
        .unwrap_err();
        assert!(err.contains("mutually exclusive"), "{err}");
    }

    #[test]
    fn default_model_prefers_the_flagged_option() {
        let d = HarnessDescriptor::parse(CLAUDE).unwrap();
        assert_eq!(d.default_model().unwrap().id, "opus");
        // sonnet has no env fallback issues
        assert_eq!(
            d.model("sonnet")
                .unwrap()
                .env
                .get("ANTHROPIC_MODEL")
                .unwrap(),
            "claude-sonnet-4-6"
        );
    }

    #[test]
    fn default_falls_back_to_first_when_none_flagged() {
        let src = r#"
name = "x"
[[models]]
id = "a"
[[models]]
id = "b"
"#;
        let d = HarnessDescriptor::parse(src).unwrap();
        assert_eq!(d.default_model().unwrap().id, "a");
        assert!(d.default_effort().is_none());
    }

    #[test]
    fn round_trips_through_toml() {
        let d = HarnessDescriptor::parse(CLAUDE).unwrap();
        let rendered = toml::to_string(&d).unwrap();
        let again = HarnessDescriptor::parse(&rendered).unwrap();
        assert_eq!(d, again);
    }

    #[test]
    fn rejects_empty_name() {
        let err = HarnessDescriptor::parse(r#"name = """#).unwrap_err();
        assert!(err.contains("name must not be empty"), "{err}");
    }

    #[test]
    fn rejects_bad_env_var_name() {
        let src = r#"
name = "x"
[auth]
org_env = "1BAD"
"#;
        let err = HarnessDescriptor::parse(src).unwrap_err();
        assert!(err.contains("org_env"), "{err}");
        assert!(err.contains("invalid env var name"), "{err}");
    }

    #[test]
    fn rejects_duplicate_option_ids() {
        let src = r#"
name = "x"
[[models]]
id = "dup"
[[models]]
id = "dup"
"#;
        let err = HarnessDescriptor::parse(src).unwrap_err();
        assert!(err.contains("duplicate id"), "{err}");
    }

    #[test]
    fn rejects_two_defaults() {
        let src = r#"
name = "x"
[[effort]]
id = "a"
default = true
[[effort]]
id = "b"
default = true
"#;
        let err = HarnessDescriptor::parse(src).unwrap_err();
        assert!(err.contains("at most one option may be default"), "{err}");
    }

    #[test]
    fn rejects_unknown_fields() {
        let src = r#"
name = "x"
bogus = "nope"
"#;
        assert!(HarnessDescriptor::parse(src).is_err());
    }

    #[test]
    fn exec_path_defaults_and_overrides() {
        let d = HarnessDescriptor::parse(r#"name = "x""#).unwrap();
        assert_eq!(d.exec_path(), "harness");
        assert!(d.args.is_empty());

        let d = HarnessDescriptor::parse(
            r#"
name = "x"
exec = "bin/run"
args = ["--serve", "--quiet"]
"#,
        )
        .unwrap();
        assert_eq!(d.exec_path(), "bin/run");
        assert_eq!(d.args, vec!["--serve", "--quiet"]);
    }

    #[test]
    fn rejects_unsafe_exec_paths() {
        for bad in ["/abs/path", "../escape", "a/../../b", ""] {
            let src = format!("name = \"x\"\nexec = {bad:?}");
            assert!(
                HarnessDescriptor::parse(&src).is_err(),
                "exec {bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn parses_modes_and_defaults_empty() {
        let src = r#"
name = "x"
[[modes]]
id = "default"
label = "Build"
default = true
[[modes]]
id = "plan"
label = "Plan"
"#;
        let d = HarnessDescriptor::parse(src).unwrap();
        assert_eq!(d.modes.len(), 2);
        assert_eq!(d.default_mode().unwrap().id, "default");
        assert_eq!(d.mode("plan").unwrap().display_label(), "Plan");
        assert!(d.mode("bogus").is_none());

        // Absent block → empty (older descriptors stay valid, no affordance).
        let d = HarnessDescriptor::parse(CLAUDE).unwrap();
        assert!(d.modes.is_empty());
        assert!(d.default_mode().is_none());
    }

    #[test]
    fn rejects_duplicate_or_multi_default_modes() {
        let dup = r#"
name = "x"
[[modes]]
id = "plan"
[[modes]]
id = "plan"
"#;
        let err = HarnessDescriptor::parse(dup).unwrap_err();
        assert!(err.contains("duplicate id"), "{err}");

        let two_defaults = r#"
name = "x"
[[modes]]
id = "a"
default = true
[[modes]]
id = "b"
default = true
"#;
        let err = HarnessDescriptor::parse(two_defaults).unwrap_err();
        assert!(err.contains("at most one mode may be default"), "{err}");
    }

    #[test]
    fn rejects_env_on_a_mode() {
        // A mode is a pure declaration (ADR 0107) — an env map is a schema
        // error, not a silent no-op.
        let src = r#"
name = "x"
[[modes]]
id = "plan"
env = { SOME_VAR = "1" }
"#;
        assert!(HarnessDescriptor::parse(src).is_err());
    }
}
