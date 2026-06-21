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
    /// (e.g. `create-pull-request` requires `ENGRAM_FORGE_TOKEN`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requires_env: Option<String>,
}

/// The `mount.json` at the root of each mounted dynamic-slot squashfs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MountManifest {
    /// [`Self::KIND_SKILL`] (wire it) | [`Self::KIND_SENTINEL`]
    /// (reserved-but-unused slot; the guest skips it).
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

    /// The manifest the coordinator's P2 packer generates for an uploaded skill:
    /// a single markdown/file skill (no bins, no env gate, no askpass) whose
    /// content lives under `skills/<name>/` in the squashfs.
    pub fn single_skill(name: impl Into<String>) -> Self {
        Self {
            kind: Self::KIND_SKILL.to_string(),
            skills: vec![SkillEntry {
                name: name.into(),
                ..Default::default()
            }],
            provides_askpass: None,
        }
    }

    /// `true` for a reserved-but-unused slot the guest should skip.
    pub fn is_sentinel(&self) -> bool {
        self.kind == Self::KIND_SENTINEL
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
            { "name": "create-pull-request", "bins": ["bin/engram-pr"],
              "requires_env": "ENGRAM_FORGE_TOKEN" }
          ],
          "provides_askpass": "bin/git-askpass"
        }"#;
        let m: MountManifest = serde_json::from_str(src).unwrap();
        assert_eq!(m.kind, "skill");
        assert_eq!(m.skills.len(), 2);
        assert_eq!(
            m.skills[1].requires_env.as_deref(),
            Some("ENGRAM_FORGE_TOKEN")
        );
        assert_eq!(m.provides_askpass.as_deref(), Some("bin/git-askpass"));
        assert!(!m.is_sentinel());
    }

    #[test]
    fn sentinel_is_skipped() {
        let m: MountManifest = serde_json::from_str(r#"{"kind":"sentinel"}"#).unwrap();
        assert!(m.is_sentinel());
        assert!(m.skills.is_empty());
    }
}
