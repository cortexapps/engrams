//! ADR 0027: activate read-only host-mounted bundles into the harness's
//! skill discovery paths.
//!
//! The init shim RO-mounts the fleet-wide `skills` and (opt-in)
//! `playwright` squashfs bundles at `/opt/engram/skills` and
//! `/opt/engram/browser`. Those bundles are *static* — the same on every
//! host. What's *dynamic* is which skills/tools a given session gets, and
//! that's decided here, per session, just before the harness launches,
//! from the durable session env:
//!
//! - `share-file` — always (the ADR-0026 upload token is on every image),
//!   iff the skills bundle mounted.
//! - `create-pull-request` — iff a forge token is present *and* the skills
//!   bundle mounted; also writes `/etc/gitconfig`.
//! - `show-your-work` — iff the playwright bundle mounted; also symlinks the
//!   bundle's `playwright-cli` wrapper onto PATH. The agent drives the
//!   browser via that CLI (bash), so there is no MCP config to wire.
//!
//! This is what replaces the bake-time `inject_share_helpers` /
//! `inject_forge_helpers` (retired): a skill edit now ships fleet-wide by
//! rolling the bundle, no per-image re-bake.
//!
//! **Best-effort.** A missing bundle (or a failed symlink) is recorded as
//! a warning and skipped — it must NEVER fail the session. Universal skill
//! delivery now depends on this engine, so the absence of a bundle can't
//! be allowed to brick a session; the harness simply comes up without the
//! affected skill.
//!
//! `root` is `/` in the FC guest (agentd). The dev `ProcessBackend` calls
//! the same function with the same `/` root because `just bundles` stages
//! the bundles at the real absolute paths on the dev box.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Env var whose presence marks a `[git]`-bound (forge) session — the
/// per-request broker token agentd's forge bridge mints. Set by coord in
/// `AgentSpec.env`/`session_env` only for git-configured images.
const FORGE_TOKEN_ENV: &str = "ENGRAM_FORGE_TOKEN";

/// What `activate` wired up, for logging + tests.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ActivationReport {
    /// Skills that were activated (e.g. `"share-file"`,
    /// `"create-pull-request"`, `"show-your-work"`).
    pub activated: Vec<String>,
    /// Non-fatal problems (bundle absent, symlink failed). The caller
    /// logs these; none of them fail the session.
    pub warnings: Vec<String>,
}

/// Resolve the canonical guest paths under `root`.
struct Layout {
    skills_bundle: PathBuf,  // /opt/engram/skills  (mounted squashfs)
    browser_bundle: PathBuf, // /opt/engram/browser (mounted squashfs)
    agents_skills: PathBuf,  // /root/.agents/skills (harness-agnostic dir)
    claude_skills: PathBuf,  // /root/.claude/skills -> agents_skills
    usr_local_bin: PathBuf,  // /usr/local/bin (on PATH)
    etc_gitconfig: PathBuf,  // /etc/gitconfig
}

impl Layout {
    fn under(root: &Path) -> Self {
        Self {
            skills_bundle: root.join("opt/engram/skills"),
            browser_bundle: root.join("opt/engram/browser"),
            agents_skills: root.join("root/.agents/skills"),
            claude_skills: root.join("root/.claude/skills"),
            usr_local_bin: root.join("usr/local/bin"),
            etc_gitconfig: root.join("etc/gitconfig"),
        }
    }
}

/// Wire whatever bundles are mounted under `root` into the harness's
/// discovery paths, gated by `session_env`. Idempotent (recreates
/// symlinks, overwrites config) so it's safe to call on every resume.
/// Never errors — see the module docs on best-effort behavior.
pub fn activate(root: &Path, session_env: &HashMap<String, String>) -> ActivationReport {
    let mut report = ActivationReport::default();
    let layout = Layout::under(root);

    let skills_mounted = layout.skills_bundle.join("bin/engram-share").exists();
    let browser_mounted = layout.browser_bundle.join("bin/playwright-cli").exists();

    if !skills_mounted && !browser_mounted {
        // Nothing mounted — a plain image with no bundles. Not an error.
        report
            .warnings
            .push("no RO bundles mounted; skills not wired".into());
        return report;
    }

    // The harness scans `~/.claude/skills`; point it at the
    // harness-agnostic `~/.agents/skills` we selectively populate.
    if let Err(e) = ensure_dir(&layout.agents_skills) {
        report
            .warnings
            .push(format!("create {}: {e}", layout.agents_skills.display()));
    }
    if let Err(e) = ensure_symlink(&layout.agents_skills, &layout.claude_skills) {
        report
            .warnings
            .push(format!("link {}: {e}", layout.claude_skills.display()));
    }

    if skills_mounted {
        // share-file: universal (ADR 0026 upload token is every-image).
        wire_skill(
            &layout,
            "share-file",
            &layout.skills_bundle.join("skills/share-file"),
            &[(
                "engram-share",
                &layout.skills_bundle.join("bin/engram-share"),
            )],
            &mut report,
        );

        // create-pull-request: only for a forge-bound session.
        if session_env.contains_key(FORGE_TOKEN_ENV) {
            wire_skill(
                &layout,
                "create-pull-request",
                &layout.skills_bundle.join("skills/create-pull-request"),
                &[("engram-pr", &layout.skills_bundle.join("bin/engram-pr"))],
                &mut report,
            );
            // git's askpass + credential wiring. The askpass binary lives
            // in the bundle; git invokes it by the absolute path below.
            let askpass = layout.skills_bundle.join("bin/git-askpass");
            let gitconfig = render_gitconfig(&askpass);
            if let Err(e) = write_file(&layout.etc_gitconfig, &gitconfig) {
                report
                    .warnings
                    .push(format!("write {}: {e}", layout.etc_gitconfig.display()));
            } else {
                report.activated.push("gitconfig".into());
            }
        }
    } else if session_env.contains_key(FORGE_TOKEN_ENV) {
        report.warnings.push(
            "forge session but skills bundle not mounted; create-pull-request unavailable".into(),
        );
    }

    if browser_mounted {
        // show-your-work skill + the `playwright-cli` wrapper onto PATH. The
        // wrapper bakes in the headless-shell config + runtime env, so the
        // agent drives the browser with plain `playwright-cli` — no MCP
        // config, no per-harness wiring.
        wire_skill(
            &layout,
            "show-your-work",
            &layout.browser_bundle.join("skills/show-your-work"),
            &[(
                "playwright-cli",
                &layout.browser_bundle.join("bin/playwright-cli"),
            )],
            &mut report,
        );
    }

    report
}

/// Symlink a skill dir into `~/.agents/skills/<name>` and link any
/// wrapper binaries onto PATH (`/usr/local/bin/<bin>`). Records the skill
/// as activated iff its source dir exists.
fn wire_skill(
    layout: &Layout,
    name: &str,
    skill_src: &Path,
    bins: &[(&str, &PathBuf)],
    report: &mut ActivationReport,
) {
    if !skill_src.exists() {
        report.warnings.push(format!(
            "skill {name} not in bundle at {}",
            skill_src.display()
        ));
        return;
    }
    let dst = layout.agents_skills.join(name);
    if let Err(e) = ensure_symlink(skill_src, &dst) {
        report.warnings.push(format!("link skill {name}: {e}"));
        return;
    }
    for (bin_name, bin_src) in bins {
        if let Err(e) = ensure_dir(&layout.usr_local_bin) {
            report
                .warnings
                .push(format!("create {}: {e}", layout.usr_local_bin.display()));
            continue;
        }
        let bin_dst = layout.usr_local_bin.join(bin_name);
        if let Err(e) = ensure_symlink(bin_src, &bin_dst) {
            report.warnings.push(format!("link bin {bin_name}: {e}"));
        }
    }
    report.activated.push(name.to_string());
}

fn render_gitconfig(askpass: &Path) -> String {
    format!(
        "# Wired by engram-agentd at session bind (ADR 0027) for a [git]-bound\n\
         # session. git consults the askpass for the password; the username is\n\
         # the GitHub App x-access-token convention; the empty credential.helper\n\
         # forces the askpass on every operation.\n\
         [core]\n\
         \taskPass = {}\n\
         [credential \"https://github.com\"]\n\
         \tusername = x-access-token\n\
         [credential]\n\
         \thelper =\n",
        askpass.display()
    )
}

fn ensure_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)
}

fn write_file(path: &Path, contents: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, contents)
}

/// Force-create a symlink `link -> target`, replacing any existing entry
/// (idempotent across resumes). Creates the parent dir first.
fn ensure_symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    if let Some(parent) = link.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Remove whatever's there (stale symlink/file) so re-activation on a
    // resume is a clean replace.
    let _ = std::fs::remove_file(link);
    std::os::unix::fs::symlink(target, link)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a fake skills + playwright bundle tree under `root` so the
    /// activation has something to wire. Mirrors the bundle layout the
    /// `deploy/bundles/*` recipes produce.
    fn stage_bundles(root: &Path, skills: bool, browser: bool) {
        if skills {
            let b = root.join("opt/engram/skills");
            std::fs::create_dir_all(b.join("bin")).unwrap();
            std::fs::write(b.join("bin/engram-share"), "#!/bin/sh\n").unwrap();
            std::fs::write(b.join("bin/engram-pr"), "#!/bin/sh\n").unwrap();
            std::fs::write(b.join("bin/git-askpass"), "#!/bin/sh\n").unwrap();
            std::fs::create_dir_all(b.join("skills/share-file")).unwrap();
            std::fs::write(b.join("skills/share-file/SKILL.md"), "---\n").unwrap();
            std::fs::create_dir_all(b.join("skills/create-pull-request")).unwrap();
            std::fs::write(b.join("skills/create-pull-request/SKILL.md"), "---\n").unwrap();
        }
        if browser {
            let b = root.join("opt/engram/browser");
            std::fs::create_dir_all(b.join("bin")).unwrap();
            std::fs::write(b.join("bin/playwright-cli"), "#!/bin/sh\n").unwrap();
            std::fs::create_dir_all(b.join("skills/show-your-work")).unwrap();
            std::fs::write(b.join("skills/show-your-work/SKILL.md"), "---\n").unwrap();
        }
    }

    fn env(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn no_bundles_is_a_noop_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let report = activate(dir.path(), &env(&[]));
        assert!(report.activated.is_empty());
        // Surfaced as a warning, never a panic / failure.
        assert!(report.warnings.iter().any(|w| w.contains("no RO bundles")));
    }

    #[test]
    fn skills_only_wires_share_file_not_forge() {
        let dir = tempfile::tempdir().unwrap();
        stage_bundles(dir.path(), true, false);
        let report = activate(dir.path(), &env(&[]));
        assert!(report.activated.contains(&"share-file".to_string()));
        assert!(!report
            .activated
            .contains(&"create-pull-request".to_string()));
        // ~/.claude/skills -> ~/.agents/skills, share-file linked, wrapper on PATH.
        let l = Layout::under(dir.path());
        assert!(l.claude_skills.is_symlink());
        assert!(l.agents_skills.join("share-file").is_symlink());
        assert!(l.usr_local_bin.join("engram-share").is_symlink());
        assert!(!l.usr_local_bin.join("engram-pr").exists());
        assert!(!l.etc_gitconfig.exists());
    }

    #[test]
    fn forge_token_wires_pr_and_gitconfig() {
        let dir = tempfile::tempdir().unwrap();
        stage_bundles(dir.path(), true, false);
        let report = activate(dir.path(), &env(&[("ENGRAM_FORGE_TOKEN", "tok")]));
        assert!(report
            .activated
            .contains(&"create-pull-request".to_string()));
        assert!(report.activated.contains(&"gitconfig".to_string()));
        let l = Layout::under(dir.path());
        assert!(l.agents_skills.join("create-pull-request").is_symlink());
        assert!(l.usr_local_bin.join("engram-pr").is_symlink());
        let gc = std::fs::read_to_string(&l.etc_gitconfig).unwrap();
        assert!(gc.contains("opt/engram/skills/bin/git-askpass"));
    }

    #[test]
    fn browser_bundle_wires_show_your_work_and_playwright_cli() {
        let dir = tempfile::tempdir().unwrap();
        stage_bundles(dir.path(), true, true);
        let report = activate(dir.path(), &env(&[]));
        assert!(report.activated.contains(&"show-your-work".to_string()));
        let l = Layout::under(dir.path());
        // The skill is discoverable and the CLI wrapper is on PATH — no MCP
        // config is written.
        assert!(l.agents_skills.join("show-your-work").is_symlink());
        assert!(l.usr_local_bin.join("playwright-cli").is_symlink());
        assert!(!dir.path().join("root/.mcp.json").exists());
    }

    #[test]
    fn forge_token_without_skills_bundle_degrades_gracefully() {
        let dir = tempfile::tempdir().unwrap();
        stage_bundles(dir.path(), false, true); // browser only, no skills
        let report = activate(dir.path(), &env(&[("ENGRAM_FORGE_TOKEN", "tok")]));
        // No skills bundle -> no create-pull-request, but a clear warning,
        // and the session is NOT failed. The browser skill still wires.
        assert!(!report
            .activated
            .contains(&"create-pull-request".to_string()));
        assert!(report.activated.contains(&"show-your-work".to_string()));
        assert!(report
            .warnings
            .iter()
            .any(|w| w.contains("skills bundle not mounted")));
    }

    #[test]
    fn activation_is_idempotent_across_resumes() {
        let dir = tempfile::tempdir().unwrap();
        stage_bundles(dir.path(), true, true);
        let e = env(&[("ENGRAM_FORGE_TOKEN", "tok")]);
        let first = activate(dir.path(), &e);
        let second = activate(dir.path(), &e);
        assert_eq!(first.activated, second.activated);
        let l = Layout::under(dir.path());
        assert!(l.agents_skills.join("share-file").is_symlink());
        assert!(l.agents_skills.join("show-your-work").is_symlink());
    }
}
