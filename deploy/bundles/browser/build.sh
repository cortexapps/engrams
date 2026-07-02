#!/usr/bin/env bash
# ADR 0065: build the opt-in `browser` RO bundle.
#
# A self-contained glibc tree: chromium (full UI build), Xvfb, x11vnc, openbox,
# liberation fonts, and ALL their shared-library deps — so it runs on any glibc
# base image without the base carrying browser deps. Built inside a
# debian:bookworm-slim stage to match the glibc baseline of the standard bases.
#
# Layout produced (ADR 0055: mounted at a dynamic reserved slot
# /opt/engram/dyn/<i>; the launcher self-locates from $0):
#   chrome/      chromium binary + resources
#   bin/         Xvfb, x11vnc, openbox + the engram-browser launcher
#   lib/         collected .so deps (LD_LIBRARY_PATH target)
#   fonts/ + fonts.conf
#   mount.json   (activate() reads this; declares the engram-browser bin)
#
# Usage:
#   build.sh --stage <dir>        # unpacked tree at <dir> (dev/ProcessBackend)
#   build.sh <out.squashfs>       # tree, then pack to squashfs (CI/FC)
# Requires Docker; the pack path also needs mksquashfs.
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

build_tree() {
    local dest="$1"
    rm -rf "$dest"; mkdir -p "$dest"
    docker run --rm \
        -e HOST_UID="$(id -u)" -e HOST_GID="$(id -g)" \
        -v "$dest:/out" \
        -v "$here/bin/engram-browser:/launcher:ro" \
        -v "$here/skills:/skills-src:ro" \
        debian:bookworm-slim bash -euo pipefail -c '
        export DEBIAN_FRONTEND=noninteractive
        apt-get update -qq
        # curl + xz-utils fetch the pinned Node for playwright-cli (ADR 0065);
        # util-linux carries setpriv AND flock (the launcher --ensure lock).
        apt-get install -y -qq --no-install-recommends \
            chromium xvfb x11vnc openbox fonts-liberation ca-certificates \
            x11-xkb-utils xkb-data util-linux curl xz-utils
        rm -rf /var/lib/apt/lists/*

        mkdir -p /out/chrome /out/bin /out/lib /out/fonts

        # Chromium binary + its resource files (icudtl.dat, *.pak, locales).
        chrome_bin="$(command -v chromium)"
        # chromium is usually a wrapper; resolve the real binary dir.
        real_dir="/usr/lib/chromium"
        cp -aL "$real_dir/." /out/chrome/ 2>/dev/null || true
        # Resolve the launch target at chrome/chrome explicitly. cp -aL above
        # dereferences symlinks, so a future chromium package shipping a `chrome`
        # symlink in $real_dir would land here as a ~259 MB real-file copy (or a
        # wrapper script), silently defeating the relative-symlink fix below.
        # Drop any copied chrome first so the result is always the intended
        # symlink (or the explicit cp fallback), never an accidental copy.
        rm -f /out/chrome/chrome
        # On Debian bookworm the real ELF is named `chromium` not `chrome`;
        # prefer a relative symlink so the launcher ($here/chrome/chrome)
        # resolves to the real binary inside the bundle.
        if [ -x /out/chrome/chromium ]; then
            ln -sf chromium /out/chrome/chrome
        else
            cp -L "$(readlink -f "$chrome_bin")" /out/chrome/chrome
        fi
        # Fail loud if the real ELF is missing —
        # e.g. $real_dir moved and cp -aL silently produced an empty chrome/.
        # The symlink target / cp fallback above is the binary the launcher
        # invokes; assert it exists and is executable.
        [ -x /out/chrome/chrome ] || { echo "FATAL: chromium binary not found under $real_dir" >&2; exit 1; }

        cp -L "$(command -v Xvfb)"   /out/bin/Xvfb
        cp -L "$(command -v x11vnc)" /out/bin/x11vnc
        cp -L "$(command -v openbox)" /out/bin/openbox
        # xkbcomp: Xvfb execs it at startup to compile the keyboard map. It is
        # NOT a shared-lib dep of Xvfb (so the ldd-walk below misses it) and a
        # minimal glibc base does not ship it; the launcher symlinks it onto
        # the hard-coded /usr/bin/xkbcomp the X server invokes.
        cp -L "$(command -v xkbcomp)" /out/bin/xkbcomp
        # setpriv: the launcher drops the whole stack to an unprivileged uid
        # with this (ADR 0065 §7) before bringing chromium up, so a renderer
        # compromise in an untrusted page is confined to a non-root, capless,
        # secret-free process. Shipping it IN the bundle (rather than relying on
        # the guest image) keeps the browser skill self-contained and the image
        # generic. setpriv (util-linux) is a hard requirement, so fail loud if
        # the package layout ever stops providing it.
        setpriv_bin="$(command -v setpriv)" \
            || { echo "FATAL: setpriv (util-linux) not found — the browser can not drop privileges" >&2; exit 1; }
        cp -L "$setpriv_bin" /out/bin/setpriv
        # flock (util-linux): the launcher --ensure serializes concurrent
        # bring-ups (the human opening the tab + the agent calling playwright-cli).
        flock_bin="$(command -v flock)" \
            || { echo "FATAL: flock (util-linux) not found — --ensure cannot serialize bring-ups" >&2; exit 1; }
        cp -L "$flock_bin" /out/bin/flock
        cp /launcher /out/bin/engram-browser
        chmod 0755 /out/bin/*

        # Collect every .so dep of the binaries into /out/lib so the bundle is
        # base-image-agnostic (an ldd-walk of each binary).
        collect() {
            ldd "$1" 2>/dev/null | awk "/=>/ {print \$3} /ld-linux/ {print \$1}" \
                | grep -E "^/" | sort -u | while read -r so; do
                    cp -nL "$so" /out/lib/ 2>/dev/null || true
                done
        }
        collect /out/chrome/chrome
        collect /out/bin/Xvfb
        collect /out/bin/x11vnc
        collect /out/bin/openbox
        collect /out/bin/xkbcomp
        collect /out/bin/setpriv

        # NSS crypto modules. chromium dlopen()s libsoftokn3 (its PKCS#11
        # softoken), its libfreebl3 math backend, and the libnssckbi trust
        # store at startup for the cert DB + hashing. They are loaded by SONAME
        # at RUNTIME, never recorded as DT_NEEDED, so the ldd-walk above does
        # not see them (it only walks linked deps). On a minimal glibc base
        # with no libnss3 package they are absent and chromium aborts hard
        # ("FATAL:crypto/nss_util.cc ... libsoftokn3.so: cannot open shared
        # object file") before it paints a single frame — so x11vnc serves a
        # wedged display and the VNC tab shows only "connection closed". Copy
        # the runtime NSS set (plus the .chk integrity files softoken verifies
        # beside each .so) next to the already-collected libnss3.so. They ship
        # in libnss3, a chromium dependency, beside libsoftokn3.so.
        nss_dir="$(dirname "$(find /usr/lib /lib -name libsoftokn3.so 2>/dev/null | head -1)")"
        for f in libsoftokn3.so libfreebl3.so libfreeblpriv3.so libnssckbi.so \
                 libnssdbm3.so libsoftokn3.chk libfreebl3.chk libfreeblpriv3.chk; do
            if [ -n "$nss_dir" ] && [ -e "$nss_dir/$f" ]; then
                cp -nL "$nss_dir/$f" /out/lib/
            fi
        done
        # Fail loud (like the chromium/xkb guards): no softoken -> chromium can
        # not start, which is the entire point of this bundle.
        [ -e /out/lib/libsoftokn3.so ] \
            || { echo "FATAL: NSS softoken (libsoftokn3.so) missing — chromium aborts at startup" >&2; exit 1; }
        # The NSS modules carry their OWN shared-lib closure that chromium does
        # not link directly — notably libsqlite3.so.0, the backing store for the
        # softoken sql cert DB. The chrome ldd-walk above never sees it (a dep
        # of the dlopen-loaded module, not of chrome), so without this chromium
        # still aborts one layer in ("FATAL ... nss_util ... libsqlite3.so.0:
        # cannot open shared object file"). collect runs ldd, which emits the
        # full transitive closure, so walking the modules pulls libsqlite3 (and
        # its own deps) into the bundle.
        # NB: NO apostrophes anywhere in this docker -c block — it is one big
        # single-quoted string, so a raw apostrophe silently truncates it.
        collect /out/lib/libsoftokn3.so
        collect /out/lib/libfreebl3.so
        collect /out/lib/libnssckbi.so
        [ -e /out/lib/libsqlite3.so.0 ] \
            || { echo "FATAL: libsqlite3.so.0 missing — NSS softoken sql DB aborts chromium" >&2; exit 1; }

        # XKB keymap DATA (xkb-data). Xvfb reads rules/symbols/geometry from
        # here and feeds them to xkbcomp; these are data files, not .so deps,
        # so the ldd-walk above misses them. Without this tree Xvfb aborts at
        # boot ("Failed to compile keymap" / "Failed to activate virtual core
        # keyboard") and no X display — hence no x11vnc — ever comes up. The
        # launcher passes `-xkbdir $here/share/X11/xkb` so the relocated tree
        # is found regardless of base-image layout.
        mkdir -p /out/share/X11
        cp -a /usr/share/X11/xkb /out/share/X11/xkb
        # Fail loud (like the chromium guard) if the keyboard stack is missing
        # — a bundle without it produces the silent "browser connection closed
        # unexpectedly" the VNC tab shows.
        [ -x /out/bin/xkbcomp ] && [ -d /out/share/X11/xkb ] \
            || { echo "FATAL: xkb keyboard stack missing (xkbcomp/xkb-data)" >&2; exit 1; }
        # setpriv is what drops the stack off root (ADR 0065 §7); a bundle
        # missing it would silently run the browser as root, so fail loud.
        [ -x /out/bin/setpriv ] \
            || { echo "FATAL: setpriv missing — the browser can not drop privileges" >&2; exit 1; }

        cp /usr/share/fonts/truetype/liberation/*.ttf /out/fonts/ 2>/dev/null || true
        cat > /out/fonts.conf <<"FONTS"
<?xml version="1.0"?>
<!DOCTYPE fontconfig SYSTEM "fonts.dtd">
<fontconfig>
  <dir prefix="relative">fonts</dir>
  <cachedir>/tmp/engram-browser-fontconfig</cachedir>
  <config></config>
</fontconfig>
FONTS

        # --- playwright-cli driving the SHARED headful chrome (ADR 0065) ------
        # The browser skill is the shared browser: the human drives it over VNC
        # and the AGENT drives the SAME chromium over CDP. So this bundle also
        # ships the Microsoft playwright-cli, configured to CONNECT to the
        # headful chrome debug port (cdpEndpoint) rather than launch its own
        # headless-shell — the agent navigation then lands in the exact window
        # the human is watching. Node + the CLI are fetched as the retired
        # playwright bundle did; only the config + wrapper differ.
        # [NB: single-quoted docker -c block below — NO raw apostrophes anywhere,
        # including inside the heredocs (a raw quote still ends the outer string).]
        NODE_VERSION=20.18.1
        PLAYWRIGHT_CLI_VERSION=0.1.13
        ARCH="$(dpkg --print-architecture)"
        case "$ARCH" in
            amd64) NODE_ARCH=x64 ;;
            arm64) NODE_ARCH=arm64 ;;
            *) echo "unsupported arch $ARCH" >&2; exit 1 ;;
        esac
        mkdir -p /out/node
        curl -fsSL "https://nodejs.org/dist/v${NODE_VERSION}/node-v${NODE_VERSION}-linux-${NODE_ARCH}.tar.xz" \
            | tar -xJ -C /out/node --strip-components=1
        export PATH="/out/node/bin:$PATH"
        # @playwright/cli ONLY — no browser install. cdpEndpoint mode connects to
        # the running headful chrome, so the CLI needs no local browser of its own.
        npm install -g --no-audit --no-fund "@playwright/cli@${PLAYWRIGHT_CLI_VERSION}"
        collect /out/node/bin/node
        [ -x /out/node/bin/playwright-cli ] \
            || { echo "FATAL: playwright-cli not installed under /out/node/bin" >&2; exit 1; }

        # CLI config: CONNECT over CDP to the shared chrome on loopback :9222,
        # never launch. The wrapper points PLAYWRIGHT_MCP_CONFIG here.
        cat > /out/cli.config.json <<"CFG"
{
  "browser": {
    "cdpEndpoint": "http://127.0.0.1:9222"
  }
}
CFG

        # Wrapper (symlinked onto PATH by activation): ensure the ONE shared
        # stack is up, then exec the real CLI, which connectOverCDP-s to the
        # headful chrome the human is watching. Its own quoted heredoc — but the
        # no-apostrophe rule still applies (outer string is single-quoted).
        cat > /out/bin/playwright-cli <<"WRAP"
#!/bin/sh
here="$(cd -- "$(dirname -- "$(readlink -f -- "$0")")/.." && pwd)"
# Bring up (or no-op) the shared headful chrome + x11vnc so the agent drives the
# exact browser the human watches over VNC (ADR 0065). Best-effort: if the
# ensure fails, still try to connect (the stack may already be coming up).
"$here/bin/engram-browser" --ensure || true
# `--ensure` gates on x11vnc RFB (:5900), which comes up a beat BEFORE chromium
# finishes binding its CDP endpoint (:9222). playwright-cli connectOverCDP fails
# fast on that gap (one-shot GET /json/version -> ECONNREFUSED, no retry; ADR
# 0065: RFB readiness is necessary-but-not-sufficient — the chromium-liveness
# gap), so block until CDP actually answers before handing off. The wait lives in
# the launcher (--wait-cdp) so --ensure stays RFB-only for the human/VNC path.
# Best-effort: on timeout it returns 0 and we still exec, so the CLI surfaces the
# real error rather than us swallowing it.
"$here/bin/engram-browser" --wait-cdp || true
export LD_LIBRARY_PATH="$here/lib:${LD_LIBRARY_PATH:-}"
export PLAYWRIGHT_MCP_CONFIG="$here/cli.config.json"
export PATH="$here/node/bin:$PATH"
exec "$here/node/bin/playwright-cli" "$@"
WRAP
        chmod 0755 /out/bin/playwright-cli

        # show-your-work skill (moved here from the retired playwright bundle).
        mkdir -p /out/skills
        cp -R /skills-src/show-your-work /out/skills/

        # Normalize perms: every file in the RO bundle must be world-readable.
        # The in-guest browser process need not run as the build uid, and some
        # chromium payload files ship 0600 (notably libGLESv2.so), which then
        # dlopen()s in-guest as "cannot open shared object file: Permission
        # denied". `a+rX` grants read to all + keeps dirs traversable and
        # executables executable, without marking data files +x.
        chmod -R a+rX /out
        chown -R "$HOST_UID:$HOST_GID" /out
    '
    cp "$here/mount.json" "$dest/"
    # Fail loud if the container's /out never reached the host $dest. The
    # in-container guard above sees a populated /out, but a `docker run -v`
    # bind mount whose host path Docker can't share (classically a macOS
    # mktemp dir under /var/folders, which Docker Desktop does NOT propagate)
    # leaves the HOST tree with only the mount.json we just cp'd — a silently
    # empty bundle that still packs + stamps. Assert the launcher landed on
    # the host side so an unshared $dest errors here instead of shipping an
    # empty browser bundle (callers must stage under a Docker-shared path —
    # an absolute path under $HOME, never /var/folders).
    [ -x "$dest/bin/engram-browser" ] || {
        echo "FATAL: $dest/bin/engram-browser missing after build — the docker bind mount did not propagate to the host. Stage under a Docker-shared path (absolute, under \$HOME), not /var/folders." >&2
        exit 1
    }
}

if [[ "${1:-}" == "--stage" ]]; then
    dest="${2:?usage: build.sh --stage <dir>}"
    build_tree "$dest"
    echo "staged browser bundle tree -> $dest"
    exit 0
fi

out="${1:?usage: build.sh <out.squashfs> | --stage <dir>}"
command -v mksquashfs >/dev/null || { echo "mksquashfs not found (install squashfs-tools)" >&2; exit 1; }
tmp="$(mktemp -d)"; trap 'rm -rf "$tmp"' EXIT
build_tree "$tmp"
rm -f "$out"
mksquashfs "$tmp" "$out" -comp zstd -all-root -noappend -no-xattrs >/dev/null
echo "built $out"
echo "sha256: $(sha256sum "$out" | cut -d' ' -f1)"
