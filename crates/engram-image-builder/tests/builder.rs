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
use engram_image_builder::docker::{BuildArgs, DockerError, DockerRunner};
use engram_image_builder::{BuildRequest, Builder};

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

fn req(source: &Path, images_dir: &Path, repo: &str, tag: &str) -> BuildRequest {
    BuildRequest {
        source: source.to_path_buf(),
        repo: repo.into(),
        tag: tag.into(),
        images_dir: images_dir.to_path_buf(),
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

    let docker = RecordingDocker::new().with_fake_rootfs(vec![
        (PathBuf::from("README.md"), b"# starter\n".to_vec()),
        (PathBuf::from("scripts/run.sh"), b"#!/bin/sh\n".to_vec()),
    ]);
    let builder = Builder::new(docker.clone());

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
    let builder = Builder::new(docker.clone());
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
    let builder = Builder::new(docker.clone());
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
    let builder = Builder::new(RecordingDocker::new());

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
    let builder = Builder::new(RecordingDocker::new());
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
    let builder = Builder::new(RecordingDocker::new());
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
    let builder = Builder::new(docker.clone());
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
    let builder = Builder::new(docker.clone());
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
    let builder = Builder::new(docker.clone());
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
    let builder = Builder::new(docker);
    builder
        .build(&req(src.path(), images.path(), "x", "warm-same"))
        .await
        .unwrap();

    // Second bake with different fake rootfs.
    let docker2 = RecordingDocker::new()
        .with_fake_rootfs(vec![(PathBuf::from("v2-only.txt"), b"yo".to_vec())]);
    let builder2 = Builder::new(docker2);
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

    let builder = Builder::new(RecordingDocker::new());
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
    let builder = Builder::new(docker);
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
