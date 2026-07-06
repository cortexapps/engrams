//! ADR 0027 e2e: a generated session must carry the activated skills +
//! browser tooling.
//!
//! Drives the real session-generation path on the dev `ProcessBackend`
//! (`create` → `start_agent`) with the RO bundles staged, and asserts the
//! produced session directory contains everything a harness discovers:
//! the `~/.claude/skills` tree (share-file always; show-your-work when the
//! browser bundle is present), and the `engram-share`/`playwright-cli` wrappers on
//! PATH + `/etc/gitconfig`. This is the cross-cutting check that the engine
//! actually lands skills in sessions — the per-unit behavior is covered by
//! `engram-session-bundles` tests.
//!
//! Single test in its own integration binary → the `set_var` of the bundle-
//! dir overrides is process-isolated (no cross-test env race).

use std::collections::HashMap;
use std::path::Path;

use engram_core::traits::sandbox::SandboxBackend;
use engram_core::types::sandbox::{
    AgentSpec, AuxRoDrive, CpuLimit, DiskLimit, MemoryLimit, SandboxSpec,
};
use engram_sandbox_process::ProcessBackend;

fn write_exec(path: &Path, body: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, body).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}

/// Build fake skills + browser bundle trees mirroring `deploy/bundles/*`.
fn stage_fake_bundles(skills: &Path, browser: &Path) {
    write_exec(&skills.join("bin/engram-share"), "#!/bin/sh\n");
    write_exec(&skills.join("bin/git-askpass"), "#!/bin/sh\n");
    std::fs::create_dir_all(skills.join("skills/share-file")).unwrap();
    std::fs::write(skills.join("skills/share-file/SKILL.md"), "---\n").unwrap();
    std::fs::write(
        skills.join("mount.json"),
        r#"{"kind":"skill","skills":[{"name":"share-file","bins":["bin/engram-share"]}],"provides_askpass":"bin/git-askpass"}"#,
    )
    .unwrap();

    write_exec(&browser.join("bin/playwright-cli"), "#!/bin/sh\n");
    std::fs::create_dir_all(browser.join("skills/show-your-work")).unwrap();
    std::fs::write(browser.join("skills/show-your-work/SKILL.md"), "---\n").unwrap();
    std::fs::write(
        browser.join("mount.json"),
        r#"{"kind":"skill","skills":[{"name":"show-your-work","bins":["bin/playwright-cli"]}]}"#,
    )
    .unwrap();
}

#[tokio::test]
async fn generated_session_has_skills_and_browser_tooling() {
    let tmp = tempfile::tempdir().unwrap();
    let skills_dir = tmp.path().join("bundles/skills");
    let browser_dir = tmp.path().join("bundles/browser");
    stage_fake_bundles(&skills_dir, &browser_dir);

    // Point ProcessBackend's dev staging at the fake bundles. Safe: edition
    // 2021 `set_var` isn't unsafe, and this is the only test in the binary.
    std::env::set_var("ENGRAM_SKILLS_BUNDLE_DIR", &skills_dir);
    std::env::set_var("ENGRAM_BROWSER_BUNDLE_DIR", &browser_dir);

    let work = tmp.path().join("work");
    let backend = ProcessBackend::new(&work);

    let spec = SandboxSpec {
        image: "test".into(),
        rootfs_source: None,
        image_uri: None,
        rootfs_manifest: None,
        cpu: CpuLimit { vcpus: 1 },
        memory: MemoryLimit { max_mib: 256 },
        disk: DiskLimit { max_gib: 1 },
        ttl: None,
        env: HashMap::new(),
        workdir: None,
        network: Default::default(),
        // Dev staging is spec-independent, but pass reserved slots through to
        // mirror what coord capture records (ADR 0055 sentinel device model).
        aux_ro_drives: (0..2).map(AuxRoDrive::reserved_slot).collect(),
    };
    let id = backend.create(spec).await.expect("create session");

    // A forge-bound session: the broker token rides AgentSpec.env (per-spawn),
    // which is exactly where the activation gate must look for it.
    let agent = AgentSpec {
        binding_epoch: 1,
        argv: vec!["/bin/sh".into(), "-c".into(), "exit 0".into()],
        env: HashMap::from_iter([("ENGRAM_FORGE_TOKEN".into(), "tok".into())]),
        session_env: HashMap::new(),
        host_ca_pem: None,
    };
    backend.start_agent(id, agent).await.expect("start_agent");

    // The generated session directory.
    let cwd = work.join(id.to_string());

    // Skills discovery: ~/.claude/skills -> ~/.agents/skills, populated.
    assert!(
        cwd.join("root/.claude/skills").is_symlink(),
        "~/.claude/skills symlink missing in the generated session",
    );
    assert!(
        cwd.join("root/.agents/skills/share-file").is_symlink(),
        "share-file skill not wired (should always be present)",
    );
    assert!(
        cwd.join("root/.agents/skills/show-your-work").is_symlink(),
        "show-your-work not wired despite browser bundle present",
    );

    // Wrappers on PATH + git wiring. The forge token still wires the askpass +
    // gitconfig (git push stays brokered); ADR 0058 retired the baked PR skill —
    // PRs open via `gh` from the integrations-cli bundle now.
    assert!(cwd.join("usr/local/bin/engram-share").is_symlink());
    let gitconfig = std::fs::read_to_string(cwd.join("etc/gitconfig")).expect("gitconfig written");
    assert!(
        gitconfig.contains("git-askpass"),
        "gitconfig must point core.askPass at the bundle git-askpass; got {gitconfig:?}",
    );

    // Browser: the playwright-cli wrapper is on PATH (no MCP config) so the
    // agent drives the browser with plain `playwright-cli`.
    assert!(
        cwd.join("usr/local/bin/playwright-cli").is_symlink(),
        "playwright-cli wrapper not symlinked onto PATH",
    );
    assert!(
        !cwd.join("root/.mcp.json").exists(),
        "no MCP config should be written (CLI path)",
    );

    // The bundle mounts themselves are symlinked under the session cwd at the
    // ADR 0055 reserved-slot paths (skills -> dyn/0, browser -> dyn/1).
    assert!(cwd.join("opt/engram/dyn/0").is_symlink());
    assert!(cwd.join("opt/engram/dyn/1").is_symlink());
}
