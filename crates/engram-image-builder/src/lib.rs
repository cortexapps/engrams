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

use std::path::{Path, PathBuf};

use engram_core::types::ImageManifest;

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
    /// ADR 0007 Phase 5: pre-computed canonical-base memory
    /// manifest. Two ways callers populate this:
    ///
    ///  1. Pre-computed by an external pipeline (sidecar Linux+KVM
    ///     job) and passed in directly.
    ///  2. Auto-captured at bake time by setting
    ///     [`capture_canonical_memory`] — the baker boots the
    ///     just-built rootfs via FC, pauses after `boot_wait`,
    ///     snapshots `memory.bin`, chunks it into the store, and
    ///     fills this field with the resulting [`ManifestRef`].
    ///
    /// `None` skips the canonical write to `bundle.json`, so
    /// sessions of this image pay session-private memory cost at
    /// restore time (functionally correct, no cross-VM dedup).
    pub canonical_memory_manifest: Option<engram_chunk_store::ManifestRef>,
    /// ADR 0007 Phase 5: opt-in bake-time canonical memory
    /// capture. When `Some`, the baker reuses
    /// `FirecrackerBackend::create` + `snapshot` to boot the
    /// just-built rootfs, wait for it to settle, and capture
    /// `memory.bin`. The bytes get chunked into the chunk store
    /// and the resulting [`ManifestRef`] populates
    /// `bundle.json::canonical_memory_manifest`. Requires FC +
    /// `/dev/kvm` on the runner; non-KVM CI lanes set this
    /// `None` and rely on the pre-computed path above.
    pub capture_canonical_memory: Option<CanonicalCaptureConfig>,
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

/// Bake-time canonical memory capture parameters. ADR 0007 Phase 5.
#[derive(Clone, Debug)]
pub struct CanonicalCaptureConfig {
    /// Path to the FC kernel image (vmlinux). The same artifact
    /// the production FC host uses; image-builder boots a transient
    /// VM with it.
    pub kernel_image_path: PathBuf,
    /// `firecracker` binary. Defaults to PATH lookup in
    /// `FirecrackerConfig::with_kernel`; override here when CI
    /// pins a specific build.
    pub firecracker_bin: Option<PathBuf>,
    /// How long to wait after `InstanceStart` before snapshotting.
    /// Production bakes pair this with a sentinel file the in-VM
    /// init script writes; for v1 we use a fixed wait that's
    /// generous enough for typical Python/Node images to reach
    /// steady state (5–10 s post-boot).
    pub boot_wait: std::time::Duration,
    /// Guest RAM. The canonical snapshot's `memory.bin` size is
    /// dominated by this — the chunked-storage compression ratio
    /// means it's fine to grant generously. Defaults to 512 MiB
    /// when caller passes `None`.
    pub memory_mib: Option<u32>,
    /// ADR 0014 M1.14: path to `engram-uffd-handler` for the
    /// synthetic profile pass. When `None`, falls back to PATH
    /// lookup ("engram-uffd-handler"). CI bakes that don't ship
    /// the UFFD handler in PATH should pin this explicitly; we
    /// skip the profile pass cleanly when the binary is missing.
    pub uffd_handler_bin: Option<PathBuf>,
    /// ADR 0014 M1.14: chunk-store root the bake's chunks land in
    /// (the `--blob-root` the UFFD handler should read from on the
    /// profile-pass). `None` skips the profile pass: the bake
    /// proceeds without a working-set trace and refill falls back
    /// to full-manifest prefetch.
    pub blob_root: Option<PathBuf>,
    /// Bypass the engram-init + stub-harness scaffolding when
    /// `true`. Default `false` (production bakes capture an
    /// agentd-on-accept snapshot). Set to `true` only by the
    /// canonical-capture integration test fixture, which boots a
    /// stock Ubuntu rootfs that has no engram-init baked in —
    /// without this opt-out the kernel panics with `init=/sbin/
    /// engram-init` missing.
    pub skip_warm_pool_prep: bool,
    /// ADR 0014 M1.16: bake-time network pool. `Some(addr)`
    /// allocates a /30 + creates a host TAP + bakes
    /// `ip=…:eth0:off` into the kernel cmdline, so the snapshot
    /// captures a virtio-net device + an up eth0 inside the VM.
    /// Required for the dashboard SHELL tab + egress to work
    /// post-warm-restore: FC can't hot-add virtio-net after
    /// `load_snapshot`, and the prod host's per-VM netns
    /// re-creates the TAP inside the netns + SNATs the bake's
    /// `10.200.0.2` to a unique-per-VM host slot.
    ///
    /// Production bakes pass `Some("10.200.0.0".parse().unwrap())`
    /// (matching `FirecrackerConfig::default`); bake host needs
    /// `CAP_NET_ADMIN` for `ip tuntap add`. `None` keeps the bake
    /// netless — only useful for test fixtures without
    /// `CAP_NET_ADMIN` that don't need post-restore networking.
    pub net_pool: Option<std::net::Ipv4Addr>,
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
    /// `/dev`, and `exec`s `engram-agentd --port <port>`. (Pre-ADR-
    /// 0015 the shim also forked a separate `engram-bootstrap`
    /// supervisor; that process was folded into agentd and the
    /// shim is one-line shorter as a result.)
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
#
# ADR 0015 M1: bootstrap is no longer a separate process. Its
# harness-supervisor responsibility moved into agentd, which the
# host dials with `WireRequest::SpawnHarness` after the FC instance
# is up. The legacy `/sbin/engram-bootstrap` is a no-op stub for
# bakes that still ship it; this init shim no longer forks it.
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
    /// ADR 0007 Phase 5: bake-time canonical-base memory
    /// manifest. `Some` when the baker captured a post-init
    /// `memory.bin` via FC + chunked it; `None` when capture
    /// was skipped. Mirrors `bundle.json::canonical_memory_manifest`.
    pub canonical_memory_manifest: Option<engram_chunk_store::ManifestRef>,
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
    /// ADR 0008 Phase 3: memory-side counterpart of
    /// `disk_bootstrap_path`. `Some` iff canonical memory was
    /// captured at bake time.
    pub memory_bootstrap_path: Option<PathBuf>,
    pub memory_chunks_blob_path: Option<PathBuf>,
    /// ADR 0014: bake-time captured FC `state.bin`. `Some` iff
    /// `capture_canonical_memory` ran. `push_to_registry` reads
    /// this and ships it as an OCI layer; coord's `enable_image`
    /// pulls the layer and writes the bytes to its BlobStorage
    /// at `state_blob_key(snapshot_id)`.
    pub snapshot_state_path: Option<PathBuf>,
    /// ADR 0014: bake-time captured FC sidecar `manifest.json`.
    /// Paired with `snapshot_state_path`.
    pub snapshot_sidecar_path: Option<PathBuf>,
    /// ADR 0014 M1.14: working-set trace JSON produced by the
    /// synthetic profile pass. `Some` iff the bake ran the profile
    /// successfully. Coord materializes to BlobStorage at
    /// `working_set_blob_key(snapshot_id)` on enable-image.
    pub snapshot_working_set_path: Option<PathBuf>,
    /// ADR 0014: full portable template snapshot. `Some` iff the
    /// bake captured canonical memory. `state_blob_key` and
    /// `sidecar_blob_key` are `None` here — coord assigns them on
    /// enable-image after writing the OCI-shipped bytes to its
    /// own BlobStorage. bundle.json's `canonical_snapshot` block
    /// carries the same shape for downstream consumers reading
    /// the artifact directly.
    pub canonical_snapshot: Option<engram_core::types::snapshot::SnapshotMetadata>,
}

#[derive(Debug)]
pub enum BuildError {
    Config(String),
    Io(std::io::Error),
    Docker(String),
    Ext4(Ext4Error),
    InvalidPath(String),
    /// ADR 0014: caller requested `--capture-canonical-memory` but
    /// the bake-time FC capture step failed (TAP provisioning,
    /// /dev/kvm permissions, snapshot/restore plumbing, etc.).
    /// Returned instead of silently producing an artifact whose
    /// bundle.json lacks the canonical_snapshot block — that's the
    /// failure mode that produced `demo:warm-75babf7` (TAP EPERM
    /// in CI) and caused warm pool to silently never fire for
    /// the image in prod.
    CanonicalCapture(String),
}

impl std::fmt::Display for BuildError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Config(m) => write!(f, "engram.toml: {m}"),
            Self::Io(e) => write!(f, "io: {e}"),
            Self::Docker(m) => write!(f, "docker: {m}"),
            Self::Ext4(e) => write!(f, "ext4: {e}"),
            Self::InvalidPath(p) => write!(f, "invalid path: {p}"),
            Self::CanonicalCapture(m) => write!(f, "canonical memory capture: {m}"),
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
}

impl<D: DockerRunner> Builder<D, Mke2fsPacker> {
    /// Default constructor: real `mke2fs` packer for `Format::Ext4`
    /// + caller-supplied chunk store.
    pub fn new(docker: D, chunk_store: engram_chunk_store::ChunkStore) -> Self {
        Self {
            docker,
            packer: Mke2fsPacker::default(),
            chunk_store,
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
        }
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
                // ADR 0007 Phase 5: `canonical_memory_manifest`
                // ships as `null` until the bake-time canonical
                // capture step runs (`req.capture_canonical_memory`).
                // The field is optional both on the wire and in
                // `ImageBundle`'s serde shape, so older bakes that
                // never set it deserialise cleanly.
                let bundle = serde_json::json!({
                    "schema_version": 1,
                    "disk_manifest": manifest_ref,
                    "canonical_memory_manifest": req.canonical_memory_manifest,
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

        // ADR 0007 Phase 5 + ADR 0014: bake-time canonical memory
        // capture. Three branches:
        //   1. Caller supplied a pre-computed ref → use it as is
        //      (no full snapshot, just the memory manifest).
        //   2. Caller enabled auto-capture AND we have an ext4
        //      rootfs (canonical only makes sense on FC) → boot
        //      the rootfs once, snapshot, chunk memory.bin,
        //      upload state.bin + sidecar to BlobStorage. Returns
        //      a full portable SnapshotMetadata.
        //   3. Otherwise → None (sessions pay session-private
        //      memory cost at restore, functionally correct).
        let (canonical_memory_manifest, canonical_snapshot) =
            if let Some(mref) = req.canonical_memory_manifest {
                (Some(mref), None)
            } else if let (Some(capture_cfg), Some(disk_ref)) =
                (req.capture_canonical_memory.as_ref(), disk_manifest)
            {
                match self
                    .capture_canonical_memory(&rootfs_path, image_dir, capture_cfg)
                    .await
                {
                    Ok(mut metadata) => {
                        // ADR 0014 M1.11: the bundle's `canonical_snapshot`
                        // block needs `disk_manifest` so the coord-side
                        // materializer + heartbeat-ack carry it through to
                        // hosts. Without this, warm-pool refill on a fresh
                        // host has the memory side reachable in BlobStorage
                        // but no way to materialize the rootfs file FC
                        // needs at `load_snapshot` time → "Block: Virtio
                        // backend error" on every refill until a cold
                        // session create primes image_cache.
                        metadata.disk_manifest = Some(disk_ref);
                        tracing::info!(
                            repo = %req.repo,
                            tag = %req.tag,
                            snapshot_id = %metadata.id,
                            memory_manifest = ?metadata.memory_manifest,
                            disk_manifest = ?metadata.disk_manifest,
                            "captured canonical template snapshot at bake time"
                        );
                        (metadata.memory_manifest, Some(metadata))
                    }
                    Err(e) => {
                        // Fail loud. Previously this was best-effort
                        // with a WARN log, but a "successful" bake that
                        // shipped without canonical_snapshot was the
                        // exact mechanism behind `demo:warm-75babf7`
                        // (TAP ioctl EPERM in CI; warn logged but exit
                        // code 0; image pushed; coord cascade silently
                        // skipped templates write; warm pool never
                        // fired for the image; every session
                        // cold-created in ~27s in prod).
                        //
                        // The caller explicitly asked for canonical
                        // capture via `--capture-canonical-memory`.
                        // Honor that — if capture can't happen, refuse
                        // to ship a half-broken artifact. CI lanes that
                        // genuinely don't have KVM should simply omit
                        // the flag.
                        tracing::error!(
                            repo = %req.repo,
                            tag = %req.tag,
                            error = %e,
                            "canonical memory capture failed; aborting bake",
                        );
                        return Err(BuildError::CanonicalCapture(format!("{e}")));
                    }
                }
            } else {
                (None, None)
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

        let mut memory_bootstrap_path = None;
        let mut memory_chunks_blob_path = None;
        if let Some(mem_ref) = canonical_memory_manifest {
            let manifest = self
                .chunk_store
                .get_manifest(mem_ref)
                .await
                .map_err(|e| BuildError::Config(format!("re-read memory manifest: {e}")))?;
            let (bootstrap, blob) =
                engram_chunk_store::Bootstrap::build_from_manifest(&self.chunk_store, &manifest)
                    .await
                    .map_err(|e| BuildError::Config(format!("memory bootstrap build: {e}")))?;
            let bs_path = image_dir.join("bootstrap.memory.json");
            let blob_path = image_dir.join("chunks.memory.blob");
            tokio::fs::write(
                &bs_path,
                serde_json::to_vec(&bootstrap)
                    .map_err(|e| BuildError::Config(format!("memory bootstrap json: {e}")))?,
            )
            .await?;
            tokio::fs::write(&blob_path, &blob).await?;
            memory_bootstrap_path = Some(bs_path);
            memory_chunks_blob_path = Some(blob_path);
        }

        // Re-write bundle.json now that we know whether the
        // canonical memory ref is set. The earlier write inside
        // the Ext4 branch already wrote a baseline bundle; this
        // is the canonical-aware overwrite. Schema bumps to v2
        // when chunked-OCI artifacts were produced — v1 readers
        // see `disk_manifest` still and work; v2 readers
        // additionally consult `bootstrap_disk` /
        // `bootstrap_memory` for Range-GET-on-fault. v3 (ADR 0014)
        // adds `canonical_snapshot` for restorable template
        // snapshots (state.bin + sidecar in BlobStorage).
        if let Some(disk_ref) = disk_manifest {
            let schema_version = if canonical_snapshot.is_some() {
                3
            } else if disk_bootstrap_path.is_some() {
                2
            } else {
                1
            };
            let mut bundle = serde_json::json!({
                "schema_version": schema_version,
                "disk_manifest": disk_ref,
                "canonical_memory_manifest": canonical_memory_manifest,
            });
            if let Some(snap) = canonical_snapshot.as_ref() {
                // The full portable snapshot descriptor — coord +
                // host-agent consume this to register a `templates`
                // row and lease a warm slot keyed by the snapshot
                // id, with state.bin + sidecar fetchable from
                // BlobStorage at the keys recorded here.
                //
                // Serialize via `serde_json::to_value(snap)` rather
                // than hand-rolling a JSON object: the cascade in
                // engram-coordinator reads this block with
                // `serde_json::from_value::<SnapshotMetadata>` and
                // any field-name drift between the struct's serde
                // shape and the hand-roll silently returns None →
                // cascade skipped → templates row never written →
                // warm pool never fires for this image. The prior
                // hand-roll used "snapshot_id" but the struct's
                // field is "id", and omitted required fields like
                // `image_version` entirely.
                bundle["canonical_snapshot"] = serde_json::to_value(snap).map_err(|e| {
                    BuildError::Config(format!("serialize canonical_snapshot: {e}"))
                })?;
            }
            if disk_bootstrap_path.is_some() {
                // We don't know the OCI layer digests yet (those
                // are computed at push time when the registry
                // checks the upload), so we just flag that the
                // chunked-OCI shape is *available* — consumers
                // resolve actual digests from the OCI manifest.
                bundle["bootstrap_disk_available"] = serde_json::Value::Bool(true);
            }
            if memory_bootstrap_path.is_some() {
                bundle["bootstrap_memory_available"] = serde_json::Value::Bool(true);
            }
            tokio::fs::write(
                image_dir.join("bundle.json"),
                serde_json::to_vec_pretty(&bundle)
                    .map_err(|e| BuildError::Config(format!("bundle.json: {e}")))?,
            )
            .await?;
        }

        // ADR 0014: snapshot_state + sidecar are staged into
        // image_dir by `capture_canonical_memory`. Surface their
        // paths on BuildOutcome so `push_to_registry` can include
        // them as OCI layers.
        let (snapshot_state_path, snapshot_sidecar_path, snapshot_working_set_path) =
            if canonical_snapshot.is_some() {
                let s = image_dir.join("snapshot.state.bin");
                let c = image_dir.join("snapshot.sidecar.json");
                let w = image_dir.join("snapshot.working_set.json");
                (
                    s.exists().then_some(s),
                    c.exists().then_some(c),
                    w.exists().then_some(w),
                )
            } else {
                (None, None, None)
            };

        Ok(BuildOutcome {
            image_dir: image_dir.to_path_buf(),
            manifest_path,
            rootfs_path,
            disk_manifest,
            canonical_memory_manifest,
            size_bytes: total_size,
            disk_bootstrap_path,
            disk_chunks_blob_path,
            memory_bootstrap_path,
            memory_chunks_blob_path,
            snapshot_state_path,
            snapshot_sidecar_path,
            snapshot_working_set_path,
            canonical_snapshot,
        })
    }

    /// Boot the just-baked rootfs via FC, wait `boot_wait`, then
    /// snapshot `memory.bin` and chunk it into the chunk store.
    /// ADR 0014: also uploads `state.bin` + sidecar JSON to
    /// `BlobStorage` so production hosts can restore this template
    /// snapshot without re-running the bake. Returns a full
    /// [`SnapshotMetadata`] with all portable fields stamped
    /// (memory_manifest, state_blob_key, sidecar_blob_key,
    /// source_sandbox_id) — bundle.json's `canonical_snapshot`
    /// block serializes the same data.
    ///
    /// Requires:
    /// - `/dev/kvm` accessible to the bake user (the dev VM, the
    ///   Blacksmith nested-virt runner, or a bare-metal builder)
    /// - The `firecracker` binary on PATH (or `capture_cfg.
    ///   firecracker_bin` set)
    ///
    /// The bake VM has **no networking** — `net_pool = None` —
    /// and runs `init=/bin/bash` by default. Operators baking
    /// agentd-injected images that need to reach steady-state
    /// before snapshotting should set `boot_wait` proportionally
    /// (5–10 s for typical Python/Node, longer for heavy
    /// services). A sentinel-file detector is the right v2.
    pub async fn capture_canonical_memory(
        &self,
        rootfs_path: &Path,
        image_dir: &Path,
        capture_cfg: &CanonicalCaptureConfig,
    ) -> Result<engram_core::types::snapshot::SnapshotMetadata, BuildError> {
        use engram_chunk_store::ManifestKind;
        use engram_core::types::sandbox::{CpuLimit, DiskLimit, MemoryLimit, SandboxSpec};
        use engram_sandbox_firecracker::{FirecrackerBackend, FirecrackerConfig};

        // FC's create flow installs `rootfs_source` as a symlink target
        // at `<work_dir>/rootfs/<sandbox_id>.dev`, and the kernel
        // resolves relative symlink targets against the symlink's
        // parent directory — not the bake's CWD. CLI invocations
        // typically pass `--images-dir var/engram/images` (relative),
        // so without this canonicalize FC's PUT /drives lands on a
        // "No such file or directory" error.
        let rootfs_path = tokio::fs::canonicalize(rootfs_path).await.map_err(|e| {
            BuildError::Config(format!(
                "canonicalize rootfs path {}: {e}",
                rootfs_path.display()
            ))
        })?;

        let work = tempfile::tempdir()
            .map_err(|e| BuildError::Config(format!("canonical bake tempdir: {e}")))?;

        // ADR 0014 M1.12 (option D) + ADR 0015 M1: produce a 16 MiB
        // empty ext4 the bake attaches as the harness substrate
        // (/dev/vdb). The init shim doesn't mount /dev/vdb; agentd's
        // harness supervisor mounts it *after* receiving SpawnHarness
        // at warm-lease time. The stub being attached at bake time
        // is what lets us snapshot an agentd-on-accept state with
        // the correct device-tree — FC's `swap_harness_drive` later
        // swaps to the session's real harness, but the snapshot
        // needs *something* openable at the embedded path. Skipped
        // in test fixtures that boot a stock rootfs without
        // engram-init (kernel panic otherwise).
        let stub_path = if capture_cfg.skip_warm_pool_prep {
            None
        } else {
            let stub_dir = work.path().join("stub");
            tokio::fs::create_dir_all(&stub_dir).await.map_err(|e| {
                BuildError::Config(format!(
                    "create stub harness staging dir {}: {e}",
                    stub_dir.display()
                ))
            })?;
            let stub_src = stub_dir.join("src");
            tokio::fs::create_dir_all(&stub_src).await.map_err(|e| {
                BuildError::Config(format!(
                    "create stub harness src dir {}: {e}",
                    stub_src.display()
                ))
            })?;
            let p = work.path().join(".stub-harness.ext4");
            // ADR 0014 M1.12 stub size: load-bearing. The stub is
            // mounted as /dev/vdb at warm-pool bake time and
            // captured in the snapshot. On warm lease, the host
            // hot-swaps /dev/vdb's backing file to the session's
            // harness pack ext4 via FC `PATCH /drives`. Firecracker
            // accepts a larger replacement file and emits a
            // virtio-blk "capacity change" notification — but the
            // guest's mounted-FS view of /dev/vdb is fixed by the
            // SUPERBLOCK + bgd that ext4 read at mount time. So
            // any swap-target larger than the stub silently
            // truncates: the guest sees only the first stub-many
            // bytes of the new file, which (since ext4 puts metadata
            // up front) shows up as `lost+found` and nothing else.
            //
            // Observed in prod 2026-05-21 session 0c83f95f: stub
            // was 16 MiB, harness pack was 455 MiB, swap "succeeded"
            // but `/run/engram/harnesses/` showed only lost+found
            // and the harness never exec'd (no claude binary).
            //
            // Sizing rule: the stub must be at least as big as the
            // largest harness pack we'll ever swap in. The Claude
            // pack ships ~455 MiB; pick 1 GiB to leave headroom
            // for future harnesses + Claude growth. The file is
            // sparse (`set_len` doesn't write zeros on Linux ext4),
            // so the snapshot's actual on-disk footprint stays
            // dominated by ext4 metadata + lost+found (tens of
            // MiB), not the 1 GiB nominal size.
            const STUB_HARNESS_SIZE: u64 = 1024 * 1024 * 1024;
            Mke2fsPacker::default()
                .pack(&stub_src, &p, STUB_HARNESS_SIZE)
                .await
                .map_err(|e| BuildError::Config(format!("pack stub harness ext4: {e}")))?;
            Some(p)
        };

        let mut fc_cfg = FirecrackerConfig::with_kernel(&capture_cfg.kernel_image_path);
        if let Some(bin) = &capture_cfg.firecracker_bin {
            fc_cfg.firecracker_bin = bin.clone();
        }
        // Bake-time isolation: egress proxy stays off (the image
        // is supposed to reach steady state on local resources).
        // Networking IS configured when `capture_cfg.net_pool` is
        // `Some` — ADR 0014 M1.16 needs the snapshot to capture a
        // virtio-net device + an `ip=…`-up eth0 because FC can't
        // hot-add network interfaces after `load_snapshot`.
        // Production prod-host's warm-restore path recreates the
        // bake's TAP inside a per-VM netns + SNATs the bake-time
        // source IP to a unique-per-VM host slot.
        fc_cfg.net_pool = capture_cfg.net_pool;
        fc_cfg.egress_proxy_port = None;
        // ADR 0014 follow-up: bake-time + restore-time MUST agree on
        // the CPU template, otherwise the snapshot captures the bake
        // host's CPUID (AMD on Blacksmith runners 2026-05-21) and the
        // guest's glibc ifunc resolver picks code paths the prod CPU
        // (Intel Cascade Lake) can't execute. The host-agent picks up
        // the same env var on startup so prod-side and bake-side stay
        // in lockstep.
        fc_cfg.cpu_template = engram_sandbox_firecracker::cpu_template_from_env();
        // ADR 0015 M1: boot through `engram-init` so the init shim
        // exec's engram-agentd (port 1024 listener) before snapshot.
        // Without this the bake captures a kernel-only state and
        // warm-launch's CONNECT gets RST. Test fixtures that boot
        // a stock rootfs (no engram-init) override to a plain shell
        // so the kernel doesn't panic.
        fc_cfg.default_boot_args = if capture_cfg.skip_warm_pool_prep {
            "console=ttyS0 reboot=k panic=1 pci=off init=/bin/bash".into()
        } else {
            "console=ttyS0 reboot=k panic=1 pci=off init=/sbin/engram-init".into()
        };
        // Hand the FC backend the bake-time stub so its restore
        // path can resolve the harness symlink — only meaningful
        // here when we run a profiling-restore later (M1.14).
        fc_cfg.stub_harness_path = stub_path.clone();

        let backend = FirecrackerBackend::new(work.path(), fc_cfg);

        let spec = SandboxSpec {
            image: format!("canonical-bake:{}", uuid::Uuid::new_v4().simple()),
            rootfs_source: Some(rootfs_path.to_path_buf()),
            image_uri: None,
            harness_pack_uri: None,
            cpu: CpuLimit { vcpus: 1 },
            memory: MemoryLimit {
                max_mib: capture_cfg.memory_mib.unwrap_or(512),
            },
            disk: DiskLimit { max_gib: 4 },
            ttl: None,
            env: Default::default(),
            workdir: None,
            harness_substrate: stub_path.clone(),
            network: Default::default(),
            canonical_memory_manifest: None,
        };

        let sandbox_id = engram_core::traits::SandboxBackend::create(&backend, spec)
            .await
            .map_err(|e| BuildError::Config(format!("canonical bake create: {e}")))?;

        // Wait for steady state. Production v2 will replace this
        // with a sentinel file the init script writes (the agentd
        // injection shim writes /run/engram/agentd-ready); v1's
        // fixed wait works for any rootfs.
        tokio::time::sleep(capture_cfg.boot_wait).await;

        // ADR 0007 Phase 6: backend owns the staging dir; we look it
        // up via snapshot_path_for after the snapshot completes so we
        // can chunk the memory.bin it wrote.
        let mut metadata = engram_core::traits::SandboxBackend::snapshot(&backend, sandbox_id)
            .await
            .map_err(|e| BuildError::Config(format!("canonical bake snapshot: {e}")))?;
        let snap_dir =
            engram_core::traits::SandboxBackend::snapshot_path_for(&backend, metadata.id);

        // Snapshot wrote memory.bin into snap_dir. Chunk it into
        // the store.
        let memory_bin = snap_dir.join("memory.bin");
        let memory_manifest = self
            .chunk_store
            .chunk_file(&memory_bin, ManifestKind::Memory, None)
            .await
            .map_err(|e| BuildError::Config(format!("chunk canonical memory.bin: {e}")))?;
        let manifest_ref = engram_chunk_store::ManifestRef::new();
        self.chunk_store
            .put_manifest(manifest_ref, &memory_manifest)
            .await
            .map_err(|e| BuildError::Config(format!("put canonical manifest: {e}")))?;

        // ADR 0014: copy state.bin + sidecar JSON into `image_dir`
        // so `push_to_registry` can package them as OCI layers.
        // Previously the bake uploaded these directly to its
        // local BlobStorage — that coupled the bake's environment
        // (LocalBlobStorage on a CI runner with no GCS creds) to
        // the production deployment's blob backend, producing
        // OCI artifacts prod hosts couldn't restore from. The
        // current design: bake → OCI artifact → coord's
        // `enable_image` materializes to BlobStorage at canonical
        // keys derived from snapshot_id. blob keys are assigned
        // app-side, not bake-side, so we leave them None here.
        let state_path = snap_dir.join("state.bin");
        let sidecar_path = snap_dir.join("manifest.json");
        let staged_state = image_dir.join("snapshot.state.bin");
        let staged_sidecar = image_dir.join("snapshot.sidecar.json");
        tokio::fs::copy(&state_path, &staged_state)
            .await
            .map_err(|e| {
                BuildError::Config(format!(
                    "stage canonical state.bin into {}: {e}",
                    staged_state.display()
                ))
            })?;

        // ADR 0014: patch `memory_manifest` into the FC sidecar
        // before staging it. The production session-snapshot path
        // does this in `PooledBackend::snapshot` (the host that
        // wrote the snapshot also chunks memory.bin and updates
        // the sidecar in place), but the bake side never did —
        // FC writes the sidecar without that field. On a
        // cross-host warm-pool restore, `materialize_memory_if_missing`
        // reads the sidecar to discover the memory_manifest ref;
        // if the field is absent the function silently no-ops and
        // FC restore later errors with "snapshot memory.bin missing".
        let sidecar_bytes = tokio::fs::read(&sidecar_path).await.map_err(|e| {
            BuildError::Config(format!(
                "read fc manifest.json {}: {e}",
                sidecar_path.display()
            ))
        })?;
        let mut sidecar_value: serde_json::Value =
            serde_json::from_slice(&sidecar_bytes).map_err(|e| {
                BuildError::Config(format!(
                    "parse fc manifest.json {}: {e}",
                    sidecar_path.display()
                ))
            })?;
        sidecar_value
            .as_object_mut()
            .ok_or_else(|| BuildError::Config("fc manifest.json root is not an object".into()))?
            .insert(
                "memory_manifest".into(),
                serde_json::to_value(manifest_ref)
                    .map_err(|e| BuildError::Config(format!("serialize memory_manifest: {e}")))?,
            );
        let patched = serde_json::to_vec_pretty(&sidecar_value)
            .map_err(|e| BuildError::Config(format!("serialize patched sidecar: {e}")))?;
        tokio::fs::write(&staged_sidecar, &patched)
            .await
            .map_err(|e| {
                BuildError::Config(format!(
                    "write patched sidecar to {}: {e}",
                    staged_sidecar.display()
                ))
            })?;

        // Stamp the metadata with everything a sibling host needs
        // to restore. memory_manifest is the chunked manifest of
        // memory.bin; canonical_memory_manifest is the same value
        // here (this IS the canonical for the template).
        // state_blob_key + sidecar_blob_key stay None — coord
        // assigns them on enable-image based on snapshot_id.
        metadata.memory_manifest = Some(manifest_ref);
        metadata.source_sandbox_id = Some(sandbox_id);
        metadata.state_blob_key = None;
        metadata.sidecar_blob_key = None;

        tracing::info!(
            snapshot_id = %metadata.id,
            source_sandbox = %sandbox_id,
            memory_manifest = %manifest_ref,
            "canonical bake snapshot uploaded; portable refs stamped",
        );

        // ADR 0014 M1.14: synthetic working-set profile pass. Restore
        // the just-taken snapshot in UFFD mode, drive bootstrap
        // through a synthetic mount+exec on the stub harness, dump
        // the recorded trace to disk for OCI shipment. Best-effort:
        // any failure here is logged at WARN and we proceed without
        // a trace — refill falls back to full-manifest prefetch.
        // Bake must be done with the primary backend BEFORE the
        // profile pass: same kernel, same chunk store.
        if let Err(e) = engram_core::traits::SandboxBackend::destroy(&backend, sandbox_id).await {
            tracing::warn!(error = %e, "canonical bake VM destroy failed; FC child cleanup is kill-on-drop");
        }
        // Skip the M1.14 profile pass when there's no stub harness
        // — the pass relies on bootstrap mounting /dev/vdb.
        let profile_outcome = match stub_path.as_ref() {
            Some(stub) => {
                self.run_working_set_profile_pass(
                    image_dir,
                    capture_cfg,
                    metadata.clone(),
                    stub,
                    manifest_ref,
                    work.path(),
                )
                .await
            }
            None => Ok(None),
        };
        match profile_outcome {
            Ok(Some(ws_path)) => {
                tracing::info!(
                    path = %ws_path.display(),
                    "M1.14 profile pass produced working-set trace",
                );
            }
            Ok(None) => {
                tracing::info!("M1.14 profile pass skipped (uffd_handler_bin / blob_root not set)",);
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "M1.14 profile pass failed; warm-pool refill will fall back to full-manifest prefetch",
                );
            }
        }

        // Primary bake VM already destroyed above (before the
        // profile pass) — the profile pass uses a separate FC
        // backend on a fresh work_dir.

        Ok(metadata)
    }

    /// ADR 0014 M1.14: synthetic profile pass. Restores the
    /// just-baked snapshot in a SECOND FC backend (UFFD mode) wired
    /// to dump the working-set trace to a file, dials vsock to drive
    /// activity, then tears down. Returns the staged trace path on
    /// success (also stages it as `image_dir/snapshot.working_set.json`),
    /// `Ok(None)` when skipped (caller didn't supply uffd handler /
    /// blob root), or `Err(...)` on a real failure (caller logs +
    /// proceeds without a trace).
    async fn run_working_set_profile_pass(
        &self,
        image_dir: &Path,
        capture_cfg: &CanonicalCaptureConfig,
        metadata: engram_core::types::snapshot::SnapshotMetadata,
        stub_path: &Path,
        memory_manifest_ref: engram_chunk_store::ManifestRef,
        primary_work_dir: &Path,
    ) -> Result<Option<PathBuf>, BuildError> {
        use engram_agentd::{SpawnHarnessRequest, WireRequest, WireResponse};
        use engram_sandbox_firecracker::{FirecrackerBackend, FirecrackerConfig, RestoreMode};
        use std::time::Duration;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::UnixStream;

        // Reserved vsock port engram-agentd listens on inside the
        // guest. Kept in sync with `engram_sandbox_firecracker::
        // ENGRAM_AGENTD_PORT` via the same hardcoded constant.
        const ENGRAM_AGENTD_PORT: u32 = 1024;

        let _ = memory_manifest_ref; // reserved for future telemetry
        let Some(uffd_bin) = capture_cfg.uffd_handler_bin.as_ref() else {
            return Ok(None);
        };
        let Some(blob_root) = capture_cfg.blob_root.as_ref() else {
            return Ok(None);
        };
        if !uffd_bin.exists() {
            return Err(BuildError::Config(format!(
                "uffd_handler_bin {} does not exist",
                uffd_bin.display()
            )));
        }

        // Reuse the bake's primary work_dir for the profile FC: the
        // snapshot files (state.bin, memory.bin, manifest.json) live
        // there. The primary sandbox was already destroyed, so the
        // jail dir is gone but the snapshots/ subtree survives.
        // Trace output lives in a separate scratch tempdir so we
        // can read it back regardless of what FC does to its jail.
        let trace_scratch = tempfile::tempdir()
            .map_err(|e| BuildError::Config(format!("profile-pass tempdir: {e}")))?;
        let trace_out = trace_scratch.path().join("working_set.json");

        // The bake patched the FC sidecar (manifest.json) in image_dir
        // with memory_manifest before staging. The primary's sidecar
        // is unpatched, so PooledBackend's `materialize_memory_if_missing`
        // path can't infer the manifest from it. Copy the patched one
        // back into the primary work_dir's snapshot dir before
        // restoring — that's what the runtime path reads.
        let primary_snap_dir = primary_work_dir
            .join("snapshots")
            .join(metadata.id.to_string());
        let patched_sidecar = image_dir.join("snapshot.sidecar.json");
        if patched_sidecar.exists() {
            tokio::fs::copy(&patched_sidecar, primary_snap_dir.join("manifest.json"))
                .await
                .map_err(|e| {
                    BuildError::Config(format!("profile-pass overlay patched sidecar: {e}"))
                })?;
        }

        let mut fc_cfg = FirecrackerConfig::with_kernel(&capture_cfg.kernel_image_path);
        if let Some(bin) = &capture_cfg.firecracker_bin {
            fc_cfg.firecracker_bin = bin.clone();
        }
        fc_cfg.uffd_handler_bin = uffd_bin.clone();
        fc_cfg.restore_mode = RestoreMode::Uffd;
        // Mirror the primary bake's net config so this profile FC's
        // `load_snapshot` can re-allocate the manifest's /30 + TAP.
        // Without this, restoring a snapshot baked with `net_pool =
        // Some(…)` here would fail trying to open the bake-time TAP
        // that doesn't exist on this allocator.
        fc_cfg.net_pool = capture_cfg.net_pool;
        fc_cfg.egress_proxy_port = None;
        fc_cfg.uffd_blob_root = Some(blob_root.clone());
        fc_cfg.stub_harness_path = Some(stub_path.to_path_buf());
        // Same CPU template the primary bake used — this restore is
        // loading the snapshot we just took, so the template MUST
        // match or the load fails (or worse, succeeds with a CPUID
        // mismatch the guest will trip over later).
        fc_cfg.cpu_template = engram_sandbox_firecracker::cpu_template_from_env();
        // host_id is required for UFFD restore (spawn_uffd_handler
        // passes --publish-trace-host when set, but the publish target
        // is the same BlobStorage as --blob-root — in dev that's the
        // bake's local store, which is fine).
        fc_cfg.host_id = Some(engram_core::HostId::new());
        fc_cfg.working_set_trace_output = Some(trace_out.clone());

        let profile_backend = FirecrackerBackend::new(primary_work_dir, fc_cfg);

        // Restore the just-baked snapshot. The FC backend's restore
        // path materializes the canonical-symlink + vsock-parent-dir
        // fixups; UFFD handler spawns and starts recording.
        let restored_id =
            engram_core::traits::SandboxBackend::restore(&profile_backend, metadata.clone())
                .await
                .map_err(|e| BuildError::Config(format!("profile-pass restore: {e}")))?;

        // Read the sidecar manifest from `image_dir` to discover
        // the bake-side vsock UDS path. FC re-bound the host-side
        // UDS at that exact path during load_snapshot (we ensured
        // the parent dir exists via restore_canonical_symlinks).
        let staged_sidecar = image_dir.join("snapshot.sidecar.json");
        let sidecar_bytes = tokio::fs::read(&staged_sidecar).await.map_err(|e| {
            BuildError::Config(format!(
                "profile-pass read staged sidecar {}: {e}",
                staged_sidecar.display()
            ))
        })?;
        let sidecar_value: serde_json::Value = serde_json::from_slice(&sidecar_bytes)
            .map_err(|e| BuildError::Config(format!("profile-pass parse sidecar: {e}")))?;
        let vsock_path: PathBuf = sidecar_value
            .get("source_vsock_canonical")
            .and_then(|v| v.as_str())
            .map(PathBuf::from)
            .ok_or_else(|| {
                BuildError::Config("profile-pass sidecar missing source_vsock_canonical".into())
            })?;

        // Dial agentd, send a synthetic SpawnHarness that mounts
        // the stub harness and exec's /bin/true. The exact argv
        // doesn't matter — what we want is for the kernel to walk
        // through mount(2) + execve(2) so the UFFD handler
        // observes the chunks underlying those code paths.
        let profile_result: Result<(), BuildError> = async {
            let mut stream = UnixStream::connect(&vsock_path).await.map_err(|e| {
                BuildError::Config(format!(
                    "profile-pass connect vsock {}: {e}",
                    vsock_path.display()
                ))
            })?;
            stream
                .write_all(format!("CONNECT {ENGRAM_AGENTD_PORT}\n").as_bytes())
                .await
                .map_err(|e| BuildError::Config(format!("profile-pass write CONNECT: {e}")))?;
            // Read FC's "OK <cid>\n" line.
            let mut header = Vec::new();
            loop {
                let mut byte = [0u8; 1];
                stream
                    .read_exact(&mut byte)
                    .await
                    .map_err(|e| BuildError::Config(format!("profile-pass read OK: {e}")))?;
                header.push(byte[0]);
                if byte[0] == b'\n' {
                    break;
                }
                if header.len() > 64 {
                    return Err(BuildError::Config(
                        "profile-pass FC vsock OK header too long".into(),
                    ));
                }
            }
            if !header.starts_with(b"OK ") {
                return Err(BuildError::Config(format!(
                    "profile-pass FC vsock unexpected header: {:?}",
                    String::from_utf8_lossy(&header),
                )));
            }
            // SpawnHarness with /bin/true. /dev/vdb is the stub
            // ext4 attached at bake time; mount it read-only at
            // /run/engram/harnesses/_profile so the ext4 mount(2)
            // code path runs.
            let req = WireRequest::SpawnHarness(SpawnHarnessRequest {
                argv: vec!["/bin/true".into()],
                env: Default::default(),
                harness_dev: Some("/dev/vdb".into()),
                harness_mount: Some("/run/engram/harnesses/_profile".into()),
            });
            engram_agentd::write_msg(&mut stream, &req)
                .await
                .map_err(|e| BuildError::Config(format!("profile-pass write SpawnHarness: {e}")))?;
            // Drain the response so we know agentd actually spawned
            // the child before we destroy the VM.
            let resp: WireResponse = engram_agentd::read_msg(&mut stream).await.map_err(|e| {
                BuildError::Config(format!("profile-pass read SpawnHarness response: {e}"))
            })?;
            if let WireResponse::Error { kind, message } = resp {
                return Err(BuildError::Config(format!(
                    "profile-pass SpawnHarness rejected ({kind}): {message}"
                )));
            }
            // Let the kernel fault its way through mount(2) + execve(2).
            // The UFFD handler's recorder window defaults to 5s; sleep
            // a hair longer than the synthetic activity so we capture
            // the tail faults too.
            tokio::time::sleep(Duration::from_secs(3)).await;
            Ok(())
        }
        .await;

        // Always destroy. The UFFD handler will exit and dump the
        // trace to `trace_out` on clean shutdown.
        let _ = engram_core::traits::SandboxBackend::destroy(&profile_backend, restored_id).await;
        profile_result?;

        // Give the UFFD handler a generous beat to land its stdout
        // / file write — destroy returns when FC is gone but the
        // handler exits asynchronously.
        for _ in 0..20 {
            if tokio::fs::metadata(&trace_out).await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        if !trace_out.exists() {
            // Dump the UFFD handler's log (parked next to trace_out by
            // spawn_uffd_handler) so the caller sees *why* the handler
            // bailed without producing a trace.
            let log_path = trace_out
                .parent()
                .map(|d| d.join("uffd-handler.log"))
                .unwrap_or_else(|| trace_out.with_extension("log"));
            let log_tail = tokio::fs::read_to_string(&log_path)
                .await
                .unwrap_or_else(|_| "<uffd-handler log not found>".into());
            return Err(BuildError::Config(format!(
                "profile-pass UFFD handler exited but no trace file produced\n--- uffd-handler.log ---\n{log_tail}"
            )));
        }

        // Stage the trace into image_dir alongside state.bin + sidecar
        // so `push_to_registry` includes it as an OCI layer.
        let staged = image_dir.join("snapshot.working_set.json");
        tokio::fs::copy(&trace_out, &staged).await.map_err(|e| {
            BuildError::Config(format!(
                "profile-pass stage trace to {}: {e}",
                staged.display()
            ))
        })?;
        Ok(Some(staged))
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
        oci: &engram_oci::OciClient,
        req: &BuildRequest,
        outcome: &BuildOutcome,
        registry_uri: &str,
    ) -> Result<RegistryPush, BuildError> {
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
            let (memory_bootstrap_json, memory_chunks_blob) = match (
                outcome.memory_bootstrap_path.as_ref(),
                outcome.memory_chunks_blob_path.as_ref(),
            ) {
                (Some(bs), Some(blob)) => (
                    Some(tokio::fs::read(bs).await.map_err(BuildError::Io)?),
                    Some(tokio::fs::read(blob).await.map_err(BuildError::Io)?),
                ),
                _ => (None, None),
            };
            // ADR 0014: ship state.bin + sidecar as OCI layers so
            // the deployment's coord can materialize them to its
            // own BlobStorage on enable-image. Both-or-neither
            // symmetry is enforced by `push_chunked_image`.
            let (snapshot_state, snapshot_sidecar_json) = match (
                outcome.snapshot_state_path.as_ref(),
                outcome.snapshot_sidecar_path.as_ref(),
            ) {
                (Some(s), Some(c)) => (
                    Some(tokio::fs::read(s).await.map_err(BuildError::Io)?),
                    Some(tokio::fs::read(c).await.map_err(BuildError::Io)?),
                ),
                _ => (None, None),
            };
            // ADR 0014 M1.14: optional working-set trace from the
            // synthetic profile pass. May be absent on older bakes
            // or when the profile pass was skipped/failed.
            let snapshot_working_set_json = match outcome.snapshot_working_set_path.as_ref() {
                Some(p) if p.exists() => Some(tokio::fs::read(p).await.map_err(BuildError::Io)?),
                _ => None,
            };

            let payload = engram_oci::ChunkedPushPayload {
                manifest_toml: manifest_bytes,
                config_json: config_bytes,
                bundle_json: bundle.clone(),
                disk_bootstrap_json,
                disk_chunks_blob,
                memory_bootstrap_json,
                memory_chunks_blob,
                snapshot_state,
                snapshot_sidecar_json,
                snapshot_working_set_json,
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

#[cfg(test)]
mod tests {
    use engram_core::types::snapshot::SnapshotMetadata;

    /// Regression guard for the bake → cascade contract: bundle.json's
    /// `canonical_snapshot` block must round-trip cleanly through
    /// `SnapshotMetadata`'s serde shape. A previous hand-rolled
    /// `serde_json::json!({...})` in the bake used the wrong field
    /// names ("snapshot_id" instead of "id", missing required
    /// `image_version`); the coord's
    /// `serde_json::from_value::<SnapshotMetadata>(snap).ok()` silently
    /// returned None and the templates cascade never fired in prod.
    #[test]
    fn bundle_canonical_snapshot_round_trips_through_snapshot_metadata() {
        let original = SnapshotMetadata {
            id: engram_core::SnapshotId::new(),
            size_bytes: 4096,
            created_at: chrono::Utc::now(),
            image_version: "warm-test".into(),
            disk_manifest: None,
            memory_manifest: None,
            source_sandbox_id: None,
            state_blob_key: Some("state".into()),
            sidecar_blob_key: Some("sidecar".into()),
            rootfs_blob_key: None,
            working_set_blob_key: None,
        };

        // Same serialization the bake uses to populate
        // bundle["canonical_snapshot"].
        let value = serde_json::to_value(&original).expect("serialize");

        // Same deserialization the cascade uses to read it back.
        let parsed: SnapshotMetadata =
            serde_json::from_value(value).expect("cascade-side deserialize");

        assert_eq!(parsed.id, original.id);
        assert_eq!(parsed.image_version, original.image_version);
        assert_eq!(parsed.state_blob_key, original.state_blob_key);
        assert_eq!(parsed.sidecar_blob_key, original.sidecar_blob_key);
    }

    /// Regression guard for the silent best-effort behavior that
    /// produced `demo:warm-75babf7` in prod. Prior to this commit,
    /// `Err(_)` from the bake-time `capture_canonical_memory` call
    /// degraded to `(None, None)` + a tracing::warn, the bake's
    /// exit code stayed zero, and the pushed artifact had
    /// `bundle.json` schema_version=2 with no canonical_snapshot —
    /// coord's enable-image cascade silently skipped the templates
    /// write, warm pool never fired, every session cold-created
    /// (~27s).
    ///
    /// This test pins the new shape: BuildError::CanonicalCapture
    /// exists and displays with a clear prefix. The actual
    /// "capture-fails-aborts-bake" semantic is exercised in
    /// integration tests under `tests/builder.rs` (gated behind
    /// docker availability); this is the compile-time guarantee
    /// that the variant and its Display impl are wired.
    #[test]
    fn canonical_capture_error_variant_exists_and_displays_clearly() {
        let err = super::BuildError::CanonicalCapture(
            "ip tuntap add tap-engr-9f5afd mode tap: Operation not permitted".into(),
        );
        let msg = format!("{err}");
        assert!(
            msg.starts_with("canonical memory capture:"),
            "must surface the failure mode loudly in the prefix; got: {msg}"
        );
        assert!(
            msg.contains("tuntap"),
            "must thread the original error through so operators \
             can diagnose without chasing log files; got: {msg}"
        );
    }
}
