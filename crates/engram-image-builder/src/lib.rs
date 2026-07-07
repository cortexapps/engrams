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
    /// Optional: write the ADR 0080 stage-1 init shim to
    /// `/sbin/engram-init`. The shim mounts the essentials, mounts the
    /// aux bundle slots, copies `engram-agentd` out of its reserved
    /// bundle slot to tmpfs, and exec's the copy on a vsock port —
    /// agentd itself is NEVER baked into the rootfs. Pair with FC's
    /// `default_boot_args = "... init=/sbin/engram-init"` and a host
    /// that stages `bundle-agentd`.
    pub init_injection: Option<InitInjection>,
}

/// How to put the stage-1 init shim inside the rootfs at bake time.
/// Optional because the dev backend doesn't need it — only Firecracker
/// (and VZ) images do. ADR 0080: this used to also bake the agentd
/// binary; agentd now rides its reserved bundle slot so it iterates
/// with zero re-bakes, and the shim is the only engrams file in the
/// rootfs.
#[derive(Clone, Debug)]
pub struct InitInjection {
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

/// Which `engram-transport` implementation the in-VM binaries should
/// select at runtime. Set on [`InitInjection`] at bake time; the
/// default init shim writes `ENGRAM_TRANSPORT=<value>` into the rootfs
/// so `engram-transport::from_env` picks the right impl.
///
/// Both backends now use `Vsock` — VZ migrated off virtio-console onto
/// Apple's real `VZVirtioSocketDevice` in ADR 0066 Phase 2. The enum
/// stays a seam for a future non-vsock backend.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Transport {
    /// AF_VSOCK — the transport for both Firecracker and VZ.
    #[default]
    Vsock,
}

impl Transport {
    /// Value for `ENGRAM_TRANSPORT` in the init shim. Lowercase
    /// matches what `engram-transport::from_env` parses.
    pub fn env_value(self) -> &'static str {
        match self {
            Self::Vsock => "vsock",
        }
    }

    /// Parse a CLI flag value (case-insensitive). Used by
    /// `engram-cli image build --transport=...`.
    pub fn parse(s: &str) -> Result<Self, String> {
        match s.to_ascii_lowercase().as_str() {
            "vsock" => Ok(Self::Vsock),
            other => Err(format!("invalid transport: {other} (expected vsock)")),
        }
    }
}

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
/// [`InitInjection`] is requested without an explicit override.
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
# /dev/shm is POSIX shared memory (tmpfs). Standard Linux init mounts it;
# our minimal devtmpfs /dev doesn't carry it. Chromium (the ADR 0027
# playwright bundle) allocates its renderer's shared memory here and the
# page process *crashes* without it — and the `--disable-dev-shm-usage`
# fallback writes to the system tmpdir, which isn't a world-writable 1777
# /tmp in our guest, so that path fails too. A real /dev/shm is the fix.
mkdir -p /dev/shm 2>/dev/null || true
mount -t tmpfs -o nosuid,nodev,mode=1777 tmpfs /dev/shm 2>/dev/null || true
# /run as tmpfs — standard Linux init behavior, and load-bearing for
# ADR 0080: agentd executes from a tmpfs COPY (/run/engram/) so its
# text pages are guest memory and the host's paused-window
# patch_drive swap of the agentd bundle device can never fault a
# running binary's pages from swapped bytes (the same asymmetry that
# makes the harness-slot swap safe). Everything staged below
# (.ca-stage, engram/harnesses, engram/) lands on this tmpfs.
mount -t tmpfs -o nosuid,nodev,mode=0755 tmpfs /run 2>/dev/null || true
# /tmp must be the standard world-writable, sticky 1777 dir. Our rootfs
# ships it as 0755 owned by the session user, which blocks writes from any
# other uid — e.g. a root `engram exec`, or chromium's renderer running with
# dropped capabilities. The ADR 0027 browser bundle hits this twice: the
# chromium shm fallback and playwright's video-artifacts temp dir both land
# under the system tmpdir and silently fail (renderer crash / "no videos were
# recorded"). Restore the convention so any uid can use /tmp. (ADR 0027 e2e.)
mkdir -p /tmp 2>/dev/null || true
chmod 1777 /tmp 2>/dev/null || true
mark fs_mounts_done
# DNS for userspace. The kernel handled IP+routes via `ip=dhcp` (see
# vz-backend kernel cmdline); IP_PNP doesn't write resolv.conf, so
# we do it here. Backend-aware by the guest's OWN address: VZ guests
# get 192.168.64.x from Apple's DHCP and the NAT gateway
# (192.168.64.1) answers DNS, so it goes first (VZ dev has no egress
# proxy). FC guests live in the 10.200/16 netns pool behind the
# MANDATORY egress proxy (issue #240): the host iptables REDIRECTs
# guest {udp,tcp}/53 to the filtering DNS proxy regardless of the
# destination IP, and that proxy NXDOMAINs anything outside
# `manifest.network.allow_hosts`. So FC points at its own gateway
# (10.200.0.1, where the REDIRECT lives) — NOT a public resolver.
# Writing `1.1.1.1` here used to be a DNS-tunnel exfiltration hatch
# (a malicious harness encodes data into subdomains of an attacker-
# controlled name); it's gone. Listing a dead 192.168.64.1 first on
# FC cost every uncached lookup a ~5s first-nameserver timeout
# (prod-found 2026-06-12), so FC writes the single gateway entry.
# timeout:2/attempts:2 bounds the residual worst case. Skip if
# /etc/resolv.conf already exists (operator override).
mkdir -p /etc
if [ ! -s /etc/resolv.conf ]; then
    # Shell-pure VZ detection (no ip/grep dependency — the shim only
    # assumes /bin/sh): the guest's local addresses appear in
    # /proc/net/fib_trie; a 192.168.64.x entry means Apple's VZ NAT.
    # Readability-guarded so a kernel without the file can't trip
    # `set -e` and kill init.
    vz_nat=""
    if [ -r /proc/net/fib_trie ]; then
        while read -r fib_line; do
            case "$fib_line" in
                *192.168.64.*) vz_nat=1; break ;;
            esac
        done < /proc/net/fib_trie
    fi
    if [ -n "$vz_nat" ]; then
        printf 'nameserver 192.168.64.1\noptions timeout:2 attempts:2\n' > /etc/resolv.conf
    else
        # FC: the gateway is where the host's DNS REDIRECT sends
        # :53 to the filtering proxy. No public-resolver fallback.
        printf 'nameserver 10.200.0.1\noptions timeout:2 attempts:2\n' > /etc/resolv.conf
    fi
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
# ADR 0080: seed the SHELL-tab ergonomics (colors + a two-tone prompt)
# if the image doesn't ship its own — engrams-owned polish that no
# longer belongs in the user's Dockerfile. Quoted heredoc: zero
# interpolation, the PS1 escapes land verbatim.
if [ ! -s /root/.bashrc ]; then
    cat > /root/.bashrc <<'ENGRAM_BASHRC'
export TERM=xterm-256color
alias ls="ls --color=auto"
alias ll="ls -lah --color=auto"
alias grep="grep --color=auto"
PS1='\[\e[36m\]\u@\h\[\e[0m\]:\[\e[34m\]\w\[\e[0m\]\$ '
ENGRAM_BASHRC
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
# ADR 0055: mount each reserved dynamic-mount slot the host attached as an
# extra read-only virtio-blk drive. Slots carry a sentinel at base-snapshot
# capture; a per-session create patch_drives the profile-selected skills into
# the slots' devices in the paused restore window. We mount every read-only
# bundle device (squashfs on FC, erofs on VZ — the ext4 CA drive is never
# matched) at a sequential /opt/engram/dyn/<i> — the index tracks the host's
# slot order (FC preserves attach order). The mounts freeze into the base
# snapshot's VFS; on a fresh-create restore the host has swapped some slots'
# devices, and agentd umount/remounts /opt/engram/dyn/* at session bind so
# each superblock re-parses its (possibly swapped) device (ADR 0035 §3).
# agentd then reads each mount's mount.json to wire skills (sentinels are
# skipped). Best-effort.
i=0
for dev in /dev/vd*; do
    [ -b "$dev" ] || continue
    [ "$dev" = "/dev/vda" ] && continue  # rootfs
    mkdir -p "/opt/engram/dyn/$i" 2>/dev/null || true
    if mount -t squashfs -o ro "$dev" "/opt/engram/dyn/$i" 2>/dev/null || \
       mount -t erofs -o ro "$dev" "/opt/engram/dyn/$i" 2>/dev/null; then
        i=$((i + 1))
    else
        rmdir "/opt/engram/dyn/$i" 2>/dev/null || true  # not a bundle device (e.g. CA ext4)
    fi
done
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
# ADR 0080: agentd is NOT baked into this rootfs — it rides its
# reserved bundle slot (engram-agentd + agentd.sha256). Probe the
# mounted slots for it (position-independent: FC keeps dyn/<i> ==
# slot i, VZ compacts resolved drives), copy binary + content stamp
# to tmpfs, and exec the COPY. The stamp is what a later
# RefreshAgent compares against the (possibly patch_drive-swapped)
# slot to decide a re-exec. PID 1 exiting panics the kernel
# (panic=1) — a boot without the agentd bundle fails loud, with the
# reason on the guest console.
AGENTD_DIR=""
for d in /opt/engram/dyn/*; do
    if [ -x "$d/engram-agentd" ]; then
        AGENTD_DIR="$d"
        break
    fi
done
if [ -z "$AGENTD_DIR" ]; then
    echo "engram-init: FATAL: no agentd bundle mounted under /opt/engram/dyn — stage bundle-agentd on the host (ADR 0080)" >&2
    exit 1
fi
mkdir -p /run/engram
cp "$AGENTD_DIR/engram-agentd" /run/engram/engram-agentd
chmod 0755 /run/engram/engram-agentd
if [ -f "$AGENTD_DIR/agentd.sha256" ]; then
    cp "$AGENTD_DIR/agentd.sha256" /run/engram/agentd.sha256
fi
mark agentd_staged
exec /run/engram/engram-agentd --port __VSOCK_PORT__
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
    /// ADR 0008 Phase 3 / ADR 0036: per-chunk bootstrap JSON for
    /// the disk side. Sits next to `rootfs.ext4` in `image_dir`
    /// (e.g. `image_dir/bootstrap.disk.json`). `None` for non-Ext4
    /// bakes (Directory) or when chunked-OCI emission was skipped.
    /// `push_to_registry` reads it to drive the per-chunk delta
    /// push (chunk bytes come from the bake's chunk store).
    pub disk_bootstrap_path: Option<PathBuf>,
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
    /// OCI client for registry pushes. Optional so tests that exercise
    /// only the local bake pipeline don't have to construct one; absence
    /// is surfaced as a clear error when a push actually needs it.
    oci: Option<engram_oci::OciClient>,
}

impl<D: DockerRunner> Builder<D, Mke2fsPacker> {
    /// Default constructor: real `mke2fs` packer for `Format::Ext4`,
    /// caller-supplied chunk store. Call [`Self::with_oci`] afterwards
    /// to attach the OCI client needed for registry pushes.
    pub fn new(docker: D, chunk_store: engram_chunk_store::ChunkStore) -> Self {
        Self {
            docker,
            packer: Mke2fsPacker::default(),
            chunk_store,
            oci: None,
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
        }
    }

    /// Attach the OCI client. Required for [`Self::push_to_registry`]
    /// (image push). Returns `self` so it composes with
    /// `Builder::new(...).with_oci(...)`.
    pub fn with_oci(mut self, oci: engram_oci::OciClient) -> Self {
        self.oci = Some(oci);
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
                // `[build] build_secrets` → BuildKit `--secret id=,env=`;
                // values come from the bake process env, never a layer.
                build_secrets: cfg.build.build_secrets.clone(),
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
            .build_after_container(req, &cfg, &image_dir, &container_id, &docker_tag)
            .await;
        // Cleanup: `build_after_container` already frees the image+container
        // on its success path (after export, before the ext4 pack — see
        // there). This trailing pass covers the error paths and is an
        // idempotent no-op on success.
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
        docker_tag: &str,
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
        // we just overlay the stage-1 init shim on top.
        //
        // ADR 0062: the image bakes NO harness. ADR 0080: it bakes NO
        // agentd either — both ride reserved bundle slots so they iterate
        // with zero re-bakes. The one engrams file in the rootfs is the
        // stage-1 init shim below (mounts the slots, copies agentd to
        // tmpfs, execs it).
        let mut effective_manifest = cfg.to_manifest();
        if let Some(injection) = &req.init_injection {
            inject_init(&rootfs_dir, injection).await?;
        }

        // ADR 0027: the share-file skill + the git forge glue is no
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

        // Free the docker image + container NOW. The rootfs is fully
        // exported to `rootfs_dir` and the last image read (inspect_config
        // above) is done, so nothing below — recursive_size, the ext4
        // pack, chunking — needs them. This is load-bearing for large warm
        // images: otherwise the ext4 pack runs with the image's layers
        // (~1x), the exported tree (~1x), AND the ext4 (~1x) all on disk at
        // once (~3x the image size), which overruns a CI runner with
        // `mke2fs: No space left on device`. Freeing here drops the pack's
        // peak to ~2x (tree + ext4). The caller's trailing cleanup is then
        // an idempotent no-op (and still covers the pre-export error paths).
        let _ = self.docker.rm_container(container_id).await;
        let _ = self.docker.rmi(docker_tag).await;
        // `rmi` drops the image reference but NOT the BuildKit cache, which
        // holds a full ~image-sized copy of the just-built layers. For a large
        // warm image (e.g. dev-brain: ~33 GiB tree → ~67 GiB ext4) that cache
        // lingers on disk through the pack below — tree + ext4 + cache then
        // overruns the CI runner with `mke2fs: No space left on device`.
        // Prune it now so the pack's peak is just (exported tree + ext4).
        // Best-effort: a prune failure must not fail the bake.
        if let Err(e) = self.docker.builder_prune().await {
            tracing::warn!(error = %e, "docker builder prune failed (non-fatal)");
        }

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
                tracing::info!(
                    dir_size_bytes = dir_size,
                    dir_size_gib = dir_size as f64 / (1024.0 * 1024.0 * 1024.0),
                    ext4_size_bytes = ext4_size,
                    ext4_size_gib = ext4_size as f64 / (1024.0 * 1024.0 * 1024.0),
                    "sizing ext4 image from source-tree disk usage (du-style)"
                );
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
                // ADR 0036 P4: the bake's manifest identity is derived
                // from its content, not minted at random. Deterministic
                // re-bakes of unchanged content therefore reproduce the
                // SAME ManifestRef — bundle.json stays byte-identical,
                // and the enable pipeline can recognize "already
                // captured this exact rootfs" and reuse the base
                // snapshot. Content-derived also means the same ref ⇒
                // the same manifest bytes, so an already-present
                // manifest (a prior identical bake) is success, not a
                // VersionConflict.
                let manifest_ref = m.content_ref();
                match self.chunk_store.get_manifest(manifest_ref).await {
                    Ok(_) => {
                        tracing::debug!(
                            manifest = %manifest_ref,
                            "content-identical manifest already in store; skipping put"
                        );
                    }
                    Err(_) => self.chunk_store.put_manifest(manifest_ref, &m).await?,
                }

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

        // ADR 0008 Phase 3 / ADR 0036: produce the per-chunk
        // bootstrap alongside the existing bundle.json. Pure
        // metadata — every entry addresses its chunk by the chunk's
        // own sha256 (also its OCI blob digest and its BlobStorage
        // key), so no concatenated chunk blob is built or written.
        // `push_to_registry` pushes each chunk the registry is
        // missing as its own blob; the runtime resolver indexes the
        // bootstrap at pull time.
        let mut disk_bootstrap_path = None;
        if let Some(disk_ref) = disk_manifest {
            let manifest = self
                .chunk_store
                .get_manifest(disk_ref)
                .await
                .map_err(|e| BuildError::Config(format!("re-read disk manifest: {e}")))?;
            let bootstrap = engram_chunk_store::Bootstrap::build_per_chunk(&manifest);
            let bs_path = image_dir.join("bootstrap.disk.json");
            tokio::fs::write(
                &bs_path,
                serde_json::to_vec(&bootstrap)
                    .map_err(|e| BuildError::Config(format!("disk bootstrap json: {e}")))?,
            )
            .await?;
            disk_bootstrap_path = Some(bs_path);
        }

        // Re-write bundle.json with the disk_manifest now that the
        // ext4 branch (above) wrote a baseline. Schema bumps to v2
        // when the chunked-OCI bootstrap was produced — v1 readers
        // still see `disk_manifest` and work; v2 readers also
        // consult `bootstrap_disk_available` for chunk-on-fault.
        if let Some(disk_ref) = disk_manifest {
            let schema_version = if disk_bootstrap_path.is_some() { 2 } else { 1 };
            let mut bundle = serde_json::json!({
                "schema_version": schema_version,
                "disk_manifest": disk_ref,
            });
            if disk_bootstrap_path.is_some() {
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

        // ADR 0036: when the bake produced a per-chunk bootstrap,
        // push a chunked OCI artifact — one blob per chunk, delta
        // style. HEAD each chunk digest and upload only the blobs
        // the registry is missing (a deterministic re-bake re-pushes
        // only its delta), then push the manifest referencing every
        // chunk layer. Failures cost one 16 MiB blob retry, never a
        // monolithic multi-GB upload session (the pre-ADR-0036
        // failure mode), and the artifact stays clear of registries'
        // per-layer size ceilings.
        if let (Some(bs_path), Some(bundle)) =
            (outcome.disk_bootstrap_path.as_ref(), bundle_bytes.as_ref())
        {
            let disk_bootstrap_json = tokio::fs::read(bs_path).await.map_err(BuildError::Io)?;
            let bootstrap: engram_chunk_store::Bootstrap =
                serde_json::from_slice(&disk_bootstrap_json)
                    .map_err(|e| BuildError::Config(format!("re-read disk bootstrap: {e}")))?;

            let (pushed, skipped) = self.push_chunk_blobs(oci, &full_uri, &bootstrap).await?;
            tracing::info!(
                uri = %full_uri,
                pushed,
                skipped,
                total = bootstrap.entries.len(),
                "chunk blobs delta-pushed to registry"
            );

            let chunk_refs: Vec<engram_oci::ChunkLayerRef> = bootstrap
                .entries
                .iter()
                .map(|e| {
                    Ok(engram_oci::ChunkLayerRef {
                        digest: e.blob_digest.clone().ok_or_else(|| {
                            BuildError::Config(format!(
                                "bootstrap entry {} missing per-chunk blob digest",
                                e.sha256
                            ))
                        })?,
                        size: e.length as u64,
                    })
                })
                .collect::<Result<_, BuildError>>()?;

            let layers = engram_oci::ChunkedImageLayers {
                manifest_toml: manifest_bytes,
                config_json: config_bytes,
                bundle_json: bundle.clone(),
                disk_bootstrap_json,
            };
            let digest = oci
                .push_chunked_image_manifest(&full_uri, layers, &chunk_refs)
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

    /// ADR 0036: delta-push every chunk in `bootstrap` as its own
    /// content-addressed OCI blob. For each entry: HEAD the digest
    /// (skip when the registry already has it — the cross-bake dedup
    /// that makes deterministic re-bakes upload only their delta),
    /// else read the chunk from the bake's chunk store and push it.
    ///
    /// Bounded concurrency keeps peak memory at
    /// `concurrency × chunk_size` (default 32 × 16 MiB ≈ 512 MiB)
    /// regardless of image size; per-blob retry with backoff means a
    /// transient blip costs one 16 MiB re-upload, not the whole push.
    /// The in-flight count is [`push_concurrency`] (env-tunable) —
    /// GHCR throttles per-connection, so a fat warm-image push stays
    /// network-bound rather than serialized behind a handful of streams.
    ///
    /// Returns `(pushed, skipped)` counts.
    async fn push_chunk_blobs(
        &self,
        oci: &engram_oci::OciClient,
        uri: &str,
        bootstrap: &engram_chunk_store::Bootstrap,
    ) -> Result<(usize, usize), BuildError> {
        use futures::stream::{FuturesUnordered, StreamExt};

        // One token mint for the whole run; concurrent first-pushes
        // would otherwise each eat a 401 + re-auth.
        oci.auth_for_push(uri)
            .await
            .map_err(|e| BuildError::Docker(format!("registry auth: {e}")))?;

        let concurrency = push_concurrency();
        const MAX_ATTEMPTS: u32 = 5;

        async fn push_one(
            oci: &engram_oci::OciClient,
            chunk_store: &engram_chunk_store::ChunkStore,
            uri: &str,
            entry: engram_chunk_store::BootstrapEntry,
            total: usize,
            done_so_far: usize,
        ) -> Result<bool, BuildError> {
            let digest = entry.blob_digest.as_deref().ok_or_else(|| {
                BuildError::Config(format!(
                    "bootstrap entry {} missing per-chunk blob digest",
                    entry.sha256
                ))
            })?;
            if oci
                .blob_exists(uri, digest)
                .await
                .map_err(|e| BuildError::Docker(format!("HEAD {digest}: {e}")))?
            {
                return Ok(false);
            }
            let bytes = chunk_store
                .get_chunk(entry.sha256)
                .await
                .map_err(|e| BuildError::Config(format!("read chunk {}: {e}", entry.sha256)))?;
            let mut attempt = 1u32;
            loop {
                match oci.push_chunk_blob(uri, digest, &bytes).await {
                    Ok(()) => break,
                    Err(e) if attempt < MAX_ATTEMPTS => {
                        // 429s want a politer pause than transient
                        // connection blips.
                        let rate_limited = e.to_string().contains("429");
                        let backoff_ms = if rate_limited { 5_000 } else { 500 } * attempt as u64;
                        tracing::warn!(
                            chunk = %entry.sha256,
                            attempt,
                            rate_limited,
                            error = %e,
                            "chunk blob push failed; retrying with backoff"
                        );
                        tokio::time::sleep(std::time::Duration::from_millis(backoff_ms)).await;
                        attempt += 1;
                    }
                    Err(e) => {
                        return Err(BuildError::Docker(format!(
                            "push chunk {} after {attempt} attempts: {e}",
                            entry.sha256
                        )));
                    }
                }
            }
            // Progress is operator-facing: bakes run in CI where this
            // log line is the only signal the push is alive.
            if done_so_far.is_multiple_of(25) {
                tracing::info!(done = done_so_far, total, "chunk push progress");
            }
            Ok(true)
        }

        let mut iter = bootstrap.entries.iter().enumerate();
        let mut tasks = FuturesUnordered::new();
        let mut pushed = 0usize;
        let mut skipped = 0usize;
        for _ in 0..concurrency {
            if let Some((i, entry)) = iter.next() {
                tasks.push(push_one(
                    oci,
                    &self.chunk_store,
                    uri,
                    entry.clone(),
                    bootstrap.entries.len(),
                    i,
                ));
            }
        }
        while let Some(res) = tasks.next().await {
            match res? {
                true => pushed += 1,
                false => skipped += 1,
            }
            if let Some((i, entry)) = iter.next() {
                tasks.push(push_one(
                    oci,
                    &self.chunk_store,
                    uri,
                    entry.clone(),
                    bootstrap.entries.len(),
                    i,
                ));
            }
        }
        Ok((pushed, skipped))
    }
}

/// In-flight concurrent blob uploads for a chunked-image push.
///
/// Default 32; override via `ENGRAM_PUSH_CONCURRENCY`. GHCR throttles
/// per-connection (warm-image bakes observe ~1.8 MB/s/stream), so the
/// aggregate push rate scales with concurrency until the registry's
/// own ceiling — bump the env var to probe it. Peak memory is
/// `N × 16 MiB` chunk, so a big N is RAM, not CPU, bound. A junk or
/// `0` value falls back to the default rather than wedging the push.
fn push_concurrency() -> usize {
    parse_push_concurrency(std::env::var("ENGRAM_PUSH_CONCURRENCY").ok())
}

/// Pure core of [`push_concurrency`], split out so the parse/clamp is
/// unit-testable without poking the process environment.
fn parse_push_concurrency(raw: Option<String>) -> usize {
    const DEFAULT: usize = 32;
    raw.and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|&n| n >= 1)
        .unwrap_or(DEFAULT)
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
async fn inject_init(rootfs_dir: &Path, injection: &InitInjection) -> Result<(), BuildError> {
    let init_dst = rootfs_dir.join(INIT_PATH);
    if let Some(parent) = init_dst.parent() {
        tokio::fs::create_dir_all(parent).await?;
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

/// Sum the *actual disk usage* (allocated 512-byte blocks, à la `du`) of every
/// entry under `dir`, without following symlinks.
///
/// We size from `st_blocks`, NOT apparent file length (`meta.len()`): the
/// brain/gradle/pnpm caches we bake into warm dev images are hundreds of
/// thousands of tiny files, and ext4 rounds every file up to a 4 KiB block
/// (plus a block per directory). Summing apparent lengths undercounts real
/// block consumption by 2-4× for such trees, so `recommended_size`'s 2×
/// headroom still undershot and `mke2fs -d` hit ENOSPC mid-populate (the
/// dev-brain bake). Block usage captures the rounding, directory blocks, and
/// xattr/inline overhead directly.
///
/// Counting is per-entry, so a hardlink (pnpm's content-addressed store links
/// into `node_modules`) is counted once per link — an overcount, but in the
/// safe direction (a slightly larger fs is fine; a too-small one is fatal).
/// Not following symlinks matches `count_entries` and `mke2fs -d`, which
/// replicates a symlink as a symlink: we count the link inode's own blocks and
/// reach a target only if it lives in the real tree. `symlink_metadata`
/// (lstat) also can't error on broken symlinks, unlike the old `metadata`.
async fn recursive_size(dir: &Path) -> std::io::Result<u64> {
    use std::os::unix::fs::MetadataExt;
    let mut total = 0u64;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let mut entries = match tokio::fs::read_dir(&d).await {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
        };
        while let Some(entry) = entries.next_entry().await? {
            // `file_type()` is readdir's d_type — it reflects the entry
            // itself, not a symlink target, so we descend only real dirs.
            let ft = entry.file_type().await?;
            // lstat: count the entry's own allocated blocks (st_blocks is in
            // 512-byte units), never the symlink target.
            let meta = tokio::fs::symlink_metadata(entry.path()).await?;
            total = total.saturating_add(meta.blocks().saturating_mul(512));
            if ft.is_dir() {
                stack.push(entry.path());
            }
        }
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// issue #240: the FC branch of the init shim's resolv.conf writer
    /// must NOT hand the guest a public recursive resolver. With the
    /// mandatory egress proxy, the host REDIRECTs guest :53 to the
    /// filtering DNS proxy; the guest is pointed at its gateway
    /// (10.200.0.1) where that REDIRECT lives, and a public-resolver
    /// nameserver would be a DNS-tunnel exfiltration hatch. Before the
    /// fix the shim wrote `nameserver 1.1.1.1` for FC; this guards the
    /// regression.
    #[test]
    fn init_shim_fc_resolv_conf_has_no_public_resolver() {
        // The FC (non-VZ) branch is the `else` that writes a single
        // gateway nameserver. It must point at the netns gateway and
        // carry no public resolver.
        assert!(
            DEFAULT_INIT_SHIM.contains("nameserver 10.200.0.1"),
            "FC guest must resolve via its gateway (where the host DNS REDIRECT lives)",
        );
        assert!(
            !DEFAULT_INIT_SHIM.contains("nameserver 1.1.1.1"),
            "init shim must not write a public resolver (1.1.1.1) — DNS-exfil hatch (#240)",
        );
        // The stale FIXME(dns-exfil) is resolved and should be gone.
        assert!(
            !DEFAULT_INIT_SHIM.contains("FIXME(dns-exfil)"),
            "the DNS-exfil FIXME is fixed; the stale marker should be removed",
        );
    }

    /// ADR 0061: the dyn-mount loop must try squashfs first (FC path) then
    /// erofs (VZ's Kata kernel has no CONFIG_SQUASHFS). The ext4 CA drive
    /// must never be matched because only read-only bundle formats are
    /// attempted.
    #[test]
    fn init_shim_dyn_mount_tries_squashfs_then_erofs() {
        assert!(
            DEFAULT_INIT_SHIM.contains("mount -t squashfs -o ro"),
            "dyn-mount loop must try squashfs first (FC path)",
        );
        assert!(
            DEFAULT_INIT_SHIM.contains("mount -t erofs -o ro"),
            "dyn-mount loop must try erofs as fallback (VZ / Kata path)",
        );
        // The dyn-mount loop iterates /dev/vd* and skips /dev/vda (rootfs).
        // It must only attempt read-only bundle formats (squashfs, erofs), never
        // ext4 — otherwise the CA ext4 drive on /dev/vdb would be double-mounted.
        // We verify the dyn-mount loop section (between "for dev in /dev/vd*" and
        // "mark bundles_mounted") contains no "mount -t ext4" invocation.
        let shim = DEFAULT_INIT_SHIM;
        let loop_start = shim
            .find("for dev in /dev/vd*")
            .expect("dyn-mount loop must be present");
        let loop_end = shim
            .find("mark bundles_mounted")
            .expect("bundles_mounted mark must be present");
        let dyn_loop_section = &shim[loop_start..loop_end];
        assert!(
            !dyn_loop_section.contains("mount -t ext4"),
            "dyn-mount loop must never attempt ext4 — that would match the CA drive",
        );
    }

    /// `ENGRAM_PUSH_CONCURRENCY` parsing: a valid override wins, but
    /// unset / junk / zero all fall back to the default (32) rather
    /// than wedging the push at 0 in-flight uploads.
    #[test]
    fn push_concurrency_parse_and_clamp() {
        assert_eq!(parse_push_concurrency(None), 32, "unset → default");
        assert_eq!(
            parse_push_concurrency(Some("64".into())),
            64,
            "valid override"
        );
        assert_eq!(parse_push_concurrency(Some(" 16 ".into())), 16, "trimmed");
        assert_eq!(
            parse_push_concurrency(Some("0".into())),
            32,
            "0 → default, never 0 streams"
        );
        assert_eq!(
            parse_push_concurrency(Some("nope".into())),
            32,
            "junk → default"
        );
        assert_eq!(
            parse_push_concurrency(Some("".into())),
            32,
            "empty → default"
        );
    }
}
