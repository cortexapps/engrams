//! Built-in harnesses (ADR 0062).
//!
//! A built-in harness ships two ways at once, both exactly like a built-in skill:
//!   - its RO squashfs is baked into the FC-host image by node-assets and shows up
//!     in the host's `current_bundles` stamp under [`BuiltinHarness::stamp_key`];
//!   - its `harness.toml` descriptor is embedded HERE in the coordinator binary.
//!
//! So a fresh deployment runs the built-in with no registration, no admin step,
//! and no broken window after the image-harness clean break. The `harness_catalog`
//! (see [`crate::harness_catalog`]) is only for *custom* uploaded harnesses.

use engram_core::types::harness::HarnessDescriptor;

/// The committed Claude descriptor, embedded so the coordinator can resolve the
/// built-in without a catalog row or an OCI pull. Single source of truth with the
/// `harness.toml` shipped inside the harness-claude squashfs.
const CLAUDE_HARNESS_TOML: &str = include_str!("../../../deploy/harness-claude/harness.toml");
const CODEX_HARNESS_TOML: &str = include_str!("../../../deploy/harness-codex/harness.toml");

/// A harness the platform ships built-in.
pub struct BuiltinHarness {
    /// Logical name — the `CreateSessionRequest.harness` value.
    pub name: &'static str,
    /// Its `harness.toml` descriptor source.
    pub descriptor_toml: &'static str,
    /// The `current_bundles` stamp key its squashfs is staged under by node-assets
    /// — namespaced `harness-<name>` so it can never collide with a skill name.
    pub stamp_key: &'static str,
}

impl BuiltinHarness {
    /// Parse the embedded descriptor.
    pub fn descriptor(&self) -> Result<HarnessDescriptor, String> {
        HarnessDescriptor::parse(self.descriptor_toml)
    }
}

static BUILTINS: &[BuiltinHarness] = &[
    BuiltinHarness {
        name: "claude",
        descriptor_toml: CLAUDE_HARNESS_TOML,
        stamp_key: "harness-claude",
    },
    BuiltinHarness {
        name: "codex",
        descriptor_toml: CODEX_HARNESS_TOML,
        stamp_key: "harness-codex",
    },
];

/// Every built-in harness, in listing order.
pub fn builtin_harnesses() -> &'static [BuiltinHarness] {
    BUILTINS
}

/// The built-in harness with this name, if any.
pub fn builtin(name: &str) -> Option<&'static BuiltinHarness> {
    BUILTINS.iter().find(|h| h.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_claude_descriptor_is_valid() {
        let b = builtin("claude").expect("claude is built-in");
        let d = b.descriptor().expect("embedded claude harness.toml parses");
        assert_eq!(d.name, "claude");
        assert_eq!(d.exec_path(), "harness");
        assert_eq!(b.stamp_key, "harness-claude");
        // ADR 0063 addendum: the harness declares its own model-API egress —
        // sessions merge these into their allowlist at create. An empty block
        // here would strangle every deny-default session's harness.
        assert_eq!(
            d.egress.allow_hosts,
            vec!["api.anthropic.com", "statsig.anthropic.com"]
        );
        // A built-in is never in the catalog: the name is resolved from here.
        assert!(builtin("definitely-not-a-builtin").is_none());
    }

    #[test]
    fn embedded_codex_descriptor_is_valid() {
        let b = builtin("codex").expect("codex is built-in");
        let d = b.descriptor().expect("embedded codex harness.toml parses");
        assert_eq!(d.name, "codex");
        assert!(d.auth.user_env.is_none());
        assert_eq!(
            d.auth
                .user_oauth
                .as_ref()
                .map(|oauth| oauth.provider.as_str()),
            Some("openai-codex")
        );
        assert_eq!(d.auth.org_env.as_deref(), Some("CODEX_API_KEY"));
        assert_eq!(d.egress.allow_hosts, vec!["api.openai.com", "chatgpt.com"]);
        assert_eq!(
            d.models
                .iter()
                .map(|model| model.id.as_str())
                .collect::<Vec<_>>(),
            vec![
                "gpt-5.6-sol",
                "gpt-5.6-terra",
                "gpt-5.6-luna",
                "gpt-5.5",
                "gpt-5.4",
                "gpt-5.4-mini",
                "gpt-5.3-codex-spark",
            ]
        );
        assert_eq!(
            d.models
                .iter()
                .find(|model| model.default)
                .map(|model| model.id.as_str()),
            Some("gpt-5.6-sol")
        );
        assert_eq!(b.stamp_key, "harness-codex");
    }
}
