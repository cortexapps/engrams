//! ADR 0027 e2e: a generated session must carry the activated skills +
//! browser tooling.
//!
//! Drives the real session-generation path on the dev `ProcessBackend`
//! (`restore_base_for_session` → `start_agent`) with the RO bundles staged.
//! It asserts that the produced session directory contains everything a
//! harness discovers:
//! the `~/.claude/skills` tree (share-file always; browser when its bundle is
//! present), and the browser/share wrappers on PATH + `/etc/gitconfig`. This is
//! the cross-cutting check that the engine
//! actually lands skills in sessions — the per-unit behavior is covered by
//! `engram-session-bundles` tests.
//!
//! Single test in its own integration binary → the `set_var` catalog
//! overrides are process-isolated (no cross-test env race).

use std::collections::HashMap;
use std::path::Path;

use engram_core::traits::sandbox::SandboxBackend;
use engram_core::types::sandbox::{AgentSpec, AuxRoDrive};
use engram_core::types::{SnapshotId, SnapshotMetadata};
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
fn stage_fake_bundles(skills: &Path, browser: &Path, harness: &Path) {
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
    std::fs::create_dir_all(browser.join("skills/browser")).unwrap();
    std::fs::write(browser.join("skills/browser/SKILL.md"), "---\n").unwrap();
    std::fs::write(
        browser.join("mount.json"),
        r#"{"kind":"skill","skills":[{"name":"browser","bins":["bin/playwright-cli"]}]}"#,
    )
    .unwrap();

    write_exec(
        &harness.join("harness"),
        "#!/bin/sh\npwd > harness-pwd\ntouch harness-ran\n",
    );
}

#[tokio::test]
async fn generated_session_has_skills_and_browser_tooling() {
    let tmp = tempfile::tempdir().unwrap();
    let skills_dir = tmp.path().join("bundles/skills");
    let browser_dir = tmp.path().join("bundles/browser");
    let harness_dir = tmp.path().join("bundles/harness-claude");
    stage_fake_bundles(&skills_dir, &browser_dir, &harness_dir);
    std::fs::write(
        tmp.path().join("bundles/current.json"),
        r#"{"skills":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","browser":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","harness-claude":"cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"}"#,
    )
    .unwrap();

    let rootfs = tmp.path().join("images/process-dev");
    std::fs::create_dir_all(rootfs.join("workspace")).unwrap();
    std::fs::write(rootfs.join("workspace/README.md"), "# Process image\n").unwrap();
    let image_catalog = tmp.path().join("process-images.json");
    std::fs::write(
        &image_catalog,
        serde_json::to_vec(&serde_json::json!([{
            "image_uri": "dev.local/process:latest",
            "manifest_digest": "process-dev",
            "rootfs": rootfs,
        }]))
        .unwrap(),
    )
    .unwrap();

    // Point ProcessBackend's dev catalog at the fake bundles. Safe: edition
    // 2021 `set_var` isn't unsafe, and this is the only test in the binary.
    std::env::set_var("ENGRAM_BUNDLE_DIR", tmp.path().join("bundles"));
    std::env::set_var("ENGRAM_PROCESS_IMAGE_CATALOG", &image_catalog);

    assert_eq!(ProcessBackend::ready_images(), ["process-dev"]);

    let work = tmp.path().join("work");
    let backend = ProcessBackend::new(&work);

    let selected_mounts = vec![
        AuxRoDrive {
            drive_id: AuxRoDrive::slot_drive_id(AuxRoDrive::HARNESS_SLOT_INDEX),
            guest_mount: AuxRoDrive::slot_guest_mount(AuxRoDrive::HARNESS_SLOT_INDEX),
            fs_type: "squashfs".into(),
            sha256: Some("cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc".into()),
        },
        AuxRoDrive {
            drive_id: AuxRoDrive::slot_drive_id(AuxRoDrive::FIRST_SKILL_SLOT_INDEX),
            guest_mount: AuxRoDrive::slot_guest_mount(AuxRoDrive::FIRST_SKILL_SLOT_INDEX),
            fs_type: "squashfs".into(),
            sha256: Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into()),
        },
        AuxRoDrive {
            drive_id: AuxRoDrive::slot_drive_id(AuxRoDrive::FIRST_SKILL_SLOT_INDEX + 1),
            guest_mount: AuxRoDrive::slot_guest_mount(AuxRoDrive::FIRST_SKILL_SLOT_INDEX + 1),
            fs_type: "squashfs".into(),
            sha256: Some("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into()),
        },
    ];
    let id = backend
        .restore_base_for_session(
            SnapshotMetadata {
                id: SnapshotId::new(),
                size_bytes: 0,
                created_at: chrono::DateTime::UNIX_EPOCH,
                image_version: "dev.local/process:latest".into(),
                disk_manifest: None,
                memory_manifest: None,
                base_memory_manifest: None,
                migration_source: None,
                source_sandbox_id: None,
                state_blob_key: None,
                sidecar_blob_key: None,
                rootfs_blob_key: None,
                working_set_blob_key: None,
                aux_bundles: Vec::new(),
                paused_at: None,
                peer_hints: Vec::new(),
            },
            HashMap::new(),
            selected_mounts,
        )
        .await
        .expect("restore directory-backed Process image");

    // A forge-bound session: the broker token rides AgentSpec.env (per-spawn),
    // which is exactly where the activation gate must look for it.
    let agent = AgentSpec {
        binding_epoch: 1,
        argv: vec!["/opt/engram/dyn/0/harness".into()],
        env: HashMap::from_iter([
            ("ENGRAM_FORGE_TOKEN".into(), "tok".into()),
            ("ENGRAM_HARNESS_CWD".into(), "/workspace".into()),
        ]),
        session_env: HashMap::new(),
        host_ca_pem: None,
    };
    backend.start_agent(id, agent).await.expect("start_agent");

    // The generated session directory.
    let cwd = work.join(id.to_string());
    assert_eq!(
        std::fs::read_to_string(cwd.join("workspace/README.md")).unwrap(),
        "# Process image\n",
        "the local image rootfs was not materialized into the session",
    );
    for _ in 0..50 {
        if cwd.join("workspace/harness-ran").exists() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(
        cwd.join("workspace/harness-ran").exists(),
        "guest-absolute harness path was not translated to the Process sandbox root",
    );
    assert_eq!(
        std::fs::read_to_string(cwd.join("workspace/harness-pwd"))
            .unwrap()
            .trim(),
        std::fs::canonicalize(cwd.join("workspace"))
            .unwrap()
            .display()
            .to_string(),
        "guest-absolute image workdir was not translated into the Process root",
    );

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
        cwd.join("root/.agents/skills/browser").is_symlink(),
        "browser skill not wired despite browser bundle present",
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

    // The browser CLI wrapper is on PATH; the capability's local image tool
    // is injected by the harness, not through a session-level MCP config.
    assert!(
        cwd.join("usr/local/bin/playwright-cli").is_symlink(),
        "playwright-cli wrapper not symlinked onto PATH",
    );
    assert!(
        !cwd.join("usr/local/bin/agent-browser").exists(),
        "evaluation-only agent-browser must not enter the production bundle",
    );
    assert!(
        !cwd.join("root/.mcp.json").exists(),
        "no MCP config should be written (CLI path)",
    );

    // The bundle mounts themselves are symlinked under the session cwd at the
    // ADR 0055 reserved-slot paths (0-2 are harness/agentd/guest-tools).
    assert!(cwd.join("opt/engram/dyn/0").is_symlink());
    assert!(cwd.join("opt/engram/dyn/3").is_symlink());
    assert!(cwd.join("opt/engram/dyn/4").is_symlink());
}
