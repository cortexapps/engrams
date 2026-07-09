//! Stage-1 init injection — the ONE engrams file written into a
//! session rootfs (ADR 0080).
//!
//! ADR 0080: this module is the single home for the stage-1 init
//! shim, so both producers of a bootable rootfs — the retiring
//! `docker export` bake and this crate's OCI-layer materializer —
//! share a single shim source. The shim mounts the essentials, mounts
//! the aux bundle slots, copies `engram-agentd` out of its reserved
//! bundle slot to tmpfs, and exec's the copy; agentd itself is NEVER
//! baked into the rootfs.

use std::path::{Path, PathBuf};

/// How to put the stage-1 init shim inside the rootfs.
/// ADR 0080: this used to also bake the agentd binary; agentd now
/// rides its reserved bundle slot so it iterates with zero re-bakes,
/// and the shim is the only engrams file in the rootfs.
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
    /// Override the default init script. When `None`, the default
    /// [`DEFAULT_INIT_SHIM`] is written (a `/bin/sh` script that
    /// mounts `/proc`, `/sys`, `/dev`, the bundle slots, and `exec`s
    /// the slot-staged `engram-agentd`).
    pub init_script: Option<PathBuf>,
}

/// Which `engram-transport` implementation the in-VM binaries should
/// select at runtime. Set on [`InitInjection`]; the default init shim
/// writes `ENGRAM_TRANSPORT=<value>` into the rootfs so
/// `engram-transport::from_env` picks the right impl.
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
/// at injection time. Named for the original vsock-only world;
/// retained to keep this string stable across the multi-transport
/// refactor.
const VSOCK_PORT_PLACEHOLDER: &str = "__VSOCK_PORT__";

/// Placeholder swapped for `ENGRAM_TRANSPORT=vsock|console` at
/// injection time so `engram-transport::from_env` in the in-VM
/// binaries picks the matching impl.
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
pub const DEFAULT_INIT_SHIM: &str = r#"#!/bin/sh
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
# ADR 0055: mount each dynamic-mount slot the host attached as an extra
# read-only virtio-blk drive. VZ tags aux disks with the stable virtio block
# identifier (`dyn_<i>`), exposed by Linux at `/sys/block/vd*/serial`; prefer
# that over `/dev/vd*` enumeration order. FC/backcompat keeps the fallback
# compact order. Only read-only bundle formats are attempted (squashfs on FC,
# erofs on VZ), so the ext4 CA drive is never matched.
mount_dyn_bundle() {
    dev="$1"
    slot="$2"
    mnt="/opt/engram/dyn/$slot"
    mkdir -p "$mnt" 2>/dev/null || true
    if mount -t squashfs -o ro "$dev" "$mnt" 2>/dev/null || \
       mount -t erofs -o ro "$dev" "$mnt" 2>/dev/null; then
        return 0
    fi
    rmdir "$mnt" 2>/dev/null || true
    return 1
}

mounted_dyn_devs=""
mounted_any_dyn=0
for attempt in 1 2 3 4 5 6 7 8 9 10; do
    # VZ path: mount by the stable block identifier, not by device name order.
    for sysdev in /sys/block/vd*; do
        [ -d "$sysdev" ] || continue
        base="${sysdev##*/}"
        dev="/dev/$base"
        [ -b "$dev" ] || continue
        [ "$dev" = "/dev/vda" ] && continue  # rootfs
        ident="$(cat "$sysdev/serial" 2>/dev/null || cat "$sysdev/device/serial" 2>/dev/null || true)"
        case "$ident" in
            dyn_[0-9]|dyn_[0-9][0-9])
                slot="${ident#dyn_}"
                ;;
            *)
                continue
                ;;
        esac
        case " $mounted_dyn_devs " in
            *" $dev "*) continue ;;
        esac
        if mount_dyn_bundle "$dev" "$slot"; then
            mounted_dyn_devs="$mounted_dyn_devs $dev"
            mounted_any_dyn=1
        fi
    done

    # FC/backcompat path: compact remaining untagged devices by enumeration.
    i=0
    for dev in /dev/vd*; do
        [ -b "$dev" ] || continue
        [ "$dev" = "/dev/vda" ] && continue  # rootfs
        case " $mounted_dyn_devs " in
            *" $dev "*) continue ;;
        esac
        while [ -d "/opt/engram/dyn/$i" ]; do
            i=$((i + 1))
        done
        if mount_dyn_bundle "$dev" "$i"; then
            mounted_dyn_devs="$mounted_dyn_devs $dev"
            mounted_any_dyn=1
            i=$((i + 1))
        fi
    done

    [ "$mounted_any_dyn" = "1" ] && break
    sleep 0.05
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

/// Write the stage-1 init shim into `<rootfs>/sbin/engram-init`
/// (chmod 0755). Mirrors the layout the kernel boot args expect:
/// `init=/sbin/engram-init`.
///
/// Harness binaries used to be baked here too. They've moved
/// host-side: bundles are attached as aux RO drives and mounted by
/// this very shim, so the rootfs no longer carries them.
pub async fn inject_init(
    rootfs_dir: &Path,
    injection: &InitInjection,
) -> Result<(), std::io::Error> {
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
async fn install_file(src: &Path, dst: &Path, label: &str) -> Result<(), std::io::Error> {
    tokio::fs::copy(src, dst).await.map_err(|e| {
        std::io::Error::new(
            e.kind(),
            format!("copy {label} {} -> {}: {e}", src.display(), dst.display()),
        )
    })?;
    chmod_executable(dst).await
}

async fn chmod_executable(path: &Path) -> Result<(), std::io::Error> {
    use std::os::unix::fs::PermissionsExt;
    tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).await?;
    Ok(())
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

    /// ADR 0061/0080: the dyn-mount path must prefer VZ's stable virtio
    /// block identifier (`/sys/block/vd*/serial == dyn_<i>`) and still fall
    /// back to FC's enumeration order. It must try squashfs first (FC path)
    /// then erofs (VZ's Kata kernel has no CONFIG_SQUASHFS). The ext4 CA
    /// drive must never be matched because only read-only bundle formats are
    /// attempted.
    #[test]
    fn init_shim_dyn_mount_prefers_ids_and_tries_squashfs_then_erofs() {
        assert!(
            DEFAULT_INIT_SHIM.contains("/sys/block/vd*"),
            "VZ dyn-mount path must inspect virtio block sysfs entries",
        );
        assert!(
            DEFAULT_INIT_SHIM.contains("cat \"$sysdev/serial\""),
            "VZ dyn-mount path must read the block device identifier",
        );
        assert!(
            DEFAULT_INIT_SHIM.contains("dyn_[0-9]|dyn_[0-9][0-9]"),
            "VZ dyn-mount path must recognize stable dyn_<slot> identifiers",
        );
        assert!(
            DEFAULT_INIT_SHIM.contains("mount -t squashfs -o ro"),
            "dyn-mount loop must try squashfs first (FC path)",
        );
        assert!(
            DEFAULT_INIT_SHIM.contains("mount -t erofs -o ro"),
            "dyn-mount loop must try erofs as fallback (VZ / Kata path)",
        );
        // The dyn-mount path scans /dev/vd* and skips /dev/vda (rootfs). It
        // must only attempt read-only bundle formats (squashfs, erofs), never
        // ext4 — otherwise the CA ext4 drive on /dev/vdb would be double-mounted.
        // We verify the dyn-mount section (between "mount_dyn_bundle()" and
        // "mark bundles_mounted") contains no "mount -t ext4" invocation.
        let shim = DEFAULT_INIT_SHIM;
        let loop_start = shim
            .find("mount_dyn_bundle()")
            .expect("dyn-mount helper must be present");
        let loop_end = shim
            .find("mark bundles_mounted")
            .expect("bundles_mounted mark must be present");
        let dyn_loop_section = &shim[loop_start..loop_end];
        assert!(
            !dyn_loop_section.contains("mount -t ext4"),
            "dyn-mount loop must never attempt ext4 — that would match the CA drive",
        );
    }

    /// The default injection substitutes both placeholders and lands the
    /// shim executable at `sbin/engram-init`.
    #[tokio::test]
    async fn inject_init_writes_executable_shim_with_substitutions() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        inject_init(
            root.path(),
            &InitInjection {
                vsock_port: 1024,
                transport: Transport::Vsock,
                init_script: None,
            },
        )
        .await
        .unwrap();

        let p = root.path().join("sbin/engram-init");
        let body = std::fs::read_to_string(&p).unwrap();
        assert!(body.contains("--port 1024"), "port placeholder substituted");
        assert!(
            body.contains("export ENGRAM_TRANSPORT=vsock"),
            "transport placeholder substituted"
        );
        assert!(!body.contains("__VSOCK_PORT__") && !body.contains("__TRANSPORT__"));
        let mode = std::fs::metadata(&p).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o755, "shim must be executable");
    }
}
