//! ADR 0055: the `mount.json` schema carried at the root of every dynamic-mount
//! squashfs.
//!
//! This is the contract between the **producers** and the **consumer**:
//!
//! - Producers write a `mount.json` describing what a mounted slot carries:
//!   the `deploy/bundles/*` bake recipes (hand-authored JSON) and the
//!   coordinator's P2 `skill_pack`, which *generates* one for an uploaded skill.
//! - The consumer — `engram-session-bundles::activate()` in the FC guest —
//!   reads each mounted slot's `mount.json` to learn which skills to wire onto
//!   the harness's discovery paths.
//!
//! Kept serde-only (no `engram-core`, which pulls in sqlx/tonic) so it stays
//! cheap to link into the lean in-guest `agentd`.

use serde::{Deserialize, Serialize};

/// One skill a mounted bundle carries.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillEntry {
    /// Skill dir name under `skills/<name>` + the harness discovery name.
    pub name: String,
    /// Bundle-relative wrapper paths to symlink onto PATH (basename = the PATH
    /// command name). Empty for markdown/file skills (the P2 upload shape).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bins: Vec<String>,
    /// Gate: skip this skill unless this env key is present in `session_env`
    /// (e.g. the ADR 0058 `integrations` skill requires `ENGRAM_CLI_INTEGRATIONS`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requires_env: Option<String>,
}

/// The `mount.json` at the root of each mounted dynamic-slot squashfs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MountManifest {
    /// [`Self::KIND_SKILL`] (wire it) | [`Self::KIND_SENTINEL`]
    /// (reserved-but-unused slot; the guest skips it) | [`Self::KIND_HARNESS`]
    /// (the ADR 0062 harness catalog on `dyn_0`; the guest skips it — the
    /// harness is `exec`'d by the coordinator's `argv`, not activated here).
    pub kind: String,
    /// Skills this bundle carries.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skills: Vec<SkillEntry>,
    /// Bundle-relative path to a git askpass binary, if this bundle ships one
    /// (wired into `/etc/gitconfig` for forge sessions).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provides_askpass: Option<String>,
}

impl MountManifest {
    /// A bundle that carries real skills to wire.
    pub const KIND_SKILL: &'static str = "skill";
    /// The tiny placeholder every reserved-but-unused slot carries at capture.
    pub const KIND_SENTINEL: &'static str = "sentinel";
    /// The ADR 0062 harness catalog squashfs (mounted on `dyn_0`). The guest
    /// skips it during `activate()` — every registered harness lives under
    /// `<name>/` in the squashfs and is `exec`'d via the coordinator-built
    /// `argv[0] = /opt/engram/dyn/0/<name>/<exec>`, never wired as a skill.
    pub const KIND_HARNESS: &'static str = "harness";

    /// The `mount.json` written at the root of the harness catalog squashfs
    /// (ADR 0062 §5). It declares only the kind — the harnesses are
    /// argv-selected, so the guest needs no per-harness wiring metadata here.
    pub fn harness_catalog() -> Self {
        Self {
            kind: Self::KIND_HARNESS.to_string(),
            skills: Vec::new(),
            provides_askpass: None,
        }
    }

    /// The manifest the coordinator's P2 packer generates for an uploaded skill:
    /// a single markdown/file skill (no bins, no env gate, no askpass) whose
    /// content lives under `skills/<name>/` in the squashfs.
    pub fn single_skill(name: impl Into<String>) -> Self {
        Self::single_skill_with_bins(name, Vec::new())
    }

    /// As [`Self::single_skill`], but the skill also contributes PATH binaries
    /// (ADR 0058 uploaded-binary arm). `bins` are squashfs-root-relative paths —
    /// `skills/<name>/<bin>` — that `activate()` symlinks onto PATH. No env gate:
    /// an uploaded binary bundle is mounted only when a connector referencing it is
    /// granted, so `selected_skills` membership *is* the gate. Empty `bins`
    /// serializes byte-for-byte like [`Self::single_skill`] (bins are skipped).
    pub fn single_skill_with_bins(name: impl Into<String>, bins: Vec<String>) -> Self {
        Self {
            kind: Self::KIND_SKILL.to_string(),
            skills: vec![SkillEntry {
                name: name.into(),
                bins,
                requires_env: None,
            }],
            provides_askpass: None,
        }
    }

    /// `true` for a reserved-but-unused slot the guest should skip.
    pub fn is_sentinel(&self) -> bool {
        self.kind == Self::KIND_SENTINEL
    }

    /// `true` for the harness catalog slot the guest should skip during
    /// `activate()` (the harness is `exec`'d via `argv`, not wired as a skill).
    pub fn is_harness(&self) -> bool {
        self.kind == Self::KIND_HARNESS
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_skill_serializes_to_the_activate_shape() {
        let m = MountManifest::single_skill("my-linter");
        let json = serde_json::to_string(&m).unwrap();
        // Markdown skill: kind + one named skill, nothing else (empty bins and
        // absent optionals are skipped, matching the hand-authored bundles).
        assert_eq!(json, r#"{"kind":"skill","skills":[{"name":"my-linter"}]}"#);
    }

    #[test]
    fn round_trips_a_builtin_with_bins_and_gate() {
        let src = r#"{
          "kind": "skill",
          "skills": [
            { "name": "share-file", "bins": ["bin/engram-share"] },
            { "name": "integrations", "bins": ["bin/engrams-integrations"],
              "requires_env": "ENGRAM_CLI_INTEGRATIONS" }
          ],
          "provides_askpass": "bin/git-askpass"
        }"#;
        let m: MountManifest = serde_json::from_str(src).unwrap();
        assert_eq!(m.kind, "skill");
        assert_eq!(m.skills.len(), 2);
        assert_eq!(
            m.skills[1].requires_env.as_deref(),
            Some("ENGRAM_CLI_INTEGRATIONS")
        );
        assert_eq!(m.provides_askpass.as_deref(), Some("bin/git-askpass"));
        assert!(!m.is_sentinel());
    }

    #[test]
    fn single_skill_with_bins_carries_path_binaries() {
        let m = MountManifest::single_skill_with_bins(
            "mytool",
            vec!["skills/mytool/bin/mytool".to_string()],
        );
        let json = serde_json::to_string(&m).unwrap();
        assert_eq!(
            json,
            r#"{"kind":"skill","skills":[{"name":"mytool","bins":["skills/mytool/bin/mytool"]}]}"#
        );
        // Empty bins is byte-identical to single_skill (the markdown shape).
        assert_eq!(
            serde_json::to_string(&MountManifest::single_skill_with_bins("x", vec![])).unwrap(),
            serde_json::to_string(&MountManifest::single_skill("x")).unwrap(),
        );
    }

    #[test]
    fn sentinel_is_skipped() {
        let m: MountManifest = serde_json::from_str(r#"{"kind":"sentinel"}"#).unwrap();
        assert!(m.is_sentinel());
        assert!(m.skills.is_empty());
    }

    #[test]
    fn harness_catalog_serializes_to_kind_only_and_is_skipped() {
        let m = MountManifest::harness_catalog();
        assert_eq!(serde_json::to_string(&m).unwrap(), r#"{"kind":"harness"}"#);
        assert!(m.is_harness());
        assert!(!m.is_sentinel());
        // Round-trips, and the guest recognizes it as the harness catalog.
        let back: MountManifest = serde_json::from_str(r#"{"kind":"harness"}"#).unwrap();
        assert!(back.is_harness());
        assert!(back.skills.is_empty());
    }
}
