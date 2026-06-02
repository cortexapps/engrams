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

pub mod blob;
pub mod config;
pub mod docker;
pub mod ext4;
pub mod harness;

use std::path::{Path, PathBuf};

use engram_core::types::ImageManifest;

pub use config::{BuildConfig, EngramRepoConfig};
pub use docker::{DockerCli, DockerRunner};
pub use ext4::{recommended_size, Ext4Error, Ext4Packer, Mke2fsPacker};
pub use harness::{BuiltinCatalog, Platform};

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
    /// ADR 0008 Phase 4: parent image's bootstrap, for cross-image
    /// chunk dedup. When `Some`, the disk-side bootstrap walks the
    /// parent's entries and reuses any chunk hashes that match —
    /// the child's diff blob then only contains chunks unique to
    /// this image. Must be paired with `parent_chunks_blob_digest`
    /// — the OCI layer digest of the parent's chunks blob, which
    /// the child's bootstrap entries inherit for shared chunks.
    ///
    /// `None` (default) produces a fully self-contained chunk blob
    /// (today's behavior).
    pub parent_disk_bootstrap_path: Option<PathBuf>,
    /// OCI layer digest of the parent's disk chunks blob. Required
    /// when `parent_disk_bootstrap_path` is set; ignored otherwise.
    pub parent_disk_chunks_blob_digest: Option<String>,
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
    /// `/dev`, and `exec`s `engram-agentd --port <port>`.
    pub init_script: Option<PathBuf>,
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
/// all standard bases ship it).
///
/// The exported `ENGRAM_TRANSPORT` env propagates to engram-agentd
/// (exec'd at the bottom) and to any harness child agentd spawns
/// via `WireRequest::SpawnHarness`, so all in-VM binaries see a
/// consistent transport selection.
const DEFAULT_INIT_SHIM: &str = r#"#!/bin/sh
# engram-init — minimal init shim. Brings up just enough kernel
# plumbing for the in-VM binaries to talk to the host, then exec's
# engram-agentd.
set -e
mount -t proc  proc /proc 2>/dev/null || true
# ADR 0019: cheap boot-phase timing markers. /proc/uptime's first field is
# seconds since kernel boot, so each value is cumulative kernel-relative
# time and the deltas between marks are phase durations. Emitted to the
# guest console (lands in the host's per-sandbox firecracker.log); a
# follow-up can have host-agent forward `engram-init: mark` lines into
# Cloud Logging. Until then they're a dev-vm spike aid (read over SSH).
mark() { echo "engram-init: mark $1 uptime=$(cut -d' ' -f1 /proc/uptime 2>/dev/null || echo '?')" >&2; }
# ~kernel boot + rootfs ext4 mount (the chunked-NBD page-in window) up to
# the first userspace instruction.
mark kernel_to_init
mount -t sysfs sys  /sys  2>/dev/null || true
mount -t devtmpfs dev /dev 2>/dev/null || true
# devpts is required for PTY allocation (`forkpty` / `posix_openpt`).
# Without `/dev/pts/` ttyd fails with `pty_spawn: ENOENT` even though
# `/dev/ptmx` is present, because the kernel needs the slave-side
# nodes to materialise here. Standard Linux init does this.
mkdir -p /dev/pts 2>/dev/null || true
mount -t devpts devpts /dev/pts 2>/dev/null || true
mark fs_mounts_done
# DNS for userspace. The kernel handled IP+routes via `ip=dhcp` (see
# vz-backend kernel cmdline); IP_PNP doesn't write resolv.conf, so
# we do it here. 192.168.64.1 is the VZ NAT gateway, which Apple's
# network stack also answers DNS on. 1.1.1.1 is a public fallback in
# case the gateway resolver is unreachable (e.g. on Linux/FC where
# the bridge isn't VZ NAT). Writing both is safe — glibc tries them
# in order. Skip if /etc/resolv.conf already exists (operator override).
#
# FIXME(dns-exfil): the egress proxy enforces `manifest.network.
# allow_hosts` for outbound *connections*, but DNS itself goes
# straight to 1.1.1.1. A malicious harness can encode data into
# subdomains of an attacker-controlled name and exfiltrate via DNS
# queries even when the proxy blocks every TCP connection. Fix:
# host the egress proxy on udp/53 as well, iptables-REDIRECT
# guest→udp/53 there, and have it answer only for names in
# `allow_hosts` (NXDOMAIN otherwise). For FC, that lets us also
# drop the `ACCEPT VM→1.1.1.1 udp/53` rule. Until that lands, the
# DNS path is an unfiltered side channel.
mkdir -p /etc
if [ ! -s /etc/resolv.conf ]; then
    printf 'nameserver 192.168.64.1\nnameserver 1.1.1.1\n' > /etc/resolv.conf
fi
# /etc/hosts: a slim rootfs (debian-slim etc.) ships an empty one, so
# `localhost` has no entry and `nsswitch` (files then dns) falls through to
# the nameservers above — which the FC guest can't reach — and anything that
# binds or dials localhost fails with "lookup localhost ... no such host".
# That breaks the in-guest `just dev` loop (tilt, the coordinator, the web
# dev server). Seed the loopback names if /etc/hosts is empty.
if [ ! -s /etc/hosts ]; then
    printf '127.0.0.1\tlocalhost\n::1\tlocalhost ip6-localhost ip6-loopback\n' > /etc/hosts
fi
# ADR 0014 M1.12 (option D) + ADR 0015 M1: engram-init no longer
# leaves a persistent mount of the harness substrate at
# /run/engram/harnesses. The host's SpawnHarness frame nominates
# the harness device + mount point and agentd does the mount
# itself; this lets warm-pool templates be harness-agnostic — the
# bake snapshot captures agentd on accept() before any harness
# mount has happened, then `PATCH /drives` per-session swaps the
# device's backing file (see crates/engram-sandbox-firecracker/
# tests/patch_drive_swap.rs).
#
# But cold-create sessions also need the egress-proxy CA from
# /.engram-host/ca.pem on the harness substrate. We tmp-mount
# /dev/vdb, copy the CA into a rootfs-persistent location, and
# unmount immediately — the block-device page cache is still
# invalidated post-PATCH so agentd's mount sees the swapped
# contents on the warm path.
mkdir -p /run/engram/harnesses /workspace 2>/dev/null || true
if [ -b /dev/vdb ]; then
    mkdir -p /run/engram/.ca-stage 2>/dev/null || true
    if mount -t ext4 -o ro /dev/vdb /run/engram/.ca-stage 2>/dev/null; then
        if [ -f /run/engram/.ca-stage/.engram-host/ca.pem ]; then
            mkdir -p /etc/engram /etc/ssl/certs 2>/dev/null || true
            cp /run/engram/.ca-stage/.engram-host/ca.pem /etc/engram/ca.pem 2>/dev/null || true
            if [ -f /etc/ssl/certs/ca-certificates.crt ]; then
                cat /etc/engram/ca.pem >> /etc/ssl/certs/ca-certificates.crt 2>/dev/null || true
            else
                cp /etc/engram/ca.pem /etc/ssl/certs/ca-certificates.crt 2>/dev/null || true
            fi
            export SSL_CERT_FILE=/etc/ssl/certs/ca-certificates.crt
            export CURL_CA_BUNDLE=/etc/ssl/certs/ca-certificates.crt
            export REQUESTS_CA_BUNDLE=/etc/ssl/certs/ca-certificates.crt
            export NODE_EXTRA_CA_CERTS=/etc/engram/ca.pem
        fi
        umount /run/engram/.ca-stage 2>/dev/null || true
    fi
    rmdir /run/engram/.ca-stage 2>/dev/null || true
fi
mark ca_staged
# ADR 0027: mount read-only host bundles (skills / playwright squashfs)
# the host attached as extra virtio-blk drives. The device letter
# depends on attach order (and whether the legacy CA ext4 drive above
# took /dev/vdb), so we PROBE the non-root block devices and identify
# each bundle by a content marker rather than hard-coding a letter —
# order-independent and robust across snapshot/restore. squashfs-only,
# so a probe never accidentally mounts the ext4 CA drive. The mounts are
# captured in the base snapshot's VFS and resume unchanged (same bytes,
# same fleet-canonical path). Best-effort: a missing/absent bundle just
# leaves the mount point empty; agentd degrades gracefully.
for dev in /dev/vdb /dev/vdc /dev/vdd /dev/vde; do
    [ -b "$dev" ] || continue
    mkdir -p /opt/engram/.probe 2>/dev/null || true
    mount -t squashfs -o ro "$dev" /opt/engram/.probe 2>/dev/null || continue
    if [ -x /opt/engram/.probe/bin/engram-share ]; then
        umount /opt/engram/.probe 2>/dev/null || true
        mkdir -p /opt/engram/skills 2>/dev/null || true
        mount -t squashfs -o ro "$dev" /opt/engram/skills 2>/dev/null || true
    elif [ -x /opt/engram/.probe/launch-mcp ]; then
        umount /opt/engram/.probe 2>/dev/null || true
        mkdir -p /opt/engram/browser 2>/dev/null || true
        mount -t squashfs -o ro "$dev" /opt/engram/browser 2>/dev/null || true
    else
        umount /opt/engram/.probe 2>/dev/null || true
    fi
done
rmdir /opt/engram/.probe 2>/dev/null || true
mark bundles_mounted
export ENGRAM_TRANSPORT=__TRANSPORT__
# Diagnostic: dump virtio-port + hvc device layout so a misconfig is
# obvious from the kernel boot log. Cheap (one-shot, only at init).
# engram-init: pre-flight diagnostics. Quiet on the happy path
# (ENGRAM_INIT_DEBUG=0); operators set ENGRAM_INIT_DEBUG=1 in
# the kernel cmdline to see /sys/class/virtio-ports enumeration
# when bringing up a new kernel build.
if [ "${ENGRAM_INIT_DEBUG:-0}" = "1" ]; then
    echo "engram-init: ENGRAM_TRANSPORT=$ENGRAM_TRANSPORT" >&2
    for p in /sys/class/virtio-ports/*; do
        [ -d "$p" ] || continue
        n=$(cat "$p/name" 2>/dev/null || echo "<unnamed>")
        d=$(cat "$p/dev" 2>/dev/null || echo "<no-dev>")
        echo "  $(basename $p) name=$n dev=$d" >&2
    done
fi
# Export the env that bootstrap, agentd, and the LAZY-SPAWNED ttyd
# inherit through `exec`. PID 1's kernel env is bare (no HOME / USER
# / PATH / SHELL), and bash without HOME resolves `~/.bashrc` to
# `/.bashrc` (does not exist) and silently skips it — losing the
# prompt + ls-color aliases we baked into /root/.bashrc. Set them
# unconditionally so `agentd::shell::start_shell` (which forks ttyd
# on the first SHELL-tab WS connect) inherits them via its parent.
# Absolute paths because PID 1's env doesn't carry PATH unless we
# put it there ourselves.
export HOME=/root
export USER=root
export PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
if [ -x /usr/bin/bash ]; then
    export SHELL=/usr/bin/bash
else
    export SHELL=/bin/sh
fi

# Pre-M1.x revision started ttyd at boot here as a daemon, which
# cost ~1s of cold-path agent_handshake (fork+exec+page-in of ttyd
# binary + bash startup + libwebsockets init) for every session
# whether the user clicked SHELL or not. We deleted that. Now the
# first SHELL request triggers a lazy spawn via agentd's
# `WireRequest::StartShell` handler — see
# `engram-agentd/src/shell.rs::start_shell`. That handler probes
# 127.0.0.1:7681, returns spawned=false if something's already
# bound (legacy / re-entry path), else `Command::new(/usr/local/
# bin/ttyd)` and waits up to READY_DEADLINE for the port to accept.
# Trade: SHELL-tab open latency goes from ~0ms (already-running)
# to ~150ms (fresh spawn). Worth it because the boot cost was paid
# on every session create, and only a tiny fraction of sessions
# actually use the SHELL tab.
mark exec_agentd
exec /sbin/engram-agentd --port __VSOCK_PORT__
"#;

#[derive(Clone, Debug)]
pub struct BuildOutcome {
    pub image_dir: PathBuf,
    pub manifest_path: PathBuf,
    /// Path on disk where the rootfs lives (directory for
    /// `Format::Directory`; `.ext4` file for `Format::Ext4`).
    /// Kept for ProcessBackend dev path; production consumers
    /// resolve content via `disk_manifest` instead.
    pub rootfs_path: PathBuf,
    /// ADR 0007: content-addressed chunked manifest pointing at
    /// the disk's bytes in `BlobStorage`. `None` for
    /// `Format::Directory` bakes (ProcessBackend reads the
    /// rootfs as files); `Some` for `Format::Ext4` bakes. The
    /// host-agent's image cache adopts this ref to materialize
    /// per-sandbox disks via NBD (Linux+FC) or materialize-to-
    /// file (macOS+VZ).
    pub disk_manifest: Option<engram_chunk_store::ManifestRef>,
    /// Size of `rootfs_path` on disk.
    pub size_bytes: u64,
    /// ADR 0008 Phase 3: bootstrap JSON for the disk side of a
    /// Nydus-shaped artifact. Sits next to `rootfs.ext4` in
    /// `image_dir` (e.g. `image_dir/bootstrap.disk.json`). `None`
    /// for non-Ext4 bakes (Directory) or when chunked-OCI emission
    /// was skipped. `push_to_registry` reads both this and
    /// `disk_chunks_blob_path` to construct a chunked OCI push.
    pub disk_bootstrap_path: Option<PathBuf>,
    /// ADR 0008 Phase 3: path to the concatenated disk chunk blob.
    /// Paired with `disk_bootstrap_path`.
    pub disk_chunks_blob_path: Option<PathBuf>,
}

#[derive(Debug)]
pub enum BuildError {
    Config(String),
    Io(std::io::Error),
    Docker(String),
    Ext4(Ext4Error),
    InvalidPath(String),
}

impl std::fmt::Display for BuildError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Config(m) => write!(f, "engram.toml: {m}"),
            Self::Io(e) => write!(f, "io: {e}"),
            Self::Docker(m) => write!(f, "docker: {m}"),
            Self::Ext4(e) => write!(f, "ext4: {e}"),
            Self::InvalidPath(p) => write!(f, "invalid path: {p}"),
        }
    }
}

impl std::error::Error for BuildError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            Self::Ext4(e) => Some(e),
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

impl From<engram_chunk_store::ChunkStoreError> for BuildError {
    fn from(e: engram_chunk_store::ChunkStoreError) -> Self {
        // Chunk-store errors surface as generic Config since
        // they're a fail-the-bake condition not categorically
        // different from any other I/O.
        Self::Config(format!("chunk store: {e}"))
    }
}

/// The baker. Composed of:
///
/// - A [`DockerRunner`] (mockable) for `docker build/create/export`.
/// - An [`Ext4Packer`] (mockable) for the `Format::Ext4` step.
/// - A [`engram_chunk_store::ChunkStore`] that the ext4 path
///   chunks the produced rootfs into (ADR 0007). The store can
///   be backed by `LocalBlobStorage` in dev or
///   `GcsBlobStorage` / `S3BlobStorage` in production CI.
///
/// Stage A decoupled bake from Postgres; baked artifacts are
/// pushed to the registry via [`Self::push_image`] and the
/// coordinator's `/api/enabled-images` POST handler enables them
/// — the builder never touches a `MetadataStore`.
pub struct Builder<D: DockerRunner, P: Ext4Packer = Mke2fsPacker> {
    docker: D,
    packer: P,
    chunk_store: engram_chunk_store::ChunkStore,
    /// OCI client for built-in harness pulls (ADR 0021) and registry
    /// pushes. Optional so tests that exercise only the local bake
    /// pipeline don't have to construct one; absence is surfaced as a
    /// clear error when an operation actually needs it.
    oci: Option<engram_oci::OciClient>,
    /// Built-in harness catalog used when `engram.toml` carries
    /// `[harness] builtin = "..."`. Defaulted to
    /// [`BuiltinCatalog::default_catalog`]; tests can override.
    catalog: BuiltinCatalog,
    /// Guest platform a built-in harness artifact is resolved for
    /// (`harness-claude:<ver>-<platform>`). The harness runs inside the
    /// guest, so this is the rootfs's arch, not the host's. Defaults to
    /// [`Platform::host`] — correct for the dev bake recipes, which
    /// cross-compile the rootfs for the host's own arch. Override with
    /// [`Self::with_harness_platform`] for cross-arch bakes.
    harness_platform: Platform,
}

impl<D: DockerRunner> Builder<D, Mke2fsPacker> {
    /// Default constructor: real `mke2fs` packer for `Format::Ext4`,
    /// caller-supplied chunk store. Call [`Self::with_oci`] afterwards
    /// to attach the OCI client needed for built-in harness pulls and
    /// registry pushes.
    pub fn new(docker: D, chunk_store: engram_chunk_store::ChunkStore) -> Self {
        Self {
            docker,
            packer: Mke2fsPacker::default(),
            chunk_store,
            oci: None,
            // Default catalog + env-driven overrides. CI lanes that
            // publish a just-built harness artifact to a local
            // registry export `ENGRAM_BUILTIN_HARNESS_CLAUDE_REPO=…`
            // before invoking the baker; production leaves the env
            // unset and falls through to the GHCR repos hardcoded in
            // `default_catalog`. See `with_overrides_from_env`.
            catalog: BuiltinCatalog::default_catalog().with_overrides_from_env(),
            harness_platform: Platform::host(),
        }
    }
}

impl<D: DockerRunner, P: Ext4Packer> Builder<D, P> {
    /// Construct with a custom packer — used by tests to record /
    /// inject errors instead of running real mke2fs.
    pub fn with_packer(docker: D, packer: P, chunk_store: engram_chunk_store::ChunkStore) -> Self {
        Self {
            docker,
            packer,
            chunk_store,
            oci: None,
            catalog: BuiltinCatalog::default_catalog(),
            harness_platform: Platform::host(),
        }
    }

    /// Attach the OCI client. Required for [`Self::push_to_registry`]
    /// (image push) and for any bake whose `engram.toml` declares a
    /// built-in harness (the baker pulls + extracts the published
    /// artifact). Returns `self` so it composes with
    /// `Builder::new(...).with_oci(...)`.
    pub fn with_oci(mut self, oci: engram_oci::OciClient) -> Self {
        self.oci = Some(oci);
        self
    }

    /// Override the built-in harness catalog. Test-facing — production
    /// uses [`BuiltinCatalog::default_catalog`] (wired by `Builder::new`).
    pub fn with_catalog(mut self, catalog: BuiltinCatalog) -> Self {
        self.catalog = catalog;
        self
    }

    /// Override the guest platform built-in harness artifacts are
    /// resolved for. Defaults to [`Platform::host`]; the bake recipes
    /// set this from the detected backend's guest arch (e.g. `--harness-
    /// platform linux-arm64` for a VZ image). Returns `self` so it
    /// composes with the other `with_*` builders.
    pub fn with_harness_platform(mut self, platform: Platform) -> Self {
        self.harness_platform = platform;
        self
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
        // we just overlay our agent / bootstrap on top.
        //
        // Harness (ADR 0021): exactly one harness baked per image, or
        // none. Built-ins come from the OCI catalog and are injected
        // here; custom harnesses ride in via the author's Dockerfile
        // and we only validate that `exec` is actually on disk.
        let mut effective_manifest = cfg.to_manifest();
        if let Some(injection) = &req.agent_injection {
            inject_agent(&rootfs_dir, injection).await?;
        }
        if let Some(source_harness) = effective_manifest.harness.clone() {
            if source_harness.builtin.is_some() {
                let oci = self.oci.as_ref().ok_or_else(|| {
                    BuildError::Config(
                        "[harness] builtin = \"...\" requires an OCI client — call Builder::with_oci(...)"
                            .into(),
                    )
                })?;
                let resolved = harness::inject_builtin_harness(
                    &rootfs_dir,
                    oci,
                    &self.catalog,
                    &source_harness,
                    self.harness_platform,
                )
                .await?;
                effective_manifest.harness = Some(resolved);
            } else {
                harness::validate_custom_harness(&rootfs_dir, &effective_manifest).await?;
            }
        }

        // ADR 0027: the share-file / create-pull-request skill glue is no
        // longer baked into the rootfs. It now lives in the fleet-wide
        // `skills` RO bundle the FC host mounts, and agentd activates the
        // right subset per session at SpawnHarness (gated on the forge
        // token) — see `engram-session-bundles`. This retires the old
        // `inject_share_helpers` / `inject_forge_helpers` bake step so a
        // skill edit ships fleet-wide by rolling the bundle, no re-bake.
        // The wrapper scripts + SKILL.md now live in `deploy/bundles/skills/`.

        // Fold the built image's Docker config (its `ENV` + `WORKDIR`)
        // into the manifest as defaults — the author's `engram.toml`
        // [env]/workdir wins. The platform doesn't otherwise read the
        // OCI image config, so this is what lets a Dockerfile's ENV /
        // WORKDIR reach the guest (the agent at start_agent + `engram
        // exec`). Inspect the created-but-unstarted container, whose
        // `Config` mirrors the image's Env/WorkingDir.
        match self.docker.inspect_config(container_id).await {
            Ok(cfg) => {
                effective_manifest.apply_image_config_defaults(&cfg.env, cfg.working_dir.as_deref())
            }
            // Non-fatal: an inspect hiccup shouldn't fail an otherwise
            // good bake. The image still works; it just doesn't inherit
            // the Dockerfile env/workdir (same as pre-this-feature), and
            // the author can always set them in engram.toml.
            Err(e) => tracing::warn!(
                error = %e,
                "docker inspect for image config failed; \
                 manifest will not inherit the Dockerfile ENV/WORKDIR",
            ),
        }

        let manifest_path = image_dir.join("manifest.toml");
        let manifest_str = render_manifest_value(&effective_manifest)?;
        tokio::fs::write(&manifest_path, manifest_str).await?;

        let dir_size = recursive_size(&rootfs_dir).await?;

        let (rootfs_path, total_size, disk_manifest) = match req.format {
            Format::Directory => (rootfs_dir, dir_size, None),
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

                // ADR 0007: chunk the ext4 into the content-addressed
                // store + commit a manifest. Production consumers
                // (FC's NBD daemon, VZ's materialize-to-file) read
                // from the manifest, not the ext4 file directly.
                // We keep the .ext4 around for now so dev workflows
                // and the OCI push path (which still uploads the
                // ext4 as a layer) don't break; Phase 6 retires the
                // file when SandboxBackend takes ManifestRef
                // natively.
                let m = self
                    .chunk_store
                    .chunk_file(
                        &ext4_path,
                        engram_chunk_store::ManifestKind::Disk,
                        None, // default 16 MiB
                    )
                    .await?;
                let manifest_ref = engram_chunk_store::ManifestRef::new();
                self.chunk_store.put_manifest(manifest_ref, &m).await?;

                // Sidecar bundle.json so image_cache + tooling can
                // find the manifest ref without hitting Postgres.
                // Format is intentionally tiny so a future "list
                // images" CLI can fetch it cheaply.
                //
                let bundle = serde_json::json!({
                    "schema_version": 1,
                    "disk_manifest": manifest_ref,
                });
                tokio::fs::write(
                    image_dir.join("bundle.json"),
                    serde_json::to_vec_pretty(&bundle)
                        .map_err(|e| BuildError::Config(format!("bundle.json: {e}")))?,
                )
                .await?;

                tracing::info!(
                    repo = %req.repo,
                    tag = %req.tag,
                    manifest = %manifest_ref,
                    chunks = m.chunks.len(),
                    total_bytes = m.total_bytes,
                    "chunked ext4 into manifest"
                );

                (ext4_path, on_disk, Some(manifest_ref))
            }
        };

        // ADR 0008 Phase 3: produce Nydus-shaped artifacts
        // alongside the existing bundle.json. For each kind that
        // has a chunked manifest, build (bootstrap, chunk_blob)
        // and write them to image_dir. push_to_registry uploads
        // both as new OCI layers; the runtime resolver indexes
        // them at pull time.
        //
        // The disk_manifest in BlobStorage stays — image_cache's
        // legacy path still resolves through it for v1 artifacts
        // and dev workflows. v2 consumers prefer the bootstrap
        // layers because they enable Range-GET-on-fault.
        let mut disk_bootstrap_path = None;
        let mut disk_chunks_blob_path = None;
        if let Some(disk_ref) = disk_manifest {
            let manifest = self
                .chunk_store
                .get_manifest(disk_ref)
                .await
                .map_err(|e| BuildError::Config(format!("re-read disk manifest: {e}")))?;

            // ADR 0008 Phase 4: if a parent bootstrap is supplied,
            // dedup against it. Shared chunks travel by reference;
            // the child's diff blob only contains chunks unique to
            // this image.
            let parent_bs_owned: Option<engram_chunk_store::Bootstrap> =
                if let Some(p) = &req.parent_disk_bootstrap_path {
                    let bytes = tokio::fs::read(p).await.map_err(BuildError::Io)?;
                    let bs: engram_chunk_store::Bootstrap = serde_json::from_slice(&bytes)
                        .map_err(|e| BuildError::Config(format!("parent bootstrap parse: {e}")))?;
                    Some(bs)
                } else {
                    None
                };
            let parent_ref = match (
                parent_bs_owned.as_ref(),
                req.parent_disk_chunks_blob_digest.as_deref(),
            ) {
                (Some(bs), Some(digest)) => Some(engram_chunk_store::ParentBootstrap {
                    bootstrap: bs,
                    primary_blob_digest: digest,
                }),
                (Some(_), None) => {
                    return Err(BuildError::Config(
                        "parent_disk_bootstrap_path requires parent_disk_chunks_blob_digest".into(),
                    ));
                }
                (None, Some(_)) => {
                    return Err(BuildError::Config(
                        "parent_disk_chunks_blob_digest requires parent_disk_bootstrap_path".into(),
                    ));
                }
                (None, None) => None,
            };

            let (bootstrap, blob) = engram_chunk_store::Bootstrap::build_from_manifest_with_parent(
                &self.chunk_store,
                &manifest,
                parent_ref.as_ref(),
            )
            .await
            .map_err(|e| BuildError::Config(format!("disk bootstrap build: {e}")))?;
            let bs_path = image_dir.join("bootstrap.disk.json");
            let blob_path = image_dir.join("chunks.disk.blob");
            tokio::fs::write(
                &bs_path,
                serde_json::to_vec(&bootstrap)
                    .map_err(|e| BuildError::Config(format!("disk bootstrap json: {e}")))?,
            )
            .await?;
            tokio::fs::write(&blob_path, &blob).await?;
            disk_bootstrap_path = Some(bs_path);
            disk_chunks_blob_path = Some(blob_path);
        }

        // Re-write bundle.json with the disk_manifest now that the
        // ext4 branch (above) wrote a baseline. Schema bumps to v2
        // when chunked-OCI artifacts were produced — v1 readers
        // still see `disk_manifest` and work; v2 readers also
        // consult `bootstrap_disk_available` for Range-GET-on-fault.
        if let Some(disk_ref) = disk_manifest {
            let schema_version = if disk_bootstrap_path.is_some() { 2 } else { 1 };
            let mut bundle = serde_json::json!({
                "schema_version": schema_version,
                "disk_manifest": disk_ref,
            });
            if disk_bootstrap_path.is_some() {
                // OCI layer digests are computed at push time; we
                // just flag that the chunked-OCI shape is available.
                bundle["bootstrap_disk_available"] = serde_json::Value::Bool(true);
            }
            tokio::fs::write(
                image_dir.join("bundle.json"),
                serde_json::to_vec_pretty(&bundle)
                    .map_err(|e| BuildError::Config(format!("bundle.json: {e}")))?,
            )
            .await?;
        }

        Ok(BuildOutcome {
            image_dir: image_dir.to_path_buf(),
            manifest_path,
            rootfs_path,
            disk_manifest,
            size_bytes: total_size,
            disk_bootstrap_path,
            disk_chunks_blob_path,
        })
    }

    /// Push a freshly-baked image (output of [`Self::build`]) to a
    /// Docker registry as an Engram OCI artifact. Two layers:
    /// `manifest.toml` and `rootfs.ext4`. Returns the registry URI
    /// (suitable for storing in `image_versions.blob_url`) and the
    /// manifest's content digest.
    ///
    /// `registry_uri` may be either `host/repo:tag` (use this exact
    /// reference) or `host/repo` (auto-append `:<req.tag>`). The
    /// shorter form keeps the `engram image build --push <host/repo>`
    /// flow tidy.
    pub async fn push_to_registry(
        &self,
        req: &BuildRequest,
        outcome: &BuildOutcome,
        registry_uri: &str,
    ) -> Result<RegistryPush, BuildError> {
        let oci = self.oci.as_ref().ok_or_else(|| {
            BuildError::Config(
                "push_to_registry: no OCI client attached — call Builder::with_oci(...)".into(),
            )
        })?;
        // Only Ext4 outputs are pushable today — the Directory format
        // is fundamentally a per-host materialization (file ownership,
        // overlayfs whiteouts) that doesn't survive a tar+pull. The
        // rootfs.ext4 file *is* the artifact bytes.
        if !matches!(req.format, Format::Ext4) {
            return Err(BuildError::Config(
                "registry push currently requires --format ext4 (Directory bakes are local-only)"
                    .into(),
            ));
        }

        let full_uri = if registry_uri.contains(':') && !registry_uri.ends_with('/') {
            // Already has a tag (covers `host:port/repo:tag` and `host/repo:tag`).
            // Heuristic: a `:` after the last `/` is a tag separator. Otherwise
            // it's just a port on the host portion.
            let last_slash = registry_uri.rfind('/').unwrap_or(0);
            if registry_uri[last_slash..].contains(':') {
                registry_uri.to_string()
            } else {
                format!("{registry_uri}:{}", req.tag)
            }
        } else {
            format!("{registry_uri}:{}", req.tag)
        };

        let manifest_bytes = tokio::fs::read(&outcome.manifest_path)
            .await
            .map_err(BuildError::Io)?;
        // Tiny config blob for introspection — registries display this
        // and it makes Engram artifacts easy to recognize in a UI.
        let config = serde_json::json!({
            "kind": "engram-image-v1",
            "format": match req.format { Format::Ext4 => "ext4", Format::Directory => "directory" },
            "repo":   req.repo,
            "tag":    req.tag,
        });
        let config_bytes = serde_json::to_vec(&config)
            .map_err(|e| BuildError::Config(format!("config json: {e}")))?;

        // ADR 0007 bundle layer — present iff the bake produced a
        // chunked disk manifest (Ext4 outputs do; Directory bakes
        // don't and can't be pushed anyway). Pullers learn the
        // chunk-manifest ref from this sidecar.
        let bundle_path = outcome.image_dir.join("bundle.json");
        let bundle_bytes = if outcome.disk_manifest.is_some() && bundle_path.exists() {
            Some(
                tokio::fs::read(&bundle_path)
                    .await
                    .map_err(BuildError::Io)?,
            )
        } else {
            None
        };

        // ADR 0008 Phase 3: when the bake produced Nydus-shaped
        // outputs (bootstrap + chunk_blob), push them as a
        // chunked OCI artifact. Consumers with chunked-OCI
        // support Range-GET individual chunks on fault; legacy
        // consumers fall back to BlobStorage via bundle.json's
        // disk_manifest. Both code paths see the same manifest
        // toml + bundle.json.
        if let (Some(bs_path), Some(blob_path), Some(bundle)) = (
            outcome.disk_bootstrap_path.as_ref(),
            outcome.disk_chunks_blob_path.as_ref(),
            bundle_bytes.as_ref(),
        ) {
            let disk_bootstrap_json = tokio::fs::read(bs_path).await.map_err(BuildError::Io)?;
            let disk_chunks_blob = tokio::fs::read(blob_path).await.map_err(BuildError::Io)?;

            let payload = engram_oci::ChunkedPushPayload {
                manifest_toml: manifest_bytes,
                config_json: config_bytes,
                bundle_json: bundle.clone(),
                disk_bootstrap_json,
                disk_chunks_blob,
            };

            let digest = oci
                .push_chunked_image(&full_uri, payload)
                .await
                .map_err(|e| BuildError::Docker(format!("oci chunked push: {e}")))?;

            return Ok(RegistryPush {
                uri: full_uri,
                manifest_digest: digest,
            });
        }

        // ADR 0007 Phase 6: skip the rootfs.ext4 layer when the
        // bundle is present. Disk bytes already live in the chunk
        // store (`Builder::build` chunked them at bake time); the
        // OCI layer is pure duplication. For a 4 GiB rootfs that's
        // 4 GiB of wasted registry bandwidth + storage per push.
        // No-bundle bakes (legacy, dev-only) still ship the full
        // ext4 layer as a fallback.
        let rootfs_arg: Option<&Path> = if bundle_bytes.is_some() {
            None
        } else {
            Some(outcome.rootfs_path.as_path())
        };

        let digest = oci
            .push_image(
                &full_uri,
                &manifest_bytes,
                rootfs_arg,
                &config_bytes,
                bundle_bytes.as_deref(),
            )
            .await
            .map_err(|e| BuildError::Docker(format!("oci push: {e}")))?;

        Ok(RegistryPush {
            uri: full_uri,
            manifest_digest: digest,
        })
    }
}

/// Result of a successful push to an OCI registry.
#[derive(Clone, Debug)]
pub struct RegistryPush {
    pub uri: String,
    pub manifest_digest: engram_oci::Digest256,
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
