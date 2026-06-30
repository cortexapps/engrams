#!/usr/bin/env bash
# ADR 0064: build the opt-in `browser` RO bundle.
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
        debian:bookworm-slim bash -euo pipefail -c '
        export DEBIAN_FRONTEND=noninteractive
        apt-get update -qq
        apt-get install -y -qq --no-install-recommends \
            chromium xvfb x11vnc openbox fonts-liberation ca-certificates \
            x11-xkb-utils xkb-data
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
        # Fail loud (like the playwright bundle) if the real ELF is missing —
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
        cp /launcher /out/bin/engram-browser
        chmod 0755 /out/bin/*

        # Collect every .so dep of the binaries into /out/lib so the bundle is
        # base-image-agnostic (same ldd-walk the playwright bundle uses).
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
