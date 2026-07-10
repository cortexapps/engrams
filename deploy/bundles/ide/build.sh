#!/usr/bin/env bash
# ADR 0081: build the opt-in `ide` RO bundle — the pinned code-server
# standalone release (VS Code web) + the engram-ide launcher agentd drives.
#
# The release tarball is PINNED — version + per-arch sha256 — from coder's
# GitHub release artifacts, guest-tools-style: the download is verified against
# the pinned sha before staging, so the bundle's content-address is a pure
# function of this file. Bump deliberately: update the version + BOTH shas
# (from the release page's asset digests) and smoke the IDE tab in a session.
#
# code-server's standalone release bundles its own Node, but that Node is
# glibc-DYNAMIC while session images ship arbitrary glibc (ADR 0067 / issue
# #569: the kernel always execs the loader baked into each ELF's PT_INTERP,
# which is the BASE IMAGE's own loader regardless of LD_LIBRARY_PATH — on a
# base whose glibc differs from the build baseline the binary dies before
# main). So this build reuses the browser bundle's portability treatment:
# inside a debian:bookworm-slim stage (a fixed, known glibc baseline) it
# collects the node binary's .so closure into lib/ via an ldd-walk, then
# patchelf's node's PT_INTERP + DT_RPATH to /tmp/engram-ide-bundle/lib — a
# stable symlink the launcher maintains at runtime pointing at the real
# dynamic mount slot (see bin/engram-ide's BUNDLE_LINK block).
#
# Layout produced (ADR 0055: mounted at a dynamic reserved slot
# /opt/engram/dyn/<i>; the launcher self-locates from $0):
#   code-server/   the unpacked release (lib/node patched, lib/vscode, out/, ...)
#   bin/           the engram-ide launcher + flock (patched)
#   lib/           collected .so deps + the loader (patchelf interpreter/rpath target)
#   mount.json     (activate() reads this; declares the engram-ide bin)
#
# Usage:
#   build.sh --stage <dir>        # unpacked tree at <dir> (dev/ProcessBackend)
#   build.sh <out.squashfs>       # tree, then pack to squashfs (CI/FC)
# Requires Docker; the pack path also needs mksquashfs.
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# Keep the version in lockstep with manifest.toml. Shas are the GitHub release
# assets' digests, independently re-verified by download at pin time.
CODE_SERVER_VERSION="4.127.0"
CODE_SERVER_SHA256_AMD64="2684cd3237181d837e8fe8757d98096e2d050a7d1687ee68ac39dd45a7a100d9"
CODE_SERVER_SHA256_ARM64="957755006866ed53c8bbcf22452b1522f9c26b85a68f93c593e74600817574d0"

build_tree() {
    local dest="$1"
    rm -rf "$dest"
    mkdir -p "$dest"
    # DO NOT bind-mount the OUTPUT dir (same rationale as browser/build.sh: on
    # macOS dev the Docker daemon may sit in a Lima/Colima VM over
    # reverse-sshfs, where a freshly created host dir races `docker run -v` and
    # GNU tar's deferred symlink pass fails on the sshfs mount). The container
    # builds the tree in its OWN overlayfs /out, then we `docker cp` it out —
    # host-side via the CLI, no sshfs in the path. Read-only INPUT mounts of
    # existing committed files are fine. Works identically on a native host.
    local cname=engram-bundle-build-ide
    docker rm -f "$cname" >/dev/null 2>&1 || true
    docker run --name "$cname" \
        -e HOST_UID="$(id -u)" -e HOST_GID="$(id -g)" \
        -e CODE_SERVER_VERSION="$CODE_SERVER_VERSION" \
        -e CODE_SERVER_SHA256_AMD64="$CODE_SERVER_SHA256_AMD64" \
        -e CODE_SERVER_SHA256_ARM64="$CODE_SERVER_SHA256_ARM64" \
        -v "$here/bin/engram-ide:/launcher:ro" \
        debian:bookworm-slim bash -euo pipefail -c '
        # [NB: single-quoted docker -c block — NO raw apostrophes anywhere,
        # a raw quote silently truncates the whole script.]
        export DEBIAN_FRONTEND=noninteractive
        apt-get update -qq
        # curl fetches the pinned release; util-linux carries flock (the
        # launcher --ensure lock); patchelf rewrites PT_INTERP/DT_RPATH on the
        # bundled node (issue #569) so the bundle carries its own loader
        # instead of depending on the one the base image ships.
        apt-get install -y -qq --no-install-recommends \
            ca-certificates curl util-linux patchelf
        rm -rf /var/lib/apt/lists/*

        # Target arch = the build container arch (mirrors browser/build.sh:
        # FC/VZ guests match the host, and CI builds per-arch containers).
        ARCH="$(dpkg --print-architecture)"
        case "$ARCH" in
            amd64) CS_SHA="$CODE_SERVER_SHA256_AMD64" ;;
            arm64) CS_SHA="$CODE_SERVER_SHA256_ARM64" ;;
            *) echo "unsupported arch $ARCH" >&2; exit 1 ;;
        esac

        mkdir -p /out/code-server /out/bin /out/lib
        echo "==> code-server ${CODE_SERVER_VERSION} (${ARCH}) from the pinned GitHub release" >&2
        curl -fsSL --retry 3 --retry-delay 2 \
            "https://github.com/coder/code-server/releases/download/v${CODE_SERVER_VERSION}/code-server-${CODE_SERVER_VERSION}-linux-${ARCH}.tar.gz" \
            -o /tmp/code-server.tar.gz
        echo "$CS_SHA  /tmp/code-server.tar.gz" | sha256sum -c - >/dev/null \
            || { echo "FATAL: code-server tarball sha256 mismatch" >&2; exit 1; }
        tar -xzf /tmp/code-server.tar.gz -C /out/code-server --strip-components=1
        rm /tmp/code-server.tar.gz
        # Fail loud if the tarball layout ever changes: lib/node is the one
        # binary the launcher execs (both the daemon and the /healthz probe).
        [ -x /out/code-server/lib/node ] \
            || { echo "FATAL: bundled node missing from the code-server tarball" >&2; exit 1; }

        # flock (util-linux): the launcher --ensure serializes concurrent
        # bring-ups.
        flock_bin="$(command -v flock)" \
            || { echo "FATAL: flock (util-linux) not found — --ensure cannot serialize bring-ups" >&2; exit 1; }
        cp -L "$flock_bin" /out/bin/flock
        cp /launcher /out/bin/engram-ide
        chmod 0755 /out/bin/*

        # Collect every .so dep of the bundled binaries into /out/lib so the
        # bundle is base-image-agnostic (an ldd-walk of each binary). The
        # trailing || true is load-bearing under set -euo pipefail: ldd on a
        # non-dynamic input (or grep on its empty output) fails the PIPELINE,
        # which would silently abort the whole build — and the vscode tree
        # ships exactly such inputs (see the ELF-magic skip below).
        collect() {
            ldd "$1" 2>/dev/null | awk "/=>/ {print \$3} /ld-linux/ {print \$1}" \
                | grep -E "^/" | sort -u | while read -r so; do
                    cp -nL "$so" /out/lib/ 2>/dev/null || true
                done || true
        }
        collect /out/code-server/lib/node
        collect /out/bin/flock
        # The tree also carries dlopen()d native addons (*.node: node-pty,
        # spdlog, sqlite3, watcher, ...). Like the browser bundle NSS modules,
        # they are loaded by SONAME at RUNTIME, never recorded as DT_NEEDED on
        # node, so the ldd-walk above does not see their closures — walk each
        # addon too. (Most resolve inside the node closure already; deps the
        # bookworm stage itself lacks — e.g. kerberos wants libkrb5 — stay
        # absent, exactly as in the upstream standalone release, and vscode
        # treats those addons as optional.) NB: not every *.node here is a
        # Linux ELF — ms-vscode.js-debug ships win32 PE .node files — so gate
        # on the ELF magic before walking.
        find /out/code-server -name "*.node" -type f | while read -r mod; do
            magic="$(head -c4 "$mod" | od -An -tx1 | tr -d " \n")"
            [ "$magic" = "7f454c46" ] || continue
            collect "$mod"
        done

        # --- patchelf: bake the bundle loader + rpath into the bundled ------
        # executables (issue #569; see the header comment for why
        # LD_LIBRARY_PATH can not fix this). The launcher maintains a stable
        # symlink at BUNDLE_LINK pointing at wherever this bundle is actually
        # mounted (a dynamic ADR 0055 slot), so an absolute path baked in here
        # at build time still resolves at runtime regardless of mount slot.
        # --force-rpath --set-rpath: DT_RPATH, deliberately NOT DT_RUNPATH —
        # RPATH applies transitively down the whole dependency chain (matters
        # because the *.node addons above are dlopen()d at runtime, not
        # linked, so nothing downstream of them would inherit a RUNPATH set
        # only on node itself).
        arch="$(uname -m)"
        case "$arch" in
            x86_64)  LOADER=ld-linux-x86-64.so.2 ;;
            aarch64) LOADER=ld-linux-aarch64.so.1 ;;
            *) echo "FATAL: unsupported arch $arch for patchelf interpreter selection" >&2; exit 1 ;;
        esac
        # The ldd-walk should have already copied the loader itself into lib/;
        # if it is missing every patched binary below would carry a dangling
        # PT_INTERP.
        [ -e "/out/lib/$LOADER" ] \
            || { echo "FATAL: loader $LOADER missing from /out/lib — the ldd-walk should have copied it" >&2; exit 1; }
        BUNDLE_LINK=/tmp/engram-ide-bundle
        patch_elf() {
            f="$1"
            [ -n "$f" ] && [ -e "$f" ] || return 0
            [ ! -h "$f" ] || return 0
            # Skip non-ELF files gracefully (the engram-ide launcher is a
            # shell script in bin/ too).
            magic="$(head -c4 "$f" | od -An -tx1 | tr -d " \n")"
            [ "$magic" = "7f454c46" ] || return 0
            patchelf --set-interpreter "$BUNDLE_LINK/lib/$LOADER" \
                --force-rpath --set-rpath "$BUNDLE_LINK/lib" "$f"
        }
        patch_elf /out/code-server/lib/node
        for f in /out/bin/*; do
            patch_elf "$f"
        done
        # NB: rg (vscode search) is a STATIC musl build in the release tarball
        # — no PT_INTERP to patch, runs anywhere. Other stray ELF helpers in
        # node_modules are never spawned on our path; only node is exec()d.

        # Shared objects carrying their OWN DT_RUNPATH ignore the inherited
        # executable DT_RPATH for their own dependency lookups (glibc rule —
        # the browser bundle hit exactly this with libpulse). Rewrite any such
        # collected lib to a bundle-lib RPATH; libs with no RUNPATH at all
        # stay untouched (the executable DT_RPATH covers them).
        for so in /out/lib/*; do
            [ -f "$so" ] || continue
            magic="$(head -c4 "$so" | od -An -tx1 | tr -d " \n")"
            [ "$magic" = "7f454c46" ] || continue
            existing="$(patchelf --print-rpath "$so" 2>/dev/null || true)"
            [ -n "$existing" ] || continue
            patchelf --force-rpath --set-rpath "$BUNDLE_LINK/lib" "$so"
        done
        # Same rule for the dlopen()d *.node addons — but APPEND rather than
        # replace: some carry meaningful \$ORIGIN entries pointing at payload
        # shipped beside them, which must keep resolving.
        find /out/code-server -name "*.node" -type f | while read -r so; do
            existing="$(patchelf --print-rpath "$so" 2>/dev/null || true)"
            [ -n "$existing" ] || continue
            patchelf --force-rpath --set-rpath "${existing}:$BUNDLE_LINK/lib" "$so"
        done

        # Normalize perms: every file in the RO bundle must be world-readable.
        # `a+rX` grants read to all + keeps dirs traversable and executables
        # executable, without marking data files +x.
        chmod -R a+rX /out
        chown -R "$HOST_UID:$HOST_GID" /out
    '
    # Copy the finished tree out of the container onto the host (host-side via
    # the CLI — no sshfs), then drop the container.
    docker cp "$cname:/out/." "$dest/"
    docker rm -f "$cname" >/dev/null 2>&1 || true
    cp "$here/mount.json" "$dest/"
    # Machine-level VS Code settings the launcher seeds into the user-data dir
    # at bring-up (the RO mount can't be code-server's live config home).
    mkdir -p "$dest/config"
    cp "$here/config/machine-settings.json" "$dest/config/"
    # Fail loud if the tree never reached the host — assert the launcher AND
    # the patched node landed so a broken build errors here instead of
    # shipping a silently empty bundle that still packs + stamps.
    [ -x "$dest/bin/engram-ide" ] && [ -x "$dest/code-server/lib/node" ] || {
        echo "FATAL: $dest missing bin/engram-ide or code-server/lib/node after build — the container tree did not reach the host." >&2
        exit 1
    }
}

if [[ "${1:-}" == "--stage" ]]; then
    dest="${2:?usage: build.sh --stage <dir>}"
    build_tree "$dest"
    echo "staged ide bundle tree -> $dest"
    exit 0
fi

out="${1:?usage: build.sh <out.squashfs> | --stage <dir>}"
command -v mksquashfs >/dev/null || { echo "mksquashfs not found (install squashfs-tools)" >&2; exit 1; }
tmp="$(mktemp -d)"; trap 'rm -rf "$tmp"' EXIT
build_tree "$tmp"

# Reproducible content-addressed pack (ADR 0027/0035): identical content MUST
# yield an identical sha across builds — see deploy/bundles/_pack.sh.
# shellcheck source=../_pack.sh
. "$here/../_pack.sh"
pack_squashfs "$tmp" "$out"

echo "built $out"
echo "sha256: $(sha256sum "$out" | cut -d' ' -f1)"
