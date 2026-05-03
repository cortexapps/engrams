//! Image baker.
//!
//! Source of truth for an image is a repo containing two files:
//!
//! ```text
//!   <repo>/
//!     Dockerfile          # WHAT'S in the image — universal Docker syntax
//!     engram.toml         # HOW the image is USED — secrets, network, resources
//! ```
//!
//! `engram-image-builder build` reads both, runs `docker build`, exports
//! the resulting OCI image's filesystem to the registry layout the
//! coordinator reads:
//!
//! ```text
//!   <images_dir>/<repo>/<tag>/
//!     manifest.toml       # rendered ImageManifest (engram.toml minus [build])
//!     rootfs/             # ProcessBackend dev path
//!     rootfs.ext4         # FirecrackerBackend prod path (Phase 2)
//! ```
//!
//! For ProcessBackend dev, we extract via `docker create + docker
//! export | tar -x`. For Firecracker prod (Phase 2), the same pipe
//! continues into `mkfs.ext4` and bakes in `engram-agentd` + an init
//! unit. Out of scope this round.

pub mod config;
pub mod docker;
pub mod ext4;

use std::path::{Path, PathBuf};

use chrono::Utc;
use engram_core::traits::MetadataStore;
use engram_core::types::{ImageManifest, ImageStatus, ImageVersion};
use engram_core::ImageVersionId;

pub use config::{BuildConfig, EngramRepoConfig};
pub use docker::{DockerCli, DockerRunner};
pub use ext4::{recommended_size, Ext4Error, Ext4Packer, Mke2fsPacker};

/// Output format the baker should produce.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Format {
    /// `<image_dir>/rootfs/` — directory tree. Consumed by
    /// `ProcessBackend` (the dev backend on macOS / non-KVM Linux).
    #[default]
    Directory,
    /// `<image_dir>/rootfs.ext4` — ext4 filesystem image. Consumed by
    /// `FirecrackerBackend` as a block device.
    Ext4,
}

/// What the user wants the baker to do.
#[derive(Clone, Debug)]
pub struct BuildRequest {
    /// Path to the source repo (containing `Dockerfile` and `engram.toml`).
    pub source: PathBuf,
    /// Repo identifier in the registry — typically `<org>/<name>`.
    pub repo: String,
    /// Tag for the produced image. Convention: `warm-<rfc3339>` so the
    /// coordinator's "latest ready" lookup picks it up by created_at.
    pub tag: String,
    /// Where the registry lives (the same root the coordinator reads).
    pub images_dir: PathBuf,
    /// Output format — `Directory` for the dev backend, `Ext4` for
    /// Firecracker. Defaults to `Directory` so existing callers keep
    /// working.
    pub format: Format,
    /// Optional: bake `engram-agentd` into the rootfs at
    /// `/sbin/engram-agentd` plus a small init shim at
    /// `/sbin/engram-init` that mounts the essentials and exec's
    /// the agent on a vsock port. When `Some`, the resulting image
    /// boots straight into the agent — pair with FC's
    /// `default_boot_args = "... init=/sbin/engram-init"` and the
    /// host's `exec_stream` reaches the in-guest agent over vsock.
    pub agent_injection: Option<AgentInjection>,
}

/// How to put `engram-agentd` inside the rootfs at bake time. Optional
/// because the dev backend doesn't need it — only Firecracker images
/// do.
#[derive(Clone, Debug)]
pub struct AgentInjection {
    /// Linux-built `engram-agentd` binary on the host. Copied verbatim
    /// to `/sbin/engram-agentd` inside the rootfs and chmod'd 0755.
    pub agent_binary: PathBuf,
    /// Vsock port the agent should listen on inside the guest. Pair
    /// this with the host-side `ENGRAM_AGENTD_PORT` constant
    /// (`engram_sandbox_firecracker::ENGRAM_AGENTD_PORT`, currently
    /// 1024).
    pub vsock_port: u32,
    /// Which host↔guest transport the in-VM binaries (agentd,
    /// bootstrap, harness) should use. The default init shim sets
    /// `ENGRAM_TRANSPORT=...` accordingly so `engram-transport`'s
    /// runtime factory picks the matching impl. Defaults to
    /// `Vsock` for back-compat with FC bakes that predate this
    /// field.
    pub transport: Transport,
    /// Override the default init script. When `None`, the baker
    /// writes a minimal `/bin/sh` shim that mounts `/proc`, `/sys`,
    /// `/dev`, spawns any baked-in sidecars (engram-bootstrap), and
    /// `exec`s `engram-agentd --port <port>`.
    pub init_script: Option<PathBuf>,
    /// Optional `engram-bootstrap` binary. When provided, baked at
    /// `/sbin/engram-bootstrap` (chmod 0755) and the default init
    /// shim spawns it in the background before exec'ing agentd.
    /// Required for any image that wants to run a harness over the
    /// in-VM transport; safe to omit for images that only need the
    /// exec channel.
    #[allow(dead_code)] // surfaced via this struct's field so callers can construct it
    pub bootstrap_binary: Option<PathBuf>,
}

/// Which `engram-transport` implementation the in-VM binaries
/// should select at runtime. Set on [`AgentInjection`] at bake time;
/// the default init shim writes `ENGRAM_TRANSPORT=<value>` into the
/// rootfs so `engram-transport::from_env` picks the right impl.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Transport {
    /// AF_VSOCK (Linux + Firecracker). Default — matches every FC
    /// bake we've shipped.
    #[default]
    Vsock,
    /// virtio-console (Apple Virtualization.framework on
    /// macOS). Selected by the vz-bake-* recipes.
    Console,
}

impl Transport {
    /// Value for `ENGRAM_TRANSPORT` in the init shim. Lowercase
    /// matches what `engram-transport::from_env` parses.
    pub fn env_value(self) -> &'static str {
        match self {
            Self::Vsock => "vsock",
            Self::Console => "console",
        }
    }

    /// Parse a CLI flag value (case-insensitive). Used by
    /// `engram-cli image build --transport=...`.
    pub fn parse(s: &str) -> Result<Self, String> {
        match s.to_ascii_lowercase().as_str() {
            "vsock" => Ok(Self::Vsock),
            "console" | "virtio-console" => Ok(Self::Console),
            other => Err(format!(
                "invalid transport: {other} (expected vsock|console)"
            )),
        }
    }
}

/// Where the agent binary lands inside the rootfs (relative to root).
const AGENT_PATH: &str = "sbin/engram-agentd";

/// Where the init shim lands inside the rootfs (relative to root).
/// Pair with kernel boot arg `init=/sbin/engram-init`.
const INIT_PATH: &str = "sbin/engram-init";

/// Placeholder in [`DEFAULT_INIT_SHIM`] swapped for the agent port
/// at bake time. Named for the original vsock-only world; retained
/// to keep this string stable across the multi-transport refactor.
const VSOCK_PORT_PLACEHOLDER: &str = "__VSOCK_PORT__";

/// Placeholder swapped for `ENGRAM_TRANSPORT=vsock|console` at
/// bake time so `engram-transport::from_env` in the in-VM binaries
/// picks the matching impl.
const TRANSPORT_PLACEHOLDER: &str = "__TRANSPORT__";

/// Default init shim. Written to `/sbin/engram-init` when an
/// [`AgentInjection`] is requested without an explicit override.
/// Requires `/bin/sh` in the rootfs (alpine, debian-slim, ubuntu —
/// all standard bases ship it). When `bootstrap_binary` is set on
/// the injection, the shim spawns it in the background before
/// exec'ing agentd; `[ -x ... ] &&` keeps it tolerant of images
/// baked without bootstrap.
///
/// The exported `ENGRAM_TRANSPORT` env propagates to engram-bootstrap
/// (forked here) and to engram-agentd (exec'd at the bottom). It
/// also rides through `BootstrapLaunch.env` to harness adapters
/// when bootstrap re-execs them, so all four in-VM binaries see a
/// consistent transport selection.
const DEFAULT_INIT_SHIM: &str = r#"#!/bin/sh
# engram-init — minimal init shim. Brings up just enough kernel
# plumbing for the in-VM binaries to talk to the host, then exec's
# engram-agentd.
set -e
mount -t proc  proc /proc 2>/dev/null || true
mount -t sysfs sys  /sys  2>/dev/null || true
mount -t devtmpfs dev /dev 2>/dev/null || true
# devpts is required for PTY allocation (`forkpty` / `posix_openpt`).
# Without `/dev/pts/` ttyd fails with `pty_spawn: ENOENT` even though
# `/dev/ptmx` is present, because the kernel needs the slave-side
# nodes to materialise here. Standard Linux init does this.
mkdir -p /dev/pts 2>/dev/null || true
mount -t devpts devpts /dev/pts 2>/dev/null || true
# DNS for userspace. The kernel handled IP+routes via `ip=dhcp` (see
# vz-backend kernel cmdline); IP_PNP doesn't write resolv.conf, so
# we do it here. 192.168.64.1 is the VZ NAT gateway, which Apple's
# network stack also answers DNS on. 1.1.1.1 is a public fallback in
# case the gateway resolver is unreachable (e.g. on Linux/FC where
# the bridge isn't VZ NAT). Writing both is safe — glibc tries them
# in order. Skip if /etc/resolv.conf already exists (operator override).
mkdir -p /etc
if [ ! -s /etc/resolv.conf ]; then
    printf 'nameserver 192.168.64.1\nnameserver 1.1.1.1\n' > /etc/resolv.conf
fi
# Harness substrate: read-only ext4 image attached as the second
# virtio-blk drive on every sandbox (`/dev/vdb`). Same wire on FC
# and VZ. The image is built once on the host from `cfg.harnesses_dir`;
# `engram-bootstrap` exec's `/run/engram/harnesses/<name>/harness`
# from here. If `/dev/vdb` isn't present (sandbox booted without a
# substrate), the mkdir leaves an empty dir and the bootstrap
# harness lookup will fail with a clear message.
mkdir -p /run/engram/harnesses /workspace 2>/dev/null || true
if [ -b /dev/vdb ]; then
    mount -t ext4 -o ro /dev/vdb /run/engram/harnesses 2>/dev/null || true
fi
export ENGRAM_TRANSPORT=__TRANSPORT__
# Diagnostic: dump virtio-port + hvc device layout so a misconfig is
# obvious from the kernel boot log. Cheap (one-shot, only at init).
# engram-init: pre-flight diagnostics. Quiet on the happy path
# (ENGRAM_INIT_DEBUG=0); operators set ENGRAM_INIT_DEBUG=1 in
# the bake's BootstrapLaunch.env to see /sys/class/virtio-ports
# enumeration when bringing up a new kernel build.
if [ "${ENGRAM_INIT_DEBUG:-0}" = "1" ]; then
    echo "engram-init: ENGRAM_TRANSPORT=$ENGRAM_TRANSPORT" >&2
    for p in /sys/class/virtio-ports/*; do
        [ -d "$p" ] || continue
        n=$(cat "$p/name" 2>/dev/null || echo "<unnamed>")
        d=$(cat "$p/dev" 2>/dev/null || echo "<no-dev>")
        echo "  $(basename $p) name=$n dev=$d" >&2
    done
fi
# In-guest ttyd: serves an interactive bash session over WebSocket on
# :7681. The coordinator's `GET /sessions/:id/shell` proxy bridges
# browser <-> ttyd, ghostty-web on the browser side renders it. We
# don't fail the boot if the binary is missing — older images that
# predate the shell feature continue to work, the dashboard's SHELL
# tab just shows "shell unavailable" for those sessions.
#
# /bin/sh is the safe-everywhere fallback (always present; debian-slim
# bases ship dash). If the image happens to also include bash —
# node:20-slim does at /usr/bin/bash — prefer it for an interactive
# experience that matches what users expect from a terminal.
# Absolute path required because PID 1's environment doesn't carry a
# PATH and ttyd uses execvp to find the shell.
#
# We also export HOME / USER / PATH before launching ttyd. The
# kernel's PID-1 env doesn't include these, and without them bash
# resolves `~/.bashrc` to `/.bashrc` (does not exist) and skips it —
# losing the prompt + ls-color aliases we baked into /root/.bashrc.
# Setting them here is enough; bash inherits them through ttyd.
if [ -x /usr/local/bin/ttyd ]; then
    SHELL_BIN=/bin/sh
    [ -x /usr/bin/bash ] && SHELL_BIN=/usr/bin/bash
    export HOME=/root
    export USER=root
    export PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
    (cd /workspace && /usr/local/bin/ttyd -W -p 7681 "$SHELL_BIN" >/var/log/ttyd.log 2>&1) &
fi
[ -x /sbin/engram-bootstrap ] && /sbin/engram-bootstrap &
exec /sbin/engram-agentd --port __VSOCK_PORT__
"#;

#[derive(Clone, Debug)]
pub struct BuildOutcome {
    pub image_dir: PathBuf,
    pub manifest_path: PathBuf,
    pub rootfs_path: PathBuf,
    pub size_bytes: u64,
}

#[derive(Debug)]
pub enum BuildError {
    Config(String),
    Io(std::io::Error),
    Docker(String),
    Ext4(Ext4Error),
    Persist(engram_core::MetaError),
    InvalidPath(String),
}

impl std::fmt::Display for BuildError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Config(m) => write!(f, "engram.toml: {m}"),
            Self::Io(e) => write!(f, "io: {e}"),
            Self::Docker(m) => write!(f, "docker: {m}"),
            Self::Ext4(e) => write!(f, "ext4: {e}"),
            Self::Persist(e) => write!(f, "metadata: {e}"),
            Self::InvalidPath(p) => write!(f, "invalid path: {p}"),
        }
    }
}

impl std::error::Error for BuildError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            Self::Ext4(e) => Some(e),
            Self::Persist(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for BuildError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<Ext4Error> for BuildError {
    fn from(e: Ext4Error) -> Self {
        Self::Ext4(e)
    }
}

/// The baker. Composed of a [`DockerRunner`] (mockable) for `docker
/// build/create/export`, an [`Ext4Packer`] (mockable) for the
/// `Format::Ext4` step, and optionally a [`MetadataStore`] for
/// recording the produced row.
pub struct Builder<D: DockerRunner, P: Ext4Packer = Mke2fsPacker> {
    docker: D,
    packer: P,
}

impl<D: DockerRunner> Builder<D, Mke2fsPacker> {
    /// Default constructor: real `mke2fs` packer for `Format::Ext4`.
    pub fn new(docker: D) -> Self {
        Self {
            docker,
            packer: Mke2fsPacker::default(),
        }
    }
}

impl<D: DockerRunner, P: Ext4Packer> Builder<D, P> {
    /// Construct with a custom packer — used by tests to record /
    /// inject errors instead of running real mke2fs.
    pub fn with_packer(docker: D, packer: P) -> Self {
        Self { docker, packer }
    }

    /// Run a single bake. Steps:
    ///
    /// 1. Validate paths in `req`.
    /// 2. Read `<source>/engram.toml`, split into manifest + build.
    /// 3. `docker build` against the source.
    /// 4. `docker create` a throwaway container; `docker export` its
    ///    filesystem to the staging tarball.
    /// 5. Extract the tarball into `<images_dir>/<repo>/<tag>/rootfs/`.
    /// 6. Write `manifest.toml`.
    /// 7. Best-effort `docker rm <container>` + remove the temporary tag.
    ///
    /// Idempotent on the registry side — overwrites any existing
    /// `<repo>/<tag>` directory. Postgres recording is delegated to
    /// [`Self::record_in_metadata`].
    pub async fn build(&self, req: &BuildRequest) -> Result<BuildOutcome, BuildError> {
        // 1. Path safety — reject `..` segments in repo / tag so a
        //    typo can't escape the registry root.
        for component in [req.repo.as_str(), req.tag.as_str()] {
            if component.is_empty() || component.contains("..") || component.starts_with('/') {
                return Err(BuildError::InvalidPath(component.to_string()));
            }
        }
        let source = req.source.canonicalize().map_err(|e| {
            BuildError::InvalidPath(format!("source {}: {e}", req.source.display()))
        })?;
        let image_dir = req.images_dir.join(&req.repo).join(&req.tag);

        // 2. Read engram.toml.
        let cfg_path = source.join("engram.toml");
        let cfg = EngramRepoConfig::read_from(&cfg_path)?;

        // 3. Pick a unique throwaway tag for the docker build artefact
        //    so concurrent bakes don't trample each other.
        let docker_tag = format!("engram-bake-{}", uuid::Uuid::new_v4().simple());
        let dockerfile = source.join(&cfg.build.dockerfile);
        if !dockerfile.exists() {
            return Err(BuildError::Config(format!(
                "Dockerfile not found at {}",
                dockerfile.display()
            )));
        }
        let context = source.join(&cfg.build.context);

        tracing::info!(repo = %req.repo, tag = %req.tag, ?dockerfile, "running docker build");
        self.docker
            .build(docker::BuildArgs {
                context: context.clone(),
                dockerfile: dockerfile.clone(),
                tag: docker_tag.clone(),
                build_args: cfg.build.args.clone(),
            })
            .await
            .map_err(|e| BuildError::Docker(format!("build: {e}")))?;

        // 4. Create container. If create fails the only artefact is
        //    the build-tagged image, so just rmi that and bail.
        let container_id = match self.docker.create(&docker_tag).await {
            Ok(id) => id,
            Err(e) => {
                let _ = self.docker.rmi(&docker_tag).await;
                return Err(BuildError::Docker(format!("create: {e}")));
            }
        };

        // 5. Everything from here owns both the container and the
        //    image — guarantee cleanup runs whether export succeeds
        //    or fails. We don't surface cleanup errors; the bake's
        //    outcome is what matters.
        let outcome = self
            .build_after_container(req, &cfg, &image_dir, &container_id)
            .await;
        let _ = self.docker.rm_container(&container_id).await;
        let _ = self.docker.rmi(&docker_tag).await;

        outcome.inspect(|o| {
            tracing::info!(
                repo = %req.repo,
                tag = %req.tag,
                size_bytes = o.size_bytes,
                "bake complete",
            );
        })
    }

    /// Steps 5–6 of `build`: export, write manifest, optionally pack
    /// to ext4, compute size. Factored out so the unconditional
    /// cleanup at the bottom of `build` is symmetrical regardless of
    /// where this fails.
    async fn build_after_container(
        &self,
        req: &BuildRequest,
        cfg: &EngramRepoConfig,
        image_dir: &Path,
        container_id: &str,
    ) -> Result<BuildOutcome, BuildError> {
        tokio::fs::create_dir_all(image_dir).await?;
        let rootfs_dir = image_dir.join("rootfs");
        // Wipe any prior contents — bake is overwrite-semantics.
        let _ = tokio::fs::remove_dir_all(&rootfs_dir).await;
        let _ = tokio::fs::remove_file(image_dir.join("rootfs.ext4")).await;
        tokio::fs::create_dir_all(&rootfs_dir).await?;

        self.docker
            .export_to_dir(container_id, &rootfs_dir)
            .await
            .map_err(|e| BuildError::Docker(format!("export: {e}")))?;

        // Optional: inject engram-agentd + bootstrap + an init shim
        // before we pack to ext4. Done after docker export so the
        // rootfs the user described in their Dockerfile is the base;
        // we just overlay our agent / bootstrap on top. Harness
        // binaries are NOT baked here — they live host-side in
        // `cfg.harnesses_dir` and are mounted into every sandbox
        // via virtio-fs at `/run/engram/harnesses`.
        let effective_manifest = cfg.to_manifest();
        if let Some(injection) = &req.agent_injection {
            inject_agent(&rootfs_dir, injection).await?;
        }

        let manifest_path = image_dir.join("manifest.toml");
        let manifest_str = render_manifest_value(&effective_manifest)?;
        tokio::fs::write(&manifest_path, manifest_str).await?;

        let dir_size = recursive_size(&rootfs_dir).await?;

        let (rootfs_path, total_size) = match req.format {
            Format::Directory => (rootfs_dir, dir_size),
            Format::Ext4 => {
                // Format the staging dir into a single ext4 image,
                // then drop the staging dir — Firecracker only needs
                // the .ext4. Doing the wipe AFTER pack succeeds means
                // a failed mke2fs leaves the directory in place for
                // diagnostics.
                let ext4_path = image_dir.join("rootfs.ext4");
                let ext4_size = recommended_size(dir_size);
                self.packer.pack(&rootfs_dir, &ext4_path, ext4_size).await?;
                let _ = tokio::fs::remove_dir_all(&rootfs_dir).await;
                let on_disk = tokio::fs::metadata(&ext4_path).await?.len();
                (ext4_path, on_disk)
            }
        };

        Ok(BuildOutcome {
            image_dir: image_dir.to_path_buf(),
            manifest_path,
            rootfs_path,
            size_bytes: total_size,
        })
    }

    /// Record (or refresh) a `image_versions` row marking the bake as
    /// `Ready`. Callers that don't want a Postgres write skip this.
    pub async fn record_in_metadata(
        &self,
        meta: &dyn MetadataStore,
        req: &BuildRequest,
        outcome: &BuildOutcome,
    ) -> Result<(), BuildError> {
        let _ = outcome; // size_bytes / blob_url will be persisted later
        meta.upsert_image_version(ImageVersion {
            id: ImageVersionId::new(),
            repo: req.repo.clone(),
            tag: req.tag.clone(),
            blob_url: None,
            status: ImageStatus::Ready,
            created_at: Utc::now(),
        })
        .await
        .map_err(BuildError::Persist)
    }
}

/// Render the manifest as TOML. Strips the `[build]` section since
/// runtime doesn't need it (already filtered out by
/// `EngramRepoConfig::to_manifest`).
fn render_manifest_value(manifest: &ImageManifest) -> Result<String, BuildError> {
    toml::to_string_pretty(manifest)
        .map_err(|e| BuildError::Config(format!("render manifest: {e}")))
}

/// Copy the agent binary into `<rootfs>/sbin/engram-agentd`, write the
/// init shim to `<rootfs>/sbin/engram-init`, and chmod 0755 on both.
/// Mirrors the layout the kernel boot args expect:
/// `init=/sbin/engram-init`.
///
/// Harness binaries used to be baked here too. They've moved
/// host-side: `cfg.harnesses_dir` is mounted into every sandbox at
/// `/run/engram/harnesses` via virtio-fs, so the rootfs no longer
/// carries them.
async fn inject_agent(rootfs_dir: &Path, injection: &AgentInjection) -> Result<(), BuildError> {
    if !injection.agent_binary.exists() {
        return Err(BuildError::Config(format!(
            "agent_binary {} does not exist",
            injection.agent_binary.display()
        )));
    }

    let agent_dst = rootfs_dir.join(AGENT_PATH);
    let init_dst = rootfs_dir.join(INIT_PATH);
    if let Some(parent) = agent_dst.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    install_file(&injection.agent_binary, &agent_dst, "agent").await?;

    if let Some(bootstrap) = injection.bootstrap_binary.as_deref() {
        if !bootstrap.exists() {
            return Err(BuildError::Config(format!(
                "bootstrap_binary {} does not exist",
                bootstrap.display()
            )));
        }
        let dst = rootfs_dir.join("sbin/engram-bootstrap");
        install_file(bootstrap, &dst, "bootstrap").await?;
    }

    match &injection.init_script {
        Some(src) => install_file(src, &init_dst, "init").await?,
        None => {
            let body = DEFAULT_INIT_SHIM
                .replace(VSOCK_PORT_PLACEHOLDER, &injection.vsock_port.to_string())
                .replace(TRANSPORT_PLACEHOLDER, injection.transport.env_value());
            tokio::fs::write(&init_dst, body).await?;
            chmod_executable(&init_dst).await?;
        }
    }

    Ok(())
}

/// Copy `src` to `dst` and chmod 0755. `label` is a short tag ("agent",
/// "init") used in the error message so a copy failure points at the
/// caller's intent.
async fn install_file(src: &Path, dst: &Path, label: &str) -> Result<(), BuildError> {
    tokio::fs::copy(src, dst).await.map_err(|e| {
        std::io::Error::new(
            e.kind(),
            format!("copy {label} {} -> {}: {e}", src.display(), dst.display()),
        )
    })?;
    chmod_executable(dst).await
}

async fn chmod_executable(path: &Path) -> Result<(), BuildError> {
    use std::os::unix::fs::PermissionsExt;
    tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).await?;
    Ok(())
}

async fn recursive_size(dir: &Path) -> std::io::Result<u64> {
    let mut total = 0u64;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let mut entries = match tokio::fs::read_dir(&d).await {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
        };
        while let Some(entry) = entries.next_entry().await? {
            let meta = entry.metadata().await?;
            if meta.is_dir() {
                stack.push(entry.path());
            } else if meta.is_file() {
                total = total.saturating_add(meta.len());
            }
        }
    }
    Ok(total)
}
