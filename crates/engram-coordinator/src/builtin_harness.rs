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
        // A built-in is never in the catalog: the name is resolved from here.
        assert!(builtin("definitely-not-a-builtin").is_none());
    }

    #[test]
    fn embedded_codex_descriptor_is_valid() {
        let b = builtin("codex").expect("codex is built-in");
        let d = b.descriptor().expect("embedded codex harness.toml parses");
        assert_eq!(d.name, "codex");
        assert_eq!(d.auth.user_env.as_deref(), Some("CODEX_ACCESS_TOKEN"));
        assert_eq!(d.auth.org_env.as_deref(), Some("CODEX_API_KEY"));
        assert_eq!(b.stamp_key, "harness-codex");
    }
}
