//! Integration tests for the image baker.
//!
//! Most tests use a `RecordingDocker` mock that captures the calls
//! the builder makes (so we can assert orchestration order and
//! arguments without needing Docker on the test machine). One
//! end-to-end test, gated on `docker` being on PATH, exercises the
//! real `DockerCli`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use engram_image_builder::docker::{BuildArgs, DockerError, DockerImageConfig, DockerRunner};
use engram_image_builder::ext4::{Ext4Error, Ext4Packer};
use engram_image_builder::{AgentInjection, BuildRequest, Builder, Format};

// ---------------------------------------------------------------------
// RecordingDocker — mock for unit-shape integration tests
// ---------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
enum Call {
    Build {
        tag: String,
        dockerfile: PathBuf,
        build_args: Vec<(String, String)>,
    },
    Create {
        tag: String,
    },
    Export {
        container_id: String,
        dest: PathBuf,
    },
    RmContainer {
        container_id: String,
    },
    Rmi {
        tag: String,
    },
}

#[derive(Default)]
struct RecordingState {
    calls: Vec<Call>,
    next_container_id: u32,
    /// Optional injected error per method — fail the next call.
    inject_build_err: Option<DockerError>,
    inject_create_err: Option<DockerError>,
    inject_export_err: Option<DockerError>,
    /// Files to materialize when `export_to_dir` is invoked.
    fake_rootfs: Vec<(PathBuf, Vec<u8>)>,
    /// Canned `docker inspect` config the baker folds into the manifest.
    image_config: DockerImageConfig,
}

#[derive(Clone)]
struct RecordingDocker {
    inner: Arc<parking_lot::Mutex<RecordingState>>,
}

impl RecordingDocker {
    fn new() -> Self {
        Self {
            inner: Arc::new(parking_lot::Mutex::new(RecordingState::default())),
        }
    }

    fn with_fake_rootfs(self, files: impl IntoIterator<Item = (PathBuf, Vec<u8>)>) -> Self {
        self.inner.lock().fake_rootfs = files.into_iter().collect();
        self
    }

    fn with_image_config(self, env: Vec<String>, working_dir: Option<String>) -> Self {
        self.inner.lock().image_config = DockerImageConfig { env, working_dir };
        self
    }

    fn calls(&self) -> Vec<Call> {
        self.inner.lock().calls.clone()
    }
}

#[async_trait]
impl DockerRunner for RecordingDocker {
    async fn build(&self, args: BuildArgs) -> Result<(), DockerError> {
        // Record the call *before* the inject-err check so failed
        // attempts are still observable to tests.
        let mut g = self.inner.lock();
        let mut build_args: Vec<(String, String)> = args.build_args.into_iter().collect();
        build_args.sort_by(|a, b| a.0.cmp(&b.0));
        g.calls.push(Call::Build {
            tag: args.tag,
            dockerfile: args.dockerfile,
            build_args,
        });
        if let Some(e) = g.inject_build_err.take() {
            return Err(e);
        }
        Ok(())
    }

    async fn create(&self, image_tag: &str) -> Result<String, DockerError> {
        let mut g = self.inner.lock();
        g.calls.push(Call::Create {
            tag: image_tag.to_string(),
        });
        if let Some(e) = g.inject_create_err.take() {
            return Err(e);
        }
        g.next_container_id += 1;
        Ok(format!("container-{}", g.next_container_id))
    }

    async fn export_to_dir(&self, container_id: &str, dest: &Path) -> Result<(), DockerError> {
        let (fake_rootfs, err) = {
            let mut g = self.inner.lock();
            g.calls.push(Call::Export {
                container_id: container_id.to_string(),
                dest: dest.to_path_buf(),
            });
            (g.fake_rootfs.clone(), g.inject_export_err.take())
        };
        if let Some(e) = err {
            return Err(e);
        }
        // Materialize the fake rootfs so callers can verify the result.
        for (rel, contents) in fake_rootfs {
            let full = dest.join(rel);
            if let Some(parent) = full.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            tokio::fs::write(&full, contents).await?;
        }
        Ok(())
    }

    async fn rm_container(&self, container_id: &str) -> Result<(), DockerError> {
        self.inner.lock().calls.push(Call::RmContainer {
            container_id: container_id.to_string(),
        });
        Ok(())
    }

    async fn rmi(&self, image_tag: &str) -> Result<(), DockerError> {
        self.inner.lock().calls.push(Call::Rmi {
            tag: image_tag.to_string(),
        });
        Ok(())
    }

    async fn inspect_config(&self, _id: &str) -> Result<DockerImageConfig, DockerError> {
        // Intentionally not recorded as a `Call` so existing
        // orchestration-order assertions (build/create/export/rm/rmi)
        // stay stable.
        Ok(self.inner.lock().image_config.clone())
    }

    fn clone_runner(&self) -> Box<dyn DockerRunner> {
        Box::new(self.clone())
    }
}

// ---------------------------------------------------------------------
// Test fixture helpers
// ---------------------------------------------------------------------

/// Build a minimal source repo: Dockerfile + engram.toml.
fn write_source_repo(dir: &Path, engram_toml: &str) {
    std::fs::write(dir.join("Dockerfile"), "FROM scratch\n").unwrap();
    std::fs::write(dir.join("engram.toml"), engram_toml).unwrap();
}

/// Helper: build a `ChunkStore` rooted at a fresh tempdir. Caller
/// holds the `TempDir` for the lifetime of the test (drop = cleanup).
fn test_chunk_store() -> (engram_chunk_store::ChunkStore, tempfile::TempDir) {
    use std::sync::Arc;
    let dir = tempfile::tempdir().unwrap();
    let blob: Arc<dyn engram_core::traits::BlobStorage> = Arc::new(
        engram_storage_local::LocalBlobStorage::new(dir.path().to_path_buf()),
    );
    (engram_chunk_store::ChunkStore::new(blob), dir)
}

fn req(source: &Path, images_dir: &Path, repo: &str, tag: &str) -> BuildRequest {
    BuildRequest {
        source: source.to_path_buf(),
        repo: repo.into(),
        tag: tag.into(),
        images_dir: images_dir.to_path_buf(),
        format: Format::Directory,
        agent_injection: None,
    }
}

fn req_ext4(source: &Path, images_dir: &Path, repo: &str, tag: &str) -> BuildRequest {
    BuildRequest {
        format: Format::Ext4,
        ..req(source, images_dir, repo, tag)
    }
}

// ---------------------------------------------------------------------
// RecordingPacker — Ext4Packer mock parallel to RecordingDocker
// ---------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
struct PackCall {
    src_dir: PathBuf,
    dst_image: PathBuf,
    size_bytes: u64,
}

#[derive(Default)]
struct PackerState {
    calls: Vec<PackCall>,
    inject_err: Option<String>,
}

#[derive(Clone, Default)]
struct RecordingPacker {
    inner: Arc<parking_lot::Mutex<PackerState>>,
}

impl RecordingPacker {
    fn calls(&self) -> Vec<PackCall> {
        self.inner.lock().calls.clone()
    }

    fn fail_next(&self, msg: impl Into<String>) {
        self.inner.lock().inject_err = Some(msg.into());
    }
}

#[async_trait]
impl Ext4Packer for RecordingPacker {
    async fn pack(
        &self,
        src_dir: &Path,
        dst_image: &Path,
        size_bytes: u64,
    ) -> Result<(), Ext4Error> {
        // Record the call + take any injected error inside the lock,
        // then drop the guard before any await — parking_lot's
        // MutexGuard isn't Send, and serve_connection's Send bound
        // would otherwise reject the future.
        let injected = {
            let mut g = self.inner.lock();
            g.calls.push(PackCall {
                src_dir: src_dir.to_path_buf(),
                dst_image: dst_image.to_path_buf(),
                size_bytes,
            });
            g.inject_err.take()
        };
        if let Some(msg) = injected {
            return Err(Ext4Error::Mke2fs(msg));
        }
        // Materialize a non-empty file at dst_image so the builder's
        // metadata().len() check succeeds and BuildOutcome reports a
        // realistic size_bytes. Real mke2fs would do something similar.
        let mut data = vec![0u8; 4096];
        data[0] = 0xAB; // mark so accidents in test setup show up
        tokio::fs::write(dst_image, &data).await?;
        Ok(())
    }
}

// ---------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------

#[tokio::test]
async fn build_runs_orchestration_and_writes_manifest_plus_rootfs() {
    let src = tempfile::tempdir().unwrap();
    let images = tempfile::tempdir().unwrap();
    write_source_repo(src.path(), r#"name = "cortex-api""#);

    let docker = RecordingDocker::new()
        .with_fake_rootfs(vec![
            (PathBuf::from("README.md"), b"# starter\n".to_vec()),
            (PathBuf::from("scripts/run.sh"), b"#!/bin/sh\n".to_vec()),
        ])
        // The Dockerfile's ENV + WORKDIR (engram.toml here sets neither),
        // surfaced via `docker inspect`, must fold into the manifest.
        .with_image_config(
            vec!["DOCKER_ONLY=yes".to_string(), "PATH=/usr/bin".to_string()],
            Some("/from-docker".to_string()),
        );
    let (cs, _csdir) = test_chunk_store();
    let builder = Builder::new(docker.clone(), cs);

    let outcome = builder
        .build(&req(src.path(), images.path(), "cortex/api", "warm-1"))
        .await
        .unwrap();

    // Manifest landed where the coordinator's ImageRegistry will look.
    assert!(outcome.manifest_path.is_file());
    let rendered = std::fs::read_to_string(&outcome.manifest_path).unwrap();
    assert!(
        rendered.contains(r#"name = "cortex-api""#),
        "rendered manifest must carry the repo's name; got {rendered:?}",
    );
    assert!(
        !rendered.contains("[build]"),
        "rendered manifest must NOT carry the source-only [build] section",
    );
    // The Dockerfile ENV + WORKDIR were folded in (the platform reads
    // the OCI image config so authors needn't restate them).
    assert!(
        rendered.contains("DOCKER_ONLY"),
        "rendered manifest must inherit the Dockerfile ENV; got {rendered:?}",
    );
    assert!(
        rendered.contains("/from-docker"),
        "rendered manifest must inherit the Dockerfile WORKDIR; got {rendered:?}",
    );

    // Rootfs was materialized.
    assert!(outcome.rootfs_path.join("README.md").is_file());
    assert!(outcome.rootfs_path.join("scripts/run.sh").is_file());
    assert!(outcome.size_bytes > 0);

    // Orchestration order: build → create → export → rm_container → rmi.
    let calls = docker.calls();
    assert!(matches!(calls[0], Call::Build { .. }));
    assert!(matches!(calls[1], Call::Create { .. }));
    assert!(matches!(calls[2], Call::Export { .. }));
    assert!(matches!(calls[3], Call::RmContainer { .. }));
    assert!(matches!(calls[4], Call::Rmi { .. }));

    // ADR 0027: the baker no longer plants ANY skill glue — neither the
    // share-file wrapper/skill nor the forge glue. Delivery moved to the
    // fleet-wide `skills` RO bundle, activated per-session by agentd (see
    // `engram-session-bundles`). The rootfs must be clean of all of it.
    assert!(!outcome.rootfs_path.join("opt/engram/git-askpass").exists());
    assert!(!outcome.rootfs_path.join("usr/local/bin/engram-pr").exists());
    assert!(
        !outcome
            .rootfs_path
            .join("usr/local/bin/engram-share")
            .exists(),
        "ADR 0027: engram-share must NOT be baked — it ships in the skills bundle"
    );
    assert!(
        !outcome.rootfs_path.join("root/.agents/skills").exists(),
        "ADR 0027: no skills baked into the rootfs"
    );
    assert!(
        !outcome.rootfs_path.join("root/.claude/skills").exists(),
        "ADR 0027: no skills symlink baked into the rootfs"
    );
}

#[tokio::test]
async fn build_does_not_bake_forge_glue_for_git_images() {
    // ADR 0027: a `[git]` binding no longer makes the baker plant forge
    // glue (askpass / engram-pr / gitconfig / create-pull-request skill).
    // The skills bundle carries the wrappers + SKILL.md; agentd writes
    // /etc/gitconfig and symlinks the create-pull-request skill per-session
    // when a forge token is present. The baked rootfs stays clean.
    let src = tempfile::tempdir().unwrap();
    let images = tempfile::tempdir().unwrap();
    write_source_repo(
        src.path(),
        "name = \"forge-img\"\n[git]\nprovider = \"github\"\n",
    );
    let docker = RecordingDocker::new();
    let (cs, _csdir) = test_chunk_store();
    let builder = Builder::new(docker.clone(), cs);

    let outcome = builder
        .build(&req(src.path(), images.path(), "acme/forge-img", "warm-1"))
        .await
        .unwrap();
    let rootfs = &outcome.rootfs_path;

    assert!(
        !rootfs.join("opt/engram/git-askpass").exists(),
        "ADR 0027: git-askpass must NOT be baked"
    );
    assert!(
        !rootfs.join("usr/local/bin/engram-pr").exists(),
        "ADR 0027: engram-pr must NOT be baked"
    );
    assert!(
        !rootfs.join("etc/gitconfig").exists(),
        "ADR 0027: gitconfig is written by agentd at session bind, not baked"
    );
    assert!(
        !rootfs.join("root/.agents/skills").exists(),
        "ADR 0027: no skills baked into the rootfs"
    );
}

#[tokio::test]
async fn build_passes_build_args_to_docker() {
    let src = tempfile::tempdir().unwrap();
    let images = tempfile::tempdir().unwrap();
    write_source_repo(
        src.path(),
        r#"
            name = "cortex-api"
            [build]
            args = { BUILD_FLAVOR = "release", LANG_VERSION = "20" }
        "#,
    );
    let docker = RecordingDocker::new();
    let (cs, _csdir) = test_chunk_store();
    let builder = Builder::new(docker.clone(), cs);
    builder
        .build(&req(src.path(), images.path(), "cortex/api", "warm-1"))
        .await
        .unwrap();

    let calls = docker.calls();
    match &calls[0] {
        Call::Build { build_args, .. } => {
            assert_eq!(
                build_args,
                &vec![
                    ("BUILD_FLAVOR".to_string(), "release".to_string()),
                    ("LANG_VERSION".to_string(), "20".to_string()),
                ],
            );
        }
        other => panic!("expected Build call first, got {other:?}"),
    }
}

#[tokio::test]
async fn build_uses_dockerfile_path_from_engram_toml() {
    let src = tempfile::tempdir().unwrap();
    let images = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(src.path().join("deploy")).unwrap();
    std::fs::write(
        src.path().join("deploy/Dockerfile.runtime"),
        "FROM scratch\n",
    )
    .unwrap();
    std::fs::write(
        src.path().join("engram.toml"),
        r#"
            name = "cortex-api"
            [build]
            dockerfile = "deploy/Dockerfile.runtime"
        "#,
    )
    .unwrap();

    let docker = RecordingDocker::new();
    let (cs, _csdir) = test_chunk_store();
    let builder = Builder::new(docker.clone(), cs);
    builder
        .build(&req(src.path(), images.path(), "cortex/api", "warm-1"))
        .await
        .unwrap();

    let calls = docker.calls();
    match &calls[0] {
        Call::Build { dockerfile, .. } => {
            assert!(dockerfile.ends_with("deploy/Dockerfile.runtime"));
        }
        other => panic!("expected Build call first, got {other:?}"),
    }
}

#[tokio::test]
async fn build_rejects_path_traversal_in_repo_or_tag() {
    let src = tempfile::tempdir().unwrap();
    let images = tempfile::tempdir().unwrap();
    write_source_repo(src.path(), r#"name = "x""#);
    let (cs, _csdir) = test_chunk_store();
    let builder = Builder::new(RecordingDocker::new(), cs);

    for (repo, tag) in [
        ("..", "warm-1"),
        ("good", ".."),
        ("a/../b", "warm-1"),
        ("/etc", "passwd"),
        ("", "warm-1"),
    ] {
        let res = builder
            .build(&req(src.path(), images.path(), repo, tag))
            .await;
        assert!(
            matches!(res, Err(engram_image_builder::BuildError::InvalidPath(_))),
            "{repo}/{tag} must be rejected; got {res:?}",
        );
    }
}

#[tokio::test]
async fn build_errors_when_dockerfile_is_missing() {
    let src = tempfile::tempdir().unwrap();
    let images = tempfile::tempdir().unwrap();
    // Only engram.toml; no Dockerfile.
    std::fs::write(src.path().join("engram.toml"), r#"name = "x""#).unwrap();
    let (cs, _csdir) = test_chunk_store();
    let builder = Builder::new(RecordingDocker::new(), cs);
    let res = builder
        .build(&req(src.path(), images.path(), "x", "warm-1"))
        .await;
    match res {
        Err(engram_image_builder::BuildError::Config(msg)) => {
            assert!(
                msg.contains("Dockerfile"),
                "msg should mention the missing file: {msg}"
            );
        }
        other => panic!("expected Config error about Dockerfile, got {other:?}"),
    }
}

#[tokio::test]
async fn build_errors_when_engram_toml_is_missing() {
    let src = tempfile::tempdir().unwrap();
    let images = tempfile::tempdir().unwrap();
    std::fs::write(src.path().join("Dockerfile"), "FROM scratch\n").unwrap();
    let (cs, _csdir) = test_chunk_store();
    let builder = Builder::new(RecordingDocker::new(), cs);
    let res = builder
        .build(&req(src.path(), images.path(), "x", "warm-1"))
        .await;
    assert!(
        matches!(res, Err(engram_image_builder::BuildError::Config(_))),
        "missing engram.toml must fail with Config error; got {res:?}",
    );
}

#[tokio::test]
async fn build_propagates_docker_build_failure_and_does_not_create_rootfs() {
    let src = tempfile::tempdir().unwrap();
    let images = tempfile::tempdir().unwrap();
    write_source_repo(src.path(), r#"name = "x""#);

    let docker = RecordingDocker::new();
    docker.inner.lock().inject_build_err = Some(DockerError::NonZeroExit {
        command: "docker build".into(),
        code: Some(1),
        stderr: "no space left on device".into(),
    });
    let (cs, _csdir) = test_chunk_store();
    let builder = Builder::new(docker.clone(), cs);
    let res = builder
        .build(&req(src.path(), images.path(), "x", "warm-1"))
        .await;
    assert!(matches!(
        res,
        Err(engram_image_builder::BuildError::Docker(_))
    ));

    // No image dir was created, no rootfs.
    let image_dir = images.path().join("x/warm-1");
    assert!(
        !image_dir.exists(),
        "build failure must not leave a partially-baked image directory",
    );
    // Only the build call was made — no create/export/rm/rmi.
    assert_eq!(docker.calls().len(), 1);
}

#[tokio::test]
async fn build_cleans_up_image_when_create_fails() {
    let src = tempfile::tempdir().unwrap();
    let images = tempfile::tempdir().unwrap();
    write_source_repo(src.path(), r#"name = "x""#);

    let docker = RecordingDocker::new();
    docker.inner.lock().inject_create_err = Some(DockerError::NonZeroExit {
        command: "docker create".into(),
        code: Some(1),
        stderr: "out of memory".into(),
    });
    let (cs, _csdir) = test_chunk_store();
    let builder = Builder::new(docker.clone(), cs);
    let res = builder
        .build(&req(src.path(), images.path(), "x", "warm-1"))
        .await;
    assert!(matches!(
        res,
        Err(engram_image_builder::BuildError::Docker(_))
    ));

    let calls = docker.calls();
    // Saw build + create-attempt + rmi (cleanup of the build-tagged image).
    assert!(matches!(calls[0], Call::Build { .. }));
    assert!(matches!(calls.last(), Some(Call::Rmi { .. })));
}

#[tokio::test]
async fn build_cleans_up_container_and_image_when_export_fails() {
    let src = tempfile::tempdir().unwrap();
    let images = tempfile::tempdir().unwrap();
    write_source_repo(src.path(), r#"name = "x""#);

    let docker = RecordingDocker::new();
    docker.inner.lock().inject_export_err = Some(DockerError::NonZeroExit {
        command: "tar -x".into(),
        code: Some(1),
        stderr: "permission denied".into(),
    });
    let (cs, _csdir) = test_chunk_store();
    let builder = Builder::new(docker.clone(), cs);
    let res = builder
        .build(&req(src.path(), images.path(), "x", "warm-1"))
        .await;
    assert!(matches!(
        res,
        Err(engram_image_builder::BuildError::Docker(_))
    ));

    // Both rm_container AND rmi must run even on export failure.
    let calls = docker.calls();
    assert!(
        calls.iter().any(|c| matches!(c, Call::RmContainer { .. })),
        "export failure must still rm the container; got {calls:?}",
    );
    assert!(
        calls.iter().any(|c| matches!(c, Call::Rmi { .. })),
        "export failure must still rmi the build tag; got {calls:?}",
    );
}

#[tokio::test]
async fn rebuild_overwrites_existing_image_dir() {
    let src = tempfile::tempdir().unwrap();
    let images = tempfile::tempdir().unwrap();
    write_source_repo(src.path(), r#"name = "x""#);

    let docker = RecordingDocker::new()
        .with_fake_rootfs(vec![(PathBuf::from("v1-only.txt"), b"hi".to_vec())]);
    let (cs, _csdir) = test_chunk_store();
    let builder = Builder::new(docker, cs);
    builder
        .build(&req(src.path(), images.path(), "x", "warm-same"))
        .await
        .unwrap();

    // Second bake with different fake rootfs.
    let docker2 = RecordingDocker::new()
        .with_fake_rootfs(vec![(PathBuf::from("v2-only.txt"), b"yo".to_vec())]);
    let (cs2, _csdir2) = test_chunk_store();
    let builder2 = Builder::new(docker2, cs2);
    let outcome = builder2
        .build(&req(src.path(), images.path(), "x", "warm-same"))
        .await
        .unwrap();

    // The first bake's file must be gone — overwrite semantics.
    assert!(
        !outcome.rootfs_path.join("v1-only.txt").exists(),
        "second bake must wipe the prior rootfs",
    );
    assert!(outcome.rootfs_path.join("v2-only.txt").is_file());
}

#[tokio::test]
async fn manifest_round_trips_full_engram_toml_through_baker() {
    // Lock down that every manifest field in the source engram.toml
    // ends up in the rendered manifest.toml verbatim — important
    // because the coordinator's ImageRegistry parses the rendered
    // manifest and the runtime contract has to match.
    let src = tempfile::tempdir().unwrap();
    let images = tempfile::tempdir().unwrap();
    write_source_repo(
        src.path(),
        r#"
            name = "cortex-api"
            description = "API service"
            secret_mode = "broker"

            [env]
            NODE_ENV = "production"

            [secrets.GITHUB_TOKEN]
            allow_hosts = ["api.github.com"]
            required = true

            [network]
            default = "deny"
            allow_hosts = ["api.github.com"]

            [resources]
            suggested_memory_mib = 4096

            [build]
            dockerfile = "Dockerfile"
            args = { LANG = "rust" }
        "#,
    );

    let (cs, _csdir) = test_chunk_store();
    let builder = Builder::new(RecordingDocker::new(), cs);
    let outcome = builder
        .build(&req(src.path(), images.path(), "cortex/api", "warm-1"))
        .await
        .unwrap();
    let rendered = std::fs::read_to_string(&outcome.manifest_path).unwrap();
    let parsed: engram_core::types::ImageManifest = toml::from_str(&rendered).unwrap();

    assert_eq!(parsed.name, "cortex-api");
    assert_eq!(parsed.description.as_deref(), Some("API service"));
    assert_eq!(parsed.secret_mode, engram_core::types::SecretMode::Broker);
    assert_eq!(parsed.env["NODE_ENV"], "production");
    assert!(parsed.secrets.contains_key("GITHUB_TOKEN"));
    assert_eq!(parsed.network.allow_hosts, vec!["api.github.com"]);
    assert_eq!(parsed.resources.suggested_memory_mib, Some(4096));
}

// ---------------------------------------------------------------------
// Real-Docker integration test — gated on `docker` being on PATH.
// ---------------------------------------------------------------------

fn docker_available() -> bool {
    std::process::Command::new("docker")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[tokio::test]
async fn end_to_end_with_real_docker() {
    if !docker_available() {
        eprintln!("`docker` not on PATH; skipping end-to-end bake test");
        return;
    }

    let src = tempfile::tempdir().unwrap();
    let images = tempfile::tempdir().unwrap();
    std::fs::write(
        src.path().join("Dockerfile"),
        // Tiny image so the test runs fast and doesn't pull much.
        "FROM alpine:3\nRUN echo 'hello from baker' > /greeting\n",
    )
    .unwrap();
    std::fs::write(
        src.path().join("engram.toml"),
        r#"
            name = "baker-test"
            [env]
            BAKED = "yes"
        "#,
    )
    .unwrap();

    let docker = engram_image_builder::DockerCli::new();
    let (cs, _csdir) = test_chunk_store();
    let builder = Builder::new(docker, cs);
    let outcome = builder
        .build(&req(src.path(), images.path(), "baker-test", "warm-1"))
        .await
        .expect("real docker bake should succeed");

    let greeting = std::fs::read_to_string(outcome.rootfs_path.join("greeting")).unwrap();
    assert_eq!(greeting.trim(), "hello from baker");

    let manifest: engram_core::types::ImageManifest =
        toml::from_str(&std::fs::read_to_string(outcome.manifest_path).unwrap()).unwrap();
    assert_eq!(manifest.name, "baker-test");
    assert_eq!(manifest.env["BAKED"], "yes");
}

// ---------------------------------------------------------------------
// Format::Ext4
// ---------------------------------------------------------------------

#[tokio::test]
async fn ext4_bake_round_trips_through_chunk_store() {
    // ADR 0007 promise: the chunked manifest is sufficient to
    // reproduce the disk exactly. If this drifts (chunk size,
    // hashing, manifest format), the host-agent's image cache
    // will silently serve corrupt blocks to FC. Lock it down.
    let src = tempfile::tempdir().unwrap();
    let images = tempfile::tempdir().unwrap();
    write_source_repo(src.path(), "name = \"chunk-roundtrip\"\n");

    let docker = RecordingDocker::new()
        .with_fake_rootfs([(PathBuf::from("etc/hostname"), b"engram\n".to_vec())]);
    let packer = RecordingPacker::default();
    let (cs, _csdir) = test_chunk_store();
    let builder = Builder::with_packer(docker, packer, cs.clone());

    let outcome = builder
        .build(&req_ext4(src.path(), images.path(), "p", "warm-1"))
        .await
        .expect("ext4 bake");

    let manifest_ref = outcome.disk_manifest.expect("disk manifest");
    let original = std::fs::read(&outcome.rootfs_path).expect("read ext4");

    // Pull the manifest back, materialize the disk into a scratch
    // file, and compare byte-for-byte against the on-disk ext4.
    let manifest = cs.get_manifest(manifest_ref).await.expect("get manifest");
    let dest = tempfile::NamedTempFile::new().unwrap();
    cs.materialize_to_file(&manifest, dest.path())
        .await
        .expect("materialize");
    let restored = std::fs::read(dest.path()).expect("read materialized");

    assert_eq!(restored.len(), original.len(), "size mismatch");
    assert_eq!(restored, original, "byte-for-byte mismatch");
}

#[tokio::test]
async fn build_ext4_packs_rootfs_into_image_file_and_drops_directory() {
    // The Ext4 path should: extract via docker, run the packer once
    // with src=<staging rootfs> and dst=<image_dir>/rootfs.ext4, then
    // remove the staging rootfs/. Final on-disk shape mirrors what
    // `image_registry::Rootfs::Ext4Image` consumes.
    let src = tempfile::tempdir().unwrap();
    let images = tempfile::tempdir().unwrap();
    write_source_repo(src.path(), "name = \"ext4-test\"\n");

    let docker = RecordingDocker::new().with_fake_rootfs([
        (PathBuf::from("etc/hostname"), b"engram\n".to_vec()),
        (
            PathBuf::from("bin/init"),
            b"#!/bin/sh\nexec /sbin/agent\n".to_vec(),
        ),
    ]);
    let packer = RecordingPacker::default();
    let (cs, _csdir) = test_chunk_store();
    let builder = Builder::with_packer(docker.clone(), packer.clone(), cs);

    let outcome = builder
        .build(&req_ext4(src.path(), images.path(), "p", "warm-1"))
        .await
        .expect("ext4 bake should succeed");

    // Outcome points at rootfs.ext4, not the directory.
    assert_eq!(outcome.rootfs_path, outcome.image_dir.join("rootfs.ext4"));
    assert!(outcome.rootfs_path.is_file(), "rootfs.ext4 should exist");
    // RecordingPacker writes a 4KB stub.
    assert_eq!(outcome.size_bytes, 4096);

    // Staging rootfs/ should have been wiped after a successful pack.
    assert!(
        !outcome.image_dir.join("rootfs").exists(),
        "staging dir must be removed after Ext4 pack",
    );

    // Packer was invoked exactly once with the right paths.
    let pack_calls = packer.calls();
    assert_eq!(pack_calls.len(), 1, "expected one pack call");
    assert_eq!(pack_calls[0].src_dir, outcome.image_dir.join("rootfs"));
    assert_eq!(
        pack_calls[0].dst_image,
        outcome.image_dir.join("rootfs.ext4")
    );
    assert!(
        pack_calls[0].size_bytes > 0,
        "size should be sized via recommended_size"
    );

    // ADR 0007: ext4 bakes produce a chunked manifest + sidecar bundle.json
    // so downstream consumers (host-agent image cache, NBD daemon,
    // VZ materialize) can resolve content through the chunk store.
    //
    // ADR 0008 Phase 3 / ADR 0036: bake also emits the per-chunk
    // bootstrap sidecar (bootstrap.disk.json) and bumps the
    // bundle schema to v2. v1 readers still see `disk_manifest`
    // and work; v2 readers additionally consult the bootstrap
    // file for chunk-on-fault.
    let manifest_ref = outcome
        .disk_manifest
        .expect("ext4 bake must produce a disk manifest");
    let bundle_path = outcome.image_dir.join("bundle.json");
    assert!(bundle_path.is_file(), "bundle.json sidecar should exist");
    let bundle: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&bundle_path).unwrap()).unwrap();
    assert_eq!(bundle["schema_version"], 2);
    let serialized: serde_json::Value = serde_json::to_value(manifest_ref).unwrap();
    assert_eq!(bundle["disk_manifest"], serialized);
    assert_eq!(bundle["bootstrap_disk_available"], true);

    // ADR 0036: the disk-side per-chunk bootstrap must be on disk
    // and reachable via BuildOutcome. No concatenated chunk blob is
    // produced — every entry addresses its chunk by its own digest
    // (the contract the delta push relies on).
    let bs_path = outcome
        .disk_bootstrap_path
        .as_ref()
        .expect("disk bootstrap path");
    assert!(bs_path.is_file(), "bootstrap.disk.json must exist");
    assert!(
        !outcome.image_dir.join("chunks.disk.blob").exists(),
        "ADR 0036: no monolithic chunk blob may be written"
    );
    let bs_bytes = std::fs::read(bs_path).unwrap();
    let parsed: engram_chunk_store::Bootstrap = serde_json::from_slice(&bs_bytes).unwrap();
    assert!(
        parsed.is_per_chunk(),
        "bootstrap must be the ADR 0036 per-chunk shape"
    );
    for entry in &parsed.entries {
        assert_eq!(
            entry.blob_digest.as_deref(),
            Some(format!("sha256:{}", entry.sha256.to_hex()).as_str()),
            "each chunk's blob digest must be its own hash"
        );
        assert_eq!(entry.blob_offset, 0);
    }

    // Docker orchestration unchanged: build → create → export → rm → rmi.
    let calls = docker.calls();
    assert!(
        matches!(calls[0], Call::Build { .. })
            && matches!(calls[1], Call::Create { .. })
            && matches!(calls[2], Call::Export { .. }),
        "expected build → create → export prefix; got {calls:?}",
    );
}

#[tokio::test]
async fn build_directory_format_skips_packer() {
    // Sanity: the default Directory format must NOT invoke the packer
    // (it's only relevant for Ext4 mode). Catches a regression where
    // someone accidentally always packs.
    let src = tempfile::tempdir().unwrap();
    let images = tempfile::tempdir().unwrap();
    write_source_repo(src.path(), "name = \"dir-only\"\n");

    let docker = RecordingDocker::new();
    let packer = RecordingPacker::default();
    let (cs, _csdir) = test_chunk_store();
    let builder = Builder::with_packer(docker, packer.clone(), cs);

    builder
        .build(&req(src.path(), images.path(), "p", "warm-1"))
        .await
        .expect("directory bake should succeed");

    assert!(
        packer.calls().is_empty(),
        "Format::Directory must not invoke the packer; got {:?}",
        packer.calls()
    );
}

#[tokio::test]
async fn build_ext4_propagates_packer_failure_and_keeps_rootfs_for_diagnostics() {
    // If mke2fs fails (out of disk, permission, etc.) we want the
    // staging rootfs/ to stay on disk so the user can rerun the
    // packer manually instead of re-doing the docker steps. Lock
    // that contract in.
    let src = tempfile::tempdir().unwrap();
    let images = tempfile::tempdir().unwrap();
    write_source_repo(src.path(), "name = \"ext4-fails\"\n");

    let docker = RecordingDocker::new()
        .with_fake_rootfs([(PathBuf::from("etc/hostname"), b"engram\n".to_vec())]);
    let packer = RecordingPacker::default();
    packer.fail_next("mocked mke2fs failure");
    let (cs, _csdir) = test_chunk_store();
    let builder = Builder::with_packer(docker, packer, cs);

    let err = builder
        .build(&req_ext4(src.path(), images.path(), "p", "warm-1"))
        .await
        .expect_err("ext4 bake should fail");

    let msg = format!("{err}");
    assert!(
        msg.contains("ext4") && msg.contains("mocked mke2fs failure"),
        "error should surface the packer failure: {msg}",
    );
    let image_dir = images.path().join("p").join("warm-1");
    assert!(
        image_dir.join("rootfs").is_dir(),
        "staging rootfs/ must survive a failed pack for diagnostics",
    );
    assert!(
        !image_dir.join("rootfs.ext4").exists(),
        "rootfs.ext4 should not exist after a failed pack",
    );
}

// ---------------------------------------------------------------------
// AgentInjection
// ---------------------------------------------------------------------

#[tokio::test]
async fn build_directory_with_agent_injection_writes_agent_and_init() {
    // Lock the layout: /sbin/engram-agentd carries the binary
    // verbatim, /sbin/engram-init is a shell script with the
    // negotiated vsock port substituted in. Both 0755.
    use std::os::unix::fs::PermissionsExt;

    let src = tempfile::tempdir().unwrap();
    let images = tempfile::tempdir().unwrap();
    let agent_src = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(agent_src.path(), b"\x7fELF<fake binary>").unwrap();
    write_source_repo(src.path(), "name = \"agent-bake-test\"\n");

    let docker = RecordingDocker::new()
        .with_fake_rootfs([(PathBuf::from("etc/hostname"), b"engram\n".to_vec())]);
    let (cs, _csdir) = test_chunk_store();
    let builder = Builder::new(docker, cs);

    let mut request = req(src.path(), images.path(), "p", "warm-1");
    request.agent_injection = Some(AgentInjection {
        agent_binary: agent_src.path().to_path_buf(),
        vsock_port: 1024,
        init_script: None,
        transport: Default::default(),
    });

    let outcome = builder.build(&request).await.expect("bake");
    let rootfs = &outcome.rootfs_path;

    let agent_dst = rootfs.join("sbin/engram-agentd");
    let init_dst = rootfs.join("sbin/engram-init");
    assert!(agent_dst.is_file(), "agent missing at /sbin/engram-agentd");
    assert!(init_dst.is_file(), "init missing at /sbin/engram-init");

    // Agent bytes match exactly what we passed in.
    let agent_bytes = std::fs::read(&agent_dst).unwrap();
    assert_eq!(agent_bytes, b"\x7fELF<fake binary>");

    // Init has the port + transport substituted; placeholders gone.
    let init_body = std::fs::read_to_string(&init_dst).unwrap();
    assert!(
        init_body.contains("--port 1024"),
        "init should reference agent port 1024: {init_body}",
    );
    assert!(
        init_body.contains("ENGRAM_TRANSPORT=vsock"),
        "default transport export should be vsock: {init_body}",
    );
    assert!(
        !init_body.contains("__VSOCK_PORT__"),
        "port placeholder should be substituted: {init_body}",
    );
    assert!(
        !init_body.contains("__TRANSPORT__"),
        "transport placeholder should be substituted: {init_body}",
    );

    // Both files are world-executable (0755).
    let agent_mode = std::fs::metadata(&agent_dst).unwrap().permissions().mode();
    let init_mode = std::fs::metadata(&init_dst).unwrap().permissions().mode();
    assert_eq!(agent_mode & 0o777, 0o755, "agent mode {agent_mode:o}");
    assert_eq!(init_mode & 0o777, 0o755, "init mode {init_mode:o}");
}

#[tokio::test]
async fn build_with_missing_agent_binary_errors_cleanly() {
    let src = tempfile::tempdir().unwrap();
    let images = tempfile::tempdir().unwrap();
    write_source_repo(src.path(), "name = \"agent-bake-test\"\n");
    let docker = RecordingDocker::new();
    let (cs, _csdir) = test_chunk_store();
    let builder = Builder::new(docker, cs);

    let mut request = req(src.path(), images.path(), "p", "warm-1");
    request.agent_injection = Some(AgentInjection {
        agent_binary: PathBuf::from("/this/path/does/not/exist"),
        vsock_port: 1024,
        init_script: None,
        transport: Default::default(),
    });

    let err = builder.build(&request).await.expect_err("should fail");
    let msg = format!("{err}");
    assert!(
        msg.contains("agent_binary") && msg.contains("does not exist"),
        "error should reference the missing agent binary: {msg}",
    );
}

#[tokio::test]
async fn build_with_init_script_override_uses_provided_script() {
    let src = tempfile::tempdir().unwrap();
    let images = tempfile::tempdir().unwrap();
    let agent_src = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(agent_src.path(), b"\x7fELF").unwrap();
    let init_src = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(init_src.path(), b"#!/bin/sh\necho custom-init\n").unwrap();
    write_source_repo(src.path(), "name = \"agent-bake-test\"\n");

    let docker = RecordingDocker::new();
    let (cs, _csdir) = test_chunk_store();
    let builder = Builder::new(docker, cs);

    let mut request = req(src.path(), images.path(), "p", "warm-1");
    request.agent_injection = Some(AgentInjection {
        agent_binary: agent_src.path().to_path_buf(),
        vsock_port: 1024,
        init_script: Some(init_src.path().to_path_buf()),
        transport: Default::default(),
    });

    let outcome = builder.build(&request).await.expect("bake");
    let init_body = std::fs::read_to_string(outcome.rootfs_path.join("sbin/engram-init")).unwrap();
    assert!(
        init_body.contains("custom-init"),
        "expected override init body: {init_body}",
    );
}

// ---------------------------------------------------------------------
// Real mke2fs end-to-end. Linux + e2fsprogs only.
// ---------------------------------------------------------------------

/// ADR 0036: packing the same logical tree twice — including via two
/// separately-created directories with different file-creation order —
/// must produce byte-identical images. This is the keystone of the
/// delta push/pull pipeline: identical bytes → identical 16 MiB chunk
/// hashes → HEAD-skip on push and exists()-skip on enable. Runs
/// wherever mke2fs is on PATH (Linux CI; macOS dev via the justfile's
/// e2fsprogs PATH entry).
#[tokio::test]
async fn ext4_pack_is_deterministic_across_rebuilds() {
    use engram_image_builder::ext4::Mke2fsPacker;

    let mke2fs_on_path = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|dir| dir.join("mke2fs").is_file()))
        .unwrap_or(false);
    if !mke2fs_on_path {
        eprintln!("mke2fs not on PATH; skipping determinism test");
        return;
    }

    fn write_tree(root: &std::path::Path, order_flipped: bool) {
        std::fs::create_dir_all(root.join("etc")).unwrap();
        std::fs::create_dir_all(root.join("sbin")).unwrap();
        let files: Vec<(&str, Vec<u8>)> = vec![
            ("etc/hostname", b"engram\n".to_vec()),
            ("sbin/engram-agentd", vec![0xAAu8; 256 * 1024]),
            ("greeting", b"hello-from-ext4".to_vec()),
        ];
        // Different creation order across the two trees — catches
        // readdir-order leaking into block allocation.
        let iter: Box<dyn Iterator<Item = &(&str, Vec<u8>)>> = if order_flipped {
            Box::new(files.iter().rev())
        } else {
            Box::new(files.iter())
        };
        for (path, body) in iter {
            std::fs::write(root.join(path), body).unwrap();
        }
    }

    let src_a = tempfile::tempdir().unwrap();
    let src_b = tempfile::tempdir().unwrap();
    write_tree(src_a.path(), false);
    write_tree(src_b.path(), true);

    let img_a = tempfile::NamedTempFile::new().unwrap();
    let img_b = tempfile::NamedTempFile::new().unwrap();
    let packer = Mke2fsPacker::default();
    packer
        .pack(src_a.path(), img_a.path(), 64 * 1024 * 1024)
        .await
        .expect("pack a");
    packer
        .pack(src_b.path(), img_b.path(), 64 * 1024 * 1024)
        .await
        .expect("pack b");

    let hash = |p: &std::path::Path| {
        use sha2::Digest;
        let mut h = sha2::Sha256::new();
        h.update(std::fs::read(p).unwrap());
        format!("{:x}", h.finalize())
    };
    assert_eq!(
        hash(img_a.path()),
        hash(img_b.path()),
        "ADR 0036: identical trees must pack to byte-identical ext4 images \
         (fixed UUID + hash_seed + SOURCE_DATE_EPOCH)"
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn ext4_pack_with_real_mke2fs_produces_mountable_image() {
    use engram_image_builder::ext4::Mke2fsPacker;

    let mke2fs_on_path = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|dir| dir.join("mke2fs").is_file()))
        .unwrap_or(false);
    if !mke2fs_on_path {
        eprintln!("mke2fs not on PATH; skipping real-packer test");
        return;
    }

    let src = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(src.path().join("etc")).unwrap();
    std::fs::write(src.path().join("etc/hostname"), "engram\n").unwrap();
    std::fs::write(src.path().join("greeting"), "hello-from-ext4").unwrap();

    let dst = tempfile::NamedTempFile::new().unwrap();
    Mke2fsPacker::default()
        .pack(src.path(), dst.path(), 64 * 1024 * 1024)
        .await
        .expect("real mke2fs should succeed");

    let meta = std::fs::metadata(dst.path()).unwrap();
    assert_eq!(meta.len(), 64 * 1024 * 1024);

    // The file's first 1024 bytes should be ext4's superblock —
    // magic 0xEF53 lives at offset 0x438 (1080) inside the FS.
    let mut buf = vec![0u8; 1100];
    use std::io::Read;
    let mut f = std::fs::File::open(dst.path()).unwrap();
    f.read_exact(&mut buf).unwrap();
    let magic = u16::from_le_bytes([buf[1080], buf[1081]]);
    assert_eq!(
        magic, 0xEF53,
        "expected ext4 superblock magic at offset 1080"
    );
}
