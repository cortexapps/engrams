//! ADR 0027 → 0055: activate read-only host-mounted skill bundles into the
//! harness's skill discovery paths.
//!
//! ADR 0055: the init shim RO-mounts each reserved dynamic slot at
//! `/opt/engram/dyn/<i>`. Unused slots carry a sentinel (`mount.json`
//! `{"kind":"sentinel"}`) and are skipped; a per-session create `patch_drive`s
//! the profile-selected skills into the other slots. Each skill bundle ships a
//! `mount.json` declaring the skills it carries, the wrapper binaries to put on
//! PATH, and any per-skill env gate. This function scans those slots, reads
//! each manifest, and wires the declared skills — replacing the old hardcoded
//! `skills`/`browser` probe (and, before that, the bake-time injectors).
//!
//! Gating that depends on session state stays, now declared in `mount.json`:
//! - a skill with `requires_env` (e.g. the ADR 0058 `integrations` discovery
//!   skill requires `ENGRAM_CLI_INTEGRATIONS`) is wired only when that env key
//!   is present;
//! - `/etc/gitconfig` gets a `[user]` block whenever an initiator is known
//!   (ADR 0031 committer attribution, every session), and the askpass +
//!   credential blocks only for a forge-bound session whose mounted skills
//!   shipped an askpass (`provides_askpass`).
//!
//! **Best-effort.** A missing/garbled bundle (or a failed symlink) is recorded
//! as a warning and skipped — it must NEVER fail the session.
//!
//! `root` is `/` in the FC guest (agentd). The dev `ProcessBackend` calls the
//! same function with the same `/` root because it symlinks the dev bundles at
//! the real absolute paths.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Env var whose presence marks a `[git]`-bound (forge) session — the
/// per-request broker token agentd's forge bridge mints. Set by coord in
/// `AgentSpec.env`/`session_env` only for git-configured images.
const FORGE_TOKEN_ENV: &str = "ENGRAM_FORGE_TOKEN";

/// What `activate` wired up, for logging + tests.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ActivationReport {
    /// Skills that were activated (e.g. `"share-file"`, `"integrations"`,
    /// `"show-your-work"`).
    pub activated: Vec<String>,
    /// Non-fatal problems (bundle absent/garbled, symlink failed). The caller
    /// logs these; none of them fail the session.
    pub warnings: Vec<String>,
}

// ADR 0055: the `mount.json` schema (`MountManifest` / `SkillEntry`) is the
// shared contract in `engram-mount-manifest` — produced by the bake recipes and
// the coordinator's P2 skill packer, consumed here. Kept in a serde-only crate
// so it stays cheap to link into the in-guest agentd.
use engram_mount_manifest::MountManifest;

/// Resolve the canonical guest paths under `root`.
struct Layout {
    dyn_root: PathBuf,      // /opt/engram/dyn (reserved-slot mount points)
    agents_skills: PathBuf, // /root/.agents/skills (harness-agnostic dir)
    claude_skills: PathBuf, // /root/.claude/skills -> agents_skills
    usr_local_bin: PathBuf, // /usr/local/bin (on PATH)
    etc_gitconfig: PathBuf, // /etc/gitconfig
    run_agentd: PathBuf,    // /run/engram/engram-agentd (the exec'd tmpfs copy)
}

impl Layout {
    fn under(root: &Path) -> Self {
        Self {
            dyn_root: root.join("opt/engram/dyn"),
            agents_skills: root.join("root/.agents/skills"),
            claude_skills: root.join("root/.claude/skills"),
            usr_local_bin: root.join("usr/local/bin"),
            etc_gitconfig: root.join("etc/gitconfig"),
            run_agentd: root.join("run/engram/engram-agentd"),
        }
    }
}

/// Wire whatever skill bundles are mounted under `root`'s reserved slots into
/// the harness's discovery paths, gated by `session_env`. Idempotent (recreates
/// symlinks, overwrites config) so it's safe to call on every resume. Never
/// errors — see the module docs on best-effort behavior.
pub fn activate(root: &Path, session_env: &HashMap<String, String>) -> ActivationReport {
    let mut report = ActivationReport::default();
    let layout = Layout::under(root);

    // ADR 0080 moved agentd out of the baked rootfs: stage-1 init copies it
    // out of its bundle slot to tmpfs (`/run/engram/engram-agentd`) and execs
    // the copy as PID 1 — so nothing puts `engram-agentd` on PATH any more,
    // which broke every wrapper that resolves it there (the skills bundle's
    // `git-askpass` / `engram-share` both `exec engram-agentd <subcommand>`).
    // Restore the contract by linking the tmpfs copy onto /usr/local/bin.
    // Gated on the copy existing: the dev ProcessBackend runs agentd as a
    // plain host process with no /run/engram staging, and must never touch
    // the machine's real /usr/local/bin. Before the bundle scan on purpose —
    // agentd belongs on PATH even for a plain image with no skills mounted.
    if layout.run_agentd.exists() {
        let link = layout.usr_local_bin.join("engram-agentd");
        if let Err(e) = ensure_symlink(&layout.run_agentd, &link) {
            report
                .warnings
                .push(format!("link engram-agentd onto PATH: {e}"));
        }
    }

    // Collect the mounted skill bundles from the reserved slots, skipping
    // sentinels (reserved-but-unused) and unmounted/empty slots.
    let mut bundles: Vec<(PathBuf, MountManifest)> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&layout.dyn_root) {
        for entry in entries.flatten() {
            let slot = entry.path();
            let manifest_path = slot.join("mount.json");
            let bytes = match std::fs::read(&manifest_path) {
                Ok(b) => b,
                Err(_) => continue, // unmounted slot / no manifest
            };
            match serde_json::from_slice::<MountManifest>(&bytes) {
                // Reserved-but-unused slot, or the ADR 0062 harness catalog on
                // dyn_0 (the harness is exec'd via the coordinator's argv, not
                // wired as a skill here).
                Ok(m) if m.is_sentinel() || m.is_harness() => {}
                Ok(m) => bundles.push((slot, m)),
                Err(e) => report.warnings.push(format!(
                    "bad mount.json at {}: {e}",
                    manifest_path.display()
                )),
            }
        }
    }

    if bundles.is_empty() {
        // Plain image with no skills selected. Not an error.
        report
            .warnings
            .push("no skill bundles mounted; skills not wired".into());
        return report;
    }

    // The harness scans `~/.claude/skills`; point it at the harness-agnostic
    // `~/.agents/skills` we selectively populate.
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

    // Wire each bundle's skills; remember the askpass binary if any bundle
    // ships one (for the gitconfig credential wiring below).
    let mut askpass: Option<PathBuf> = None;
    for (slot, manifest) in &bundles {
        if let Some(rel) = &manifest.provides_askpass {
            askpass = Some(slot.join(rel));
        }
        // Bundle-level bins (ADR 0065): launchers a capability bundle puts on
        // PATH without being a user-facing agent skill. Wired independently of
        // the per-skill loop below — no skill dir, no `~/.agents/skills` entry.
        wire_bins(&layout, slot, &manifest.bins, &mut report);
        for skill in &manifest.skills {
            if let Some(req) = &skill.requires_env {
                if !session_env.contains_key(req) {
                    report.warnings.push(format!(
                        "skill {} requires {} (absent in session env); skipped",
                        skill.name, req
                    ));
                    continue;
                }
            }
            let skill_src = slot.join("skills").join(&skill.name);
            let bins: Vec<(String, PathBuf)> = skill
                .bins
                .iter()
                .filter_map(|b| {
                    Path::new(b)
                        .file_name()
                        .and_then(|f| f.to_str())
                        .map(|f| (f.to_string(), slot.join(b)))
                })
                .collect();
            wire_skill(&layout, &skill.name, &skill_src, &bins, &mut report);
        }
    }

    // ADR 0031 + 0027: `/etc/gitconfig`. The `[user]` block (committer
    // attribution) applies to EVERY session with a known initiator; the askpass
    // + credential blocks only to a forge-bound session whose mounted skills
    // shipped an askpass.
    let user_email = session_env.get("ENGRAM_USER_EMAIL").map(String::as_str);
    let user_name = session_env.get("ENGRAM_USER_NAME").map(String::as_str);
    let askpass = askpass.filter(|_| session_env.contains_key(FORGE_TOKEN_ENV));
    if user_email.is_some() || askpass.is_some() {
        let gitconfig = render_gitconfig(askpass.as_deref(), user_email, user_name);
        if let Err(e) = write_file(&layout.etc_gitconfig, &gitconfig) {
            report
                .warnings
                .push(format!("write {}: {e}", layout.etc_gitconfig.display()));
        } else {
            report.activated.push("gitconfig".into());
        }
    }

    report
}

/// Symlink a skill dir into `~/.agents/skills/<name>` and link any wrapper
/// binaries onto PATH (`/usr/local/bin/<bin>`). Records the skill as activated
/// iff its source dir exists.
fn wire_skill(
    layout: &Layout,
    name: &str,
    skill_src: &Path,
    bins: &[(String, PathBuf)],
    report: &mut ActivationReport,
) {
    if !skill_src.exists() {
        report.warnings.push(format!(
            "skill {name} declared in mount.json but missing at {}",
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

/// Symlink bundle-level bins (not tied to a skill) onto `/usr/local/bin`.
/// Basename of each path becomes the PATH command. No skill dir is required
/// and nothing is registered under `~/.agents/skills`.
fn wire_bins(layout: &Layout, slot: &Path, bins: &[String], report: &mut ActivationReport) {
    for rel in bins {
        let src = slot.join(rel);
        let Some(name) = Path::new(rel).file_name().and_then(|f| f.to_str()) else {
            report.warnings.push(format!("bad bundle bin path {rel}"));
            continue;
        };
        if !src.exists() {
            report
                .warnings
                .push(format!("bundle bin {rel} missing at {}", src.display()));
            continue;
        }
        if let Err(e) = ensure_dir(&layout.usr_local_bin) {
            report
                .warnings
                .push(format!("create {}: {e}", layout.usr_local_bin.display()));
            continue;
        }
        if let Err(e) = ensure_symlink(&src, &layout.usr_local_bin.join(name)) {
            report.warnings.push(format!("link bundle bin {name}: {e}"));
        }
    }
}

/// Render `/etc/gitconfig`. The user block (ADR 0031 committer attribution) is
/// emitted whenever `user_email` is set. The askpass + credential-helper
/// blocks (ADR 0027 forge credential wiring) are emitted only when `askpass`
/// is supplied (a forge-bound session whose mounted skills shipped an askpass).
fn render_gitconfig(
    askpass: Option<&Path>,
    user_email: Option<&str>,
    user_name: Option<&str>,
) -> String {
    let mut s = String::from("# Wired by engram-agentd at session bind (ADR 0027/0031).\n");
    if let Some(email) = user_email {
        s.push_str("[user]\n");
        s.push_str(&format!("\temail = {email}\n"));
        if let Some(name) = user_name {
            s.push_str(&format!("\tname = {name}\n"));
        }
    }
    if let Some(askpass) = askpass {
        // git consults the askpass for the password; the username is the
        // GitHub App x-access-token convention; the empty credential.helper
        // forces the askpass on every operation.
        s.push_str(&format!(
            "[core]\n\
             \taskPass = {}\n\
             [credential \"https://github.com\"]\n\
             \tusername = x-access-token\n\
             [credential]\n\
             \thelper =\n",
            askpass.display()
        ));
    }
    s
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

    /// Stage a fake `skills` bundle at reserved slot `n` with a `mount.json`,
    /// mirroring what `deploy/bundles/skills` produces.
    fn stage_skills_slot(root: &Path, n: usize) {
        let b = root.join(format!("opt/engram/dyn/{n}"));
        std::fs::create_dir_all(b.join("bin")).unwrap();
        for bin in ["engram-share", "git-askpass"] {
            std::fs::write(b.join("bin").join(bin), "#!/bin/sh\n").unwrap();
        }
        std::fs::create_dir_all(b.join("skills/share-file")).unwrap();
        std::fs::write(b.join("skills/share-file/SKILL.md"), "---\n").unwrap();
        std::fs::write(
            b.join("mount.json"),
            r#"{"kind":"skill",
                "skills":[
                  {"name":"share-file","bins":["bin/engram-share"]}
                ],
                "provides_askpass":"bin/git-askpass"}"#,
        )
        .unwrap();
    }

    /// Stage a fake `browser` bundle at reserved slot `n`.
    fn stage_browser_slot(root: &Path, n: usize) {
        let b = root.join(format!("opt/engram/dyn/{n}"));
        std::fs::create_dir_all(b.join("bin")).unwrap();
        std::fs::write(b.join("bin/playwright-cli"), "#!/bin/sh\n").unwrap();
        std::fs::create_dir_all(b.join("skills/show-your-work")).unwrap();
        std::fs::write(b.join("skills/show-your-work/SKILL.md"), "---\n").unwrap();
        std::fs::write(
            b.join("mount.json"),
            r#"{"kind":"skill","skills":[{"name":"show-your-work","bins":["bin/playwright-cli"]}]}"#,
        )
        .unwrap();
    }

    /// Stage a sentinel at reserved slot `n` (reserved-but-unused).
    fn stage_sentinel_slot(root: &Path, n: usize) {
        let b = root.join(format!("opt/engram/dyn/{n}"));
        std::fs::create_dir_all(&b).unwrap();
        std::fs::write(b.join("mount.json"), r#"{"kind":"sentinel"}"#).unwrap();
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
        // Reserved slots all sentinel — nothing to wire.
        stage_sentinel_slot(dir.path(), 0);
        stage_sentinel_slot(dir.path(), 1);
        let report = activate(dir.path(), &env(&[]));
        assert!(report.activated.is_empty());
        assert!(report
            .warnings
            .iter()
            .any(|w| w.contains("no skill bundles")));
    }

    #[test]
    fn skills_only_wires_share_file_not_forge() {
        let dir = tempfile::tempdir().unwrap();
        stage_skills_slot(dir.path(), 0);
        stage_sentinel_slot(dir.path(), 1);
        let report = activate(dir.path(), &env(&[]));
        assert!(report.activated.contains(&"share-file".to_string()));
        let l = Layout::under(dir.path());
        assert!(l.claude_skills.is_symlink());
        assert!(l.agents_skills.join("share-file").is_symlink());
        assert!(l.usr_local_bin.join("engram-share").is_symlink());
        assert!(!l.etc_gitconfig.exists());
    }

    #[test]
    fn forge_token_wires_gitconfig_askpass() {
        let dir = tempfile::tempdir().unwrap();
        stage_skills_slot(dir.path(), 0);
        let report = activate(dir.path(), &env(&[("ENGRAM_FORGE_TOKEN", "tok")]));
        // ADR 0058: the baked PR skill is retired. Git push stays brokered via
        // the skills bundle's askpass + gitconfig (gated on ENGRAM_FORGE_TOKEN);
        // PRs now open via `gh` from the integrations-cli bundle, not a skill.
        assert!(report.activated.contains(&"gitconfig".to_string()));
        let l = Layout::under(dir.path());
        let gc = std::fs::read_to_string(&l.etc_gitconfig).unwrap();
        assert!(gc.contains("bin/git-askpass"));
        assert!(gc.contains("x-access-token"));
    }

    #[test]
    fn user_email_writes_gitconfig_user_block_without_forge() {
        let dir = tempfile::tempdir().unwrap();
        stage_skills_slot(dir.path(), 0);
        let report = activate(
            dir.path(),
            &env(&[
                ("ENGRAM_USER_EMAIL", "ada@example.com"),
                ("ENGRAM_USER_NAME", "Ada Lovelace"),
            ]),
        );
        assert!(report.activated.contains(&"gitconfig".to_string()));
        let l = Layout::under(dir.path());
        let gc = std::fs::read_to_string(&l.etc_gitconfig).unwrap();
        assert!(gc.contains("[user]"));
        assert!(gc.contains("email = ada@example.com"));
        assert!(gc.contains("name = Ada Lovelace"));
        // No forge token → no askpass / credential wiring.
        assert!(!gc.contains("askPass"));
        assert!(!gc.contains("x-access-token"));
    }

    #[test]
    fn forge_and_user_writes_both_blocks() {
        let dir = tempfile::tempdir().unwrap();
        stage_skills_slot(dir.path(), 0);
        let report = activate(
            dir.path(),
            &env(&[
                ("ENGRAM_FORGE_TOKEN", "tok"),
                ("ENGRAM_USER_EMAIL", "ada@example.com"),
                ("ENGRAM_USER_NAME", "Ada"),
            ]),
        );
        assert!(report.activated.contains(&"gitconfig".to_string()));
        let l = Layout::under(dir.path());
        let gc = std::fs::read_to_string(&l.etc_gitconfig).unwrap();
        assert!(gc.contains("[user]") && gc.contains("ada@example.com"));
        assert!(gc.contains("git-askpass") && gc.contains("x-access-token"));
    }

    #[test]
    fn browser_slot_wires_show_your_work_and_playwright_cli() {
        let dir = tempfile::tempdir().unwrap();
        stage_skills_slot(dir.path(), 0);
        stage_browser_slot(dir.path(), 1);
        let report = activate(dir.path(), &env(&[]));
        assert!(report.activated.contains(&"show-your-work".to_string()));
        let l = Layout::under(dir.path());
        assert!(l.agents_skills.join("show-your-work").is_symlink());
        assert!(l.usr_local_bin.join("playwright-cli").is_symlink());
        assert!(!dir.path().join("root/.mcp.json").exists());
    }

    #[test]
    fn browser_only_with_forge_token_still_wires_show_your_work() {
        // A profile that selected only the browser skill (not the skills
        // bundle) + a forge token: no `provides_askpass` bundle is mounted, so
        // there's nothing forge-gated to wire — show-your-work still works, no
        // failure. (The retired PR skill used to be the forge-gated entry here.)
        let dir = tempfile::tempdir().unwrap();
        stage_browser_slot(dir.path(), 0);
        let report = activate(dir.path(), &env(&[("ENGRAM_FORGE_TOKEN", "tok")]));
        assert!(report.activated.contains(&"show-your-work".to_string()));
    }

    #[test]
    fn sentinel_only_slots_are_skipped_with_a_warning() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..12 {
            stage_sentinel_slot(dir.path(), i);
        }
        let report = activate(dir.path(), &env(&[("ENGRAM_FORGE_TOKEN", "tok")]));
        assert!(report.activated.is_empty());
        assert!(report
            .warnings
            .iter()
            .any(|w| w.contains("no skill bundles")));
    }

    #[test]
    fn activate_wires_bundle_level_bins_without_a_skill() {
        // ADR 0065: the `browser` bundle ships a launcher on PATH but is NOT a
        // user-facing agent skill — its mount.json is the flat shape
        // {"kind":"skill","bins":["bin/engram-browser"]}. The bin must land on
        // PATH with no phantom agent skill registered and no warning.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let slot = root.join("opt/engram/dyn/0");
        std::fs::create_dir_all(slot.join("bin")).unwrap();
        std::fs::write(slot.join("bin/engram-browser"), b"#!/bin/sh\n").unwrap();
        std::fs::write(
            slot.join("mount.json"),
            br#"{"kind":"skill","bins":["bin/engram-browser"]}"#,
        )
        .unwrap();

        let report = activate(root, &HashMap::new());

        let l = Layout::under(root);
        let link = l.usr_local_bin.join("engram-browser");
        assert!(
            link.is_symlink(),
            "engram-browser must be symlinked onto PATH"
        );
        // No phantom agent skill registered under ~/.agents/skills.
        let phantom = l.agents_skills.exists()
            && std::fs::read_dir(&l.agents_skills)
                .map(|d| d.count() > 0)
                .unwrap_or(false);
        assert!(!phantom, "bundle-level bins must NOT create an agent skill");
        assert!(report
            .warnings
            .iter()
            .all(|w| !w.contains("engram-browser")));
    }

    /// Stage the ADR 0080 tmpfs agentd copy (`/run/engram/engram-agentd`)
    /// the stage-1 init leaves behind in a real guest.
    fn stage_run_agentd(root: &Path) {
        let run = root.join("run/engram");
        std::fs::create_dir_all(&run).unwrap();
        std::fs::write(run.join("engram-agentd"), b"\x7fELF").unwrap();
    }

    #[test]
    fn agentd_tmpfs_copy_is_linked_onto_path() {
        // ADR 0080 regression: `git-askpass`/`engram-share` exec
        // `engram-agentd` via PATH, but the binary now lives only at the
        // tmpfs copy init exec'd. activate() must restore the PATH contract.
        let dir = tempfile::tempdir().unwrap();
        stage_run_agentd(dir.path());
        stage_skills_slot(dir.path(), 0);
        activate(dir.path(), &env(&[]));
        let l = Layout::under(dir.path());
        let link = l.usr_local_bin.join("engram-agentd");
        assert!(link.is_symlink(), "engram-agentd must be on PATH");
        assert_eq!(std::fs::read_link(&link).unwrap(), l.run_agentd);
    }

    #[test]
    fn agentd_path_link_lands_even_with_no_skill_bundles() {
        // A plain image with every slot sentinel still needs agentd on PATH
        // (e.g. a later-mounted bundle's wrapper, or a raw `engram-agentd`
        // invocation from an exec) — the link must precede the
        // no-bundles early return.
        let dir = tempfile::tempdir().unwrap();
        stage_run_agentd(dir.path());
        stage_sentinel_slot(dir.path(), 0);
        let report = activate(dir.path(), &env(&[]));
        assert!(report
            .warnings
            .iter()
            .any(|w| w.contains("no skill bundles")));
        let l = Layout::under(dir.path());
        assert!(l.usr_local_bin.join("engram-agentd").is_symlink());
    }

    #[test]
    fn no_agentd_staging_means_no_path_link_and_no_warning() {
        // Dev ProcessBackend: agentd runs as a host process, /run/engram is
        // never staged — activate() must not create the link (it would point
        // at nothing) and must not warn (this is the normal dev shape).
        let dir = tempfile::tempdir().unwrap();
        stage_skills_slot(dir.path(), 0);
        let report = activate(dir.path(), &env(&[]));
        let l = Layout::under(dir.path());
        assert!(!l.usr_local_bin.join("engram-agentd").exists());
        assert!(report.warnings.iter().all(|w| !w.contains("engram-agentd")));
    }

    #[test]
    fn activation_is_idempotent_across_resumes() {
        let dir = tempfile::tempdir().unwrap();
        stage_skills_slot(dir.path(), 0);
        stage_browser_slot(dir.path(), 1);
        let e = env(&[("ENGRAM_FORGE_TOKEN", "tok")]);
        let first = activate(dir.path(), &e);
        let second = activate(dir.path(), &e);
        assert_eq!(first.activated, second.activated);
        let l = Layout::under(dir.path());
        assert!(l.agents_skills.join("share-file").is_symlink());
        assert!(l.agents_skills.join("show-your-work").is_symlink());
    }
}
