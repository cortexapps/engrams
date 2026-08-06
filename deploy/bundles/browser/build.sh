#!/usr/bin/env bash
# ADR 0065: build the opt-in `browser` RO bundle.
#
# A self-contained glibc tree: chromium (full UI build), Xvfb, x11vnc, openbox,
# liberation fonts, and ALL their shared-library deps. It runs on any glibc
# base image without the base carrying browser deps — NOT via LD_LIBRARY_PATH
# (issue #569: the kernel always execs the loader baked into each ELF's
# PT_INTERP, which is the BASE IMAGE's own loader regardless of
# LD_LIBRARY_PATH — on a base whose glibc differs from bookworm, e.g. Ubuntu
# 22.04's 2.35, every bundled binary dies before main: chrome SIGBUS,
# Xvfb/node SIGSEGV). Instead every bundled executable is patchelf'd at build
# time (see the patchelf step below) to point its own PT_INTERP + DT_RPATH at
# this bundle's own lib/, so it carries its interpreter with it. Built inside
# a debian:bookworm-slim stage so the bundled glibc/loader is a fixed, known
# baseline rather than whatever a given base image happens to ship.
#
# Layout produced (ADR 0055: mounted at a dynamic reserved slot
# /opt/engram/dyn/<i>; the launcher self-locates from $0, and maintains a
# stable /tmp symlink to it so the patchelf'd, build-time-baked absolute
# PT_INTERP/DT_RPATH paths below resolve at runtime — see bin/engram-browser):
#   chrome/      chromium binary + resources
#   bin/         Xvfb, x11vnc, openbox + the engram-browser launcher
#   lib/         collected .so deps + the loader (patchelf interpreter/rpath target)
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
    rm -rf "$dest"
    mkdir -p "$dest"
    # DO NOT bind-mount the OUTPUT dir. The Docker daemon may run in a Lima/Colima
    # VM reached over reverse-sshfs (macOS dev), where a bind-mounted /out is
    # unreliable two ways: a freshly created host dir is not yet visible in the VM
    # when `docker run -v` fires (mkdir /out/* -> "No such file or directory"),
    # and GNU tar's deferred symlink pass fails on the sshfs mount (node ships
    # npm/npx as symlinks -> "Cannot open: Permission denied"). Instead the
    # container builds the tree in its OWN overlayfs /out (mkdir + symlinks behave
    # normally), then we `docker cp` the finished tree to the host — docker cp
    # writes host-side through the CLI, with no sshfs in the path. Read-only INPUT
    # mounts of existing committed files are fine (sshfs serves existing files
    # reliably; only newly created dirs race). Works identically on a native host.
    local cname=engram-bundle-build-browser
    docker rm -f "$cname" >/dev/null 2>&1 || true
    docker run --name "$cname" \
        -e HOST_UID="$(id -u)" -e HOST_GID="$(id -g)" \
        -v "$here/bin/engram-browser:/launcher:ro" \
        -v "$here/bin/engram-chromium:/chromium-launcher:ro" \
        -v "$here/skills:/skills-src:ro" \
        debian:bookworm-slim bash -euo pipefail -c '
        export DEBIAN_FRONTEND=noninteractive
        apt-get update -qq
        # curl + xz-utils fetch pinned Node for playwright-cli (ADR 0065/0097);
        # util-linux carries setpriv AND flock (the launcher --ensure lock);
        # patchelf rewrites PT_INTERP/DT_RPATH on every bundled ELF (issue
        # #569) so the bundle carries its own loader instead of depending on
        # the one the base image ships.
        apt-get install -y -qq --no-install-recommends \
            xvfb x11vnc openbox fonts-liberation ca-certificates \
            x11-xkb-utils xkb-data util-linux curl xz-utils patchelf

        # Chromium is PINNED, installed from a snapshot.debian.org timestamp —
        # never floated from bookworm-security. An unpinned `apt-get install
        # chromium` silently rides every Debian security push into the next
        # bundle rebuild: 150.0.7871.46-1~deb12u1 (Jul 2026) crashed on
        # startup for everyone (Debian bug #1141488, SIGTRAP in the browser
        # process ~100ms in), which shipped a crash-looping chrome to every
        # prod session (black Browser tab) while CI stayed green (e2e_vnc
        # gates on the RFB banner; chrome liveness is a warning). To BUMP:
        # pick the new version + a snapshot timestamp that contains it from
        # https://snapshot.debian.org/package/chromium/ and update both
        # constants; sanity-check the new chrome actually starts in a session
        # (Browser tab shows a page, or SessionService/Exec:
        # `pgrep -f chrome/chrome && tail /tmp/engram-browser.chrome.log`).
        CHROMIUM_PIN="149.0.7827.196-1~deb12u1"
        SNAPSHOT_TS="20260630T000000Z"
        echo "deb [check-valid-until=no] https://snapshot.debian.org/archive/debian-security/$SNAPSHOT_TS bookworm-security main" \
            > /etc/apt/sources.list.d/chromium-snapshot.list
        apt-get update -qq
        apt-get install -y -qq --no-install-recommends \
            chromium="$CHROMIUM_PIN" chromium-common="$CHROMIUM_PIN"
        rm /etc/apt/sources.list.d/chromium-snapshot.list
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
        # Standalone chromium entry point: the ONLY supported way to launch the
        # bundled browser without the shared Xvfb/VNC stack (a test runner
        # handing playwright/puppeteer an executablePath). It performs the same
        # BUNDLE_LINK + FONTCONFIG_FILE setup engram-browser does — without
        # which chrome/chrome dies exit 127 on the patched PT_INTERP and
        # renders with the base image fonts. See bin/engram-chromium.
        # (No raw apostrophes in this block — see the NB above.)
        cp /chromium-launcher /out/bin/engram-chromium
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

        # --- playwright-cli driving shared chrome (ADR 0097) -----------------
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
        PLAYWRIGHT_CLI_VERSION=0.1.17
        ARCH="$(dpkg --print-architecture)"
        case "$ARCH" in
            amd64) NODE_ARCH=x64 ;;
            arm64) NODE_ARCH=arm64 ;;
            *) echo "unsupported arch $ARCH" >&2; exit 1 ;;
        esac
        # /out is the container overlayfs (not a bind mount; see build_tree),
        # so node ships its bin/npm|npx symlinks straight in with no sshfs
        # deferred-symlink failure.
        mkdir -p /out/node
        curl -fsSL "https://nodejs.org/dist/v${NODE_VERSION}/node-v${NODE_VERSION}-linux-${NODE_ARCH}.tar.xz" \
            | tar -xJ -C /out/node --strip-components=1
        export PATH="/out/node/bin:$PATH"
        # @playwright/cli plus ONLY its video encoder — no browser install.
        # cdpEndpoint mode connects to the running headful chrome, so the CLI
        # needs no local browser of its own. Video is encoded by a separate,
        # Playwright-versioned FFmpeg helper; install it at bundle-build time so
        # isolated sessions never need an artifact-CDN egress exception.
        npm install -g --no-audit --no-fund "@playwright/cli@${PLAYWRIGHT_CLI_VERSION}"
        collect /out/node/bin/node
        [ -x /out/node/bin/playwright-cli ] \
            || { echo "FATAL: playwright-cli not installed under /out/node/bin" >&2; exit 1; }
        export PLAYWRIGHT_BROWSERS_PATH=/out/playwright
        /out/node/bin/node \
            /out/node/lib/node_modules/@playwright/cli/node_modules/playwright-core/cli.js \
            install ffmpeg
        ffmpeg_bin="$(find /out/playwright -maxdepth 2 -type f -name ffmpeg-linux -perm -0100 -print -quit)"
        [ -n "$ffmpeg_bin" ] && "$ffmpeg_bin" -version >/dev/null \
            || { echo "FATAL: Playwright FFmpeg helper missing from /out/playwright" >&2; exit 1; }

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
# Fail loud if the stable symlink the patched binaries resolve through does not
# point at THIS bundle — the launcher calls above are best-effort (|| true), so
# a failed bring-up (e.g. the symlink is owned by another uid) would otherwise
# fall through to exec-ing the bundled node against a stale or absent lib tree,
# a subtle heisenbug (issue #569). $here is the same readlink-f-resolved form
# the launcher points the symlink at.
[ "$(readlink /tmp/engram-browser-bundle 2>/dev/null)" = "$here" ] || {
    echo "playwright-cli: /tmp/engram-browser-bundle does not point at this bundle — browser launcher failed above" >&2
    exit 1
}
# No LD_LIBRARY_PATH here (issue #569): node is patched at build time (see
# patchelf step above) to find lib/ via its own DT_RPATH, and the --ensure
# call above is what guarantees the /tmp stable-symlink target DT_RPATH
# resolves through actually exists before this ever runs.
export PLAYWRIGHT_MCP_CONFIG="$here/cli.config.json"
# Resolve the Playwright-versioned video helper from this read-only bundle.
# The daemon inherits this on its first invocation, so recording never consults
# a per-user cache or attempts a runtime artifact download.
export PLAYWRIGHT_BROWSERS_PATH="$here/playwright"
# The bundled version is deliberately pinned. Avoid the CLI making a best-effort
# npm registry request on every short-lived invocation (including the private
# foregrounding call below), which adds latency or noise in network-isolated
# sessions without providing a useful upgrade path.
export NO_UPDATE_NOTIFIER=1
export PATH="$here/node/bin:$PATH"
observation_dir=/tmp/engram-browser-observations
pending="$observation_dir/.pending-view"
mkdir -p "$observation_dir"
if [ -s "$pending" ]; then
    required="$(cat "$pending")"
    echo "playwright-cli: call browser_view on $required before another browser command" >&2
    exit 125
fi
filename=""
want_filename=0
for arg in "$@"; do
    if [ "$want_filename" -eq 1 ]; then
        filename="$arg"
        want_filename=0
        continue
    fi
    case "$arg" in
        --filename) want_filename=1 ;;
        --filename=*) filename="${arg#--filename=}" ;;
    esac
done
# Invoke the JavaScript entrypoint through the bundled Node explicitly. The
# npm-generated playwright-cli shim starts with `#!/usr/bin/env node`; minimal
# guest images (including the production-shaped Firecracker fixture) need not
# carry /usr/bin/env, even though this bundle already carries Node itself.
# Executing the shim directly therefore reports the misleading ENOENT
# "playwright-cli: not found". Node accepts the shim path (and ignores its
# shebang), preserving npm symlink/module resolution without depending on any
# base-image utility.
"$here/node/bin/node" "$here/node/bin/playwright-cli" "$@"
status=$?
# A CDP-connected page can remain a background Chrome target even
# after a successful navigation or interaction. Semantic commands would then
# work while VNC still showed the previously active tab, violating the shared
# headful-browser contract. Best-effort foreground the CLI session page after
# every successful command. Invoke the real entrypoint directly so this
# housekeeping action does not recurse through the wrapper or emit a second
# browser-activity event; commands without a page (close, list, etc.) simply
# fail here and retain their original successful status.
if [ "$status" -eq 0 ]; then
    "$here/node/bin/node" "$here/node/bin/playwright-cli" \
        run-code "async page => await page.bringToFront()" >/dev/null 2>&1 || true
fi
if [ "$status" -eq 0 ] && [ "${1:-}" = screenshot ] && [ -f "$filename" ]; then
    canonical="$(readlink -f -- "$filename")"
    case "$canonical" in
        "$observation_dir"/*)
            printf "%s\n" "$canonical" > "$pending"
            echo "playwright-cli: private screenshot ready at $canonical. Call browser_view with this exact path now; browser commands are blocked until it returns the pixels."
            ;;
    esac
fi
exit "$status"
WRAP
        chmod 0755 /out/bin/playwright-cli

        # One intent-aware browser skill (ADR 0097).
        mkdir -p /out/skills
        cp -R /skills-src/browser /out/skills/

        # --- patchelf: bake the bundle loader + rpath into every bundled ----
        # ELF EXECUTABLE (issue #569; see the header comment above for
        # why LD_LIBRARY_PATH alone can not fix this). The launcher
        # (bin/engram-browser) maintains a stable symlink at BUNDLE_LINK
        # pointing at wherever this bundle is actually mounted (a dynamic ADR
        # 0055 slot), so an absolute path baked in here at build time still
        # resolves at runtime regardless of mount slot.
        #
        # --set-interpreter: point PT_INTERP at OUR loader (copied into lib/
        # by the ldd-walk above) instead of the base image loader.
        # --force-rpath --set-rpath: DT_RPATH, deliberately NOT DT_RUNPATH —
        # RPATH applies transitively down the whole dependency chain (matters
        # because the NSS modules above are dlopen()d at runtime, not linked,
        # so nothing downstream of them would inherit a RUNPATH set only on
        # chrome itself).
        #
        # Only executables are patched, never the .so files collected into
        # lib/ — a shared library has no PT_INTERP/is never exec()d, so
        # patching one would be a no-op at best.
        arch="$(uname -m)"
        case "$arch" in
            x86_64)  LOADER=ld-linux-x86-64.so.2 ;;
            aarch64) LOADER=ld-linux-aarch64.so.1 ;;
            *) echo "FATAL: unsupported arch $arch for patchelf interpreter selection" >&2; exit 1 ;;
        esac
        # Fail loud (like the NSS/xkb guards above): the ldd-walk should have
        # already copied the loader itself into lib/ (ldd emits the ld-linux
        # entry alongside every other =>-resolved dep); if it is missing here
        # every patched binary below would carry a dangling PT_INTERP.
        [ -e "/out/lib/$LOADER" ] \
            || { echo "FATAL: loader $LOADER missing from /out/lib — the ldd-walk should have copied it" >&2; exit 1; }
        BUNDLE_LINK=/tmp/engram-browser-bundle
        patch_elf() {
            f="$1"
            [ -n "$f" ] && [ -e "$f" ] || return 0
            # Skip symlinks: every real executable in the sweep dirs is patched
            # as itself, and a symlink either points at one of those (already
            # covered) or at something that must NOT be patched (node ships
            # bin/npm + bin/npx as symlinks to .js scripts under lib/).
            [ ! -h "$f" ] || return 0
            # Skip non-ELF files gracefully: shell-script wrappers (the
            # engram-browser + playwright-cli launchers land in bin/ too) and
            # any wrapped packaging (the real ELF is resolved separately for
            # those, e.g. chrome/chrome below via readlink -f).
            magic="$(head -c4 "$f" | od -An -tx1 | tr -d " \n")"
            [ "$magic" = "7f454c46" ] || return 0
            # One invocation for both rewrites — patchelf rewrites the whole
            # (large) ELF per run, so merging halves the work.
            patchelf --set-interpreter "$BUNDLE_LINK/lib/$LOADER" \
                --force-rpath --set-rpath "$BUNDLE_LINK/lib" "$f"
        }
        # chromium: chrome/chrome may be the relative symlink to chromium (the
        # bookworm case) or the cp fallback real file; readlink -f always
        # lands on the actual ELF either way.
        patch_elf "$(readlink -f /out/chrome/chrome)"
        # chrome_crashpad_handler: chromium execs it out-of-process by a
        # relative path next to the main binary. The exact filename has
        # varied slightly across chromium packagings, so locate it by pattern
        # rather than hard-code it.
        crashpad="$(find /out/chrome -maxdepth 1 -iname "*crashpad_handler*" 2>/dev/null | head -1)"
        patch_elf "$crashpad"
        # Everything else executable ships in these two dirs; sweep them all
        # (patch_elf skips scripts/symlinks itself) so a newly bundled binary
        # can not drift out of the patch list and die on a glibc-skewed base.
        for f in /out/bin/* /out/node/bin/*; do
            patch_elf "$f"
        done

        # One targeted exception to "never patch the .so files": a shared
        # library carrying its OWN DT_RUNPATH. Per the glibc lookup rules an
        # object WITH a RUNPATH ignores the inherited executable DT_RPATH for
        # its own dependency lookups and searches only its RUNPATH (after
        # LD_LIBRARY_PATH, which we no longer set) — so e.g. libpulse.so.0
        # (RUNPATH pointing at the pulseaudio subdir of the distro lib dir)
        # fails to find its bundled libpulsecommon on any base that does not
        # ship pulseaudio, and chrome dies at load ("libpulsecommon-16.1.so:
        # cannot open shared object file" — proven in the Ubuntu 22.04 verify
        # run). The old global LD_LIBRARY_PATH masked exactly this. Rewrite
        # any such existing RUNPATH/RPATH to a bundle-lib RPATH; libs with no
        # RUNPATH at all stay untouched (the executable DT_RPATH covers them).
        for so in /out/lib/*; do
            [ -f "$so" ] || continue
            magic="$(head -c4 "$so" | od -An -tx1 | tr -d " \n")"
            [ "$magic" = "7f454c46" ] || continue
            existing="$(patchelf --print-rpath "$so" 2>/dev/null || true)"
            [ -n "$existing" ] || continue
            patchelf --force-rpath --set-rpath "$BUNDLE_LINK/lib" "$so"
        done

        # Normalize perms: every file in the RO bundle must be world-readable.
        # The in-guest browser process need not run as the build uid, and some
        # chromium payload files ship 0600 (notably libGLESv2.so), which then
        # dlopen()s in-guest as "cannot open shared object file: Permission
        # denied". `a+rX` grants read to all + keeps dirs traversable and
        # executables executable, without marking data files +x.
        chmod -R a+rX /out
        chown -R "$HOST_UID:$HOST_GID" /out
    '
    # Copy the finished tree out of the container onto the host (host-side via
    # the CLI — no sshfs), then drop the container.
    docker cp "$cname:/out/." "$dest/"
    docker rm -f "$cname" >/dev/null 2>&1 || true
    cp "$here/mount.json" "$dest/"
    # Fail loud if the tree never reached the host — e.g. the container built an
    # empty /out, or docker cp copied nothing. Assert the launcher landed so a
    # broken build errors here instead of shipping a silently empty bundle that
    # still packs + stamps.
    # Assert EVERY bin mount.json declares: a missing one is only a run-time
    # warning in the guest (engram-session-bundles skips it), so a silently
    # incomplete bundle would otherwise pack, stamp and ship.
    for b in engram-browser engram-chromium playwright-cli; do
        [ -x "$dest/bin/$b" ] || {
            echo "FATAL: $dest/bin/$b missing after build — the container tree did not reach the host." >&2
            exit 1
        }
    done
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
