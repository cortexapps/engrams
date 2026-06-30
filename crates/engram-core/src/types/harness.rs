//! The harness descriptor (`harness.toml`) — ADR 0063.
//!
//! A harness-agnostic declaration each harness ships inside its bundle,
//! describing its *environment contract*: the credential env-var names (an
//! org/programmatic one + an optional user/interactive one) and the model /
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
}

/// Credential env-var names. A credential is one var, so these are plain
/// strings (unlike the per-option model/effort env maps).
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
        validate_options("models", &self.models)?;
        validate_options("effort", &self.effort)?;
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
}
