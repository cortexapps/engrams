# ADR 0067: Browser-stack reliability + a glibc-portable bundle

**Status:** Accepted (2026-07-02) — the fix train for issue #569: the Browser tab shows a
white screen (or "connection closed") on some base images, and a long-lived guest fills with
**zombie** Xvfb/openbox/chromium processes. The investigation opened on the hypothesis that
the ADR 0065 §7 privilege drop had broken **chromium's sandbox** on our guest kernel; live
verification **flipped** that — the sandbox works fine under the exact `setpriv` drop — and
the real failures were a bundle that silently dies on any glibc-skewed base image, an
unsupervised Xvfb/x11vnc pair, no PID-1 zombie reaping, a GPU-process crash loop delaying
CDP by ~70 s, and a launcher that discarded every byte of diagnostic output.

**Related:** ADR 0065 (the shared headful browser this hardens — its §Rollout narrative is
annotated where this ADR supersedes it), ADR 0027 (the bundle engine; its
"wrapper bakes `LD_LIBRARY_PATH`" pattern is retired for this bundle), ADR 0055 (dynamic
per-session mount slots — why the patched paths need a stable symlink), ADR 0025 (the owned
FC guest kernel, where the sandbox symbols are now asserted), issues #567/#568 (the
restore-rebind + force-stop-on-failed-probe groundwork the fail-fast design leans on).

## Context

Three compounding blind spots made #569 hard to even see:

1. **No logs.** `--ensure-locked` redirected the whole detached stack to `/dev/null`, so a
   startup failure (chromium sandbox init, Xvfb keymap, a loader crash) left no trace.
2. **No supervision.** Only chromium had a respawn loop; Xvfb and x11vnc ran
   fire-and-forget, so either dying left the chrome loop spinning against a dead display
   with nothing to notice, clean up, or report.
3. **No reaping.** The stack is deliberately `setsid`-detached and reparents to agentd
   (PID 1) when the short-lived `--ensure` exits; nothing ever `wait()`ed on it, so every
   exit accumulated as a zombie — hundreds in a long-lived guest.

**The proven prod root cause (live forensics on prod session `1fb1fe8b`, 2026-07-03) is
not the sandbox.** Running chromium by hand in a prod guest with stderr captured — the
thing the launcher's `/dev/null` redirect had made impossible — produced the missing
evidence. The failure plays out like this:

1. Xvfb dies mid-session (trigger still unproven; the evict/resume cycle is the prime
   suspect, cf. #567). x11vnc follows it down:
   `X connection to :99 broken (explicit kill or server shutdown).`
2. From then on, every chromium relaunch exits deliberately in under a second:
   `ERROR:ui/ozone/platform/x11/ozone_platform_x11.cc: Missing X server or $DISPLAY`
   `ERROR:ui/aura/env.cc: The platform failed to initialize.  Exiting.`
   That exit spawns crashpad first and leaves no dumps — matching every forensic detail
   #569 attributed to a sandbox failure it could never quote.
3. Chrome's only supervisor relaunches it against the dead display once per second,
   forever: white screen while x11vnc still streams, then a dead tab once it doesn't.

The sandbox itself works on prod x86_64: a captured full bring-up reached
`DevTools listening on ws://127.0.0.1:9222/...` on the first launch, with the browser at
uid 9000, `CapEff: 0000000000000000`, no `--no-sandbox`, and its renderers in different
user/pid namespaces than init.

Separately, validation on a non-bookworm rootfs exposed a **latent** portability bug: the
bundle ran on the **base image's** dynamic loader. The kernel execs the interpreter baked
into each ELF's `PT_INTERP` regardless of `LD_LIBRARY_PATH`, so on any base whose glibc
differs from the bookworm build stage (reproduced on Ubuntu 22.04, glibc 2.35) every
bundled binary died before `main` — chrome SIGBUS, Xvfb/node SIGSEGV. Prod escapes this
only because the demo image happens to be bookworm; any user image on another glibc would
hit it.

## Decision

Eight decisions, one train:

- **(a) Capture the launcher's output.** The detached stack logs to `BROWSER_LOG`
  (`/tmp/engram-browser.log`, Xvfb/openbox/x11vnc) and chrome's respawn loop to
  `CHROME_LOG` (`/tmp/engram-browser.chrome.log`), each **truncated per bring-up/launch**
  so they stay small and always hold the latest attempt.

- **(b) Event-driven supervisor subshells for Xvfb and x11vnc.** Each daemon runs under a
  subshell that blocks on it and, the instant it exits, signals the whole process group
  (`kill -TERM 0`) → the main shell's trap tears everything down. Replaces the interim
  1 s `kill -0 $PID` poll loop, which forked a `sleep` every second forever and re-derived
  liveness from a **reapable pid** (the shell reaps a dead background child within ~1 s;
  the pid is then recyclable, so the watchdog could green-light a stranger permanently).
  Fail-fast is the right recovery *because of* #568: agentd's force-stop-on-failed-RFB-probe
  means the next StartBrowser rebuilds a fresh stack rather than reattaching to a half-dead
  one. openbox stays unsupervised (cosmetic — focus/maximize only). One empirically-proven
  gotcha is load-bearing and commented in place: subshells **inherit `set -e`**, so the
  daemon line needs `|| true` or a crash exit aborts the subshell before the teardown kill
  runs.

- **(c) An init-style zombie reaper in agentd (`engram-agentd/src/reaper.rs`).** SIGCHLD- +
  10 s-tick-driven, but **never `waitpid(-1)`** — that would harvest ANY child and steal
  exit statuses out from under agentd's own `tokio::process::Child` handles (the harness
  supervisor's cached child drives the live-vs-exited **reattach decision** on teleport;
  losing its status to a stray reap breaks resume). Instead: scan `/proc` for state-`Z`
  direct children, then `waitpid(pid, WNOHANG)` only on pids **not in a tracked registry**
  of tokio-owned children. `spawn_tracked` is the choke point: it holds the registry lock
  across spawn+insert, and the scan holds the same lock across scan+reap, so the reaper can
  never observe a tokio-tracked pid as untracked.

- **(d) CDP liveness warning.** x11vnc's RFB banner (what readiness gates on) proves the
  *display* is up, not that chromium is — a dead/crash-looping chrome behind a healthy
  x11vnc was #569's visible symptom. StartBrowser on an already-up stack now runs a **1 s
  fast-path CDP probe** (`GET /json/version`, outside the start lock) and carries the
  result on the wire (`BrowserReady.cdp_warning`); a **fresh spawn** probes asynchronously
  at a 20 s budget (parity with the launcher's `--wait-cdp`) since cold-start CDP lag is
  expected. Deliberately **log-only above the host-agent** — never fails the RPC; surfacing
  it as a session event is left as future work (scope cap).

- **(e) A deliberate in-place bincode wire break on `BrowserReady`.** `cdp_warning` was
  added to the existing variant; bincode encoding is positional, so a new host fails to
  decode `BrowserReady` from an old baked agentd — surfaced as the **typed version-skew
  error**, remedy re-bake + `RefreshImage`. Accepted as a clean break (per house policy):
  the browser path on pre-#567/#569 bakes is already broken, and this fix train re-bakes
  every image anyway.

- **(f) A glibc-portable bundle via patchelf, not `LD_LIBRARY_PATH`.** build.sh rewrites
  every bundled executable's `PT_INTERP` + rpath to the bundle's own `lib/` —
  `--force-rpath` (DT_RPATH, not RUNPATH) so the path applies transitively down the dep
  chain including the dlopen'd NSS modules; libs that carry their **own** RUNPATH (which
  overrides an inherited RPATH — libpulse's pointed at a distro pulseaudio dir) get
  rewritten too. `PT_INTERP` can't be relative and ADR 0055 mount slots are per-session, so
  the baked path is the **stable symlink `/tmp/engram-browser-bundle`**, created/refreshed
  by the launcher before any bundled binary runs (and now asserted by the `playwright-cli`
  wrapper before it execs the bundled node). The global `LD_LIBRARY_PATH` export is
  **removed** — it never fixed the loader problem and actively poisoned system utilities
  (`/bin/sh`, `timeout`) on glibc-mismatched bases. Exec-via-`ld.so` (running each binary
  under the bundled loader explicitly) was rejected: chromium re-execs `/proc/self/exe` for
  its zygote/helper processes, which escapes any wrapper.

- **(g) `--disable-gpu`.** A microVM has no GPU and Xvfb has no GLX; chromium's GPU process
  can only crash-loop (ANGLE "GLX is not present") before settling on the same software
  raster path this flag selects directly — measured on a 2-vCPU FC guest the loop delayed
  CDP binding by **~70 s**.

- **(h) Kernel config assertions.** `deploy/kernel/build-fc-kernel.sh` now `require()`s
  `CONFIG_NAMESPACES`/`USER_NS`/`PID_NS`/`NET_NS`/`SECCOMP`/`SECCOMP_FILTER` so a future
  base-config re-sync cannot silently strip the symbols chromium's unprivileged namespace
  sandbox needs.

## Validation evidence

On a local Colima+KVM **aarch64** Firecracker rig:

- **Unprivileged user namespaces work on the owned guest kernel, including under the exact
  `setpriv --reuid=9000 --clear-groups --inh-caps=-all --bounding-set=-all` drop** — flipping
  the issue's opening hypothesis. Chromium 149 runs **sandboxed**: distinct user + pid
  namespaces per renderer, uid 9000, `CapEff=0`, no `--no-sandbox`.
- The glibc-skew crash was **reproduced on an Ubuntu 22.04 rootfs** (chrome SIGBUS, Xvfb/node
  SIGSEGV under the old LD_LIBRARY_PATH scheme) and fixed by the patchelf'd bundle on the
  same rootfs.
- Arch caveat: the rig is aarch64; the **x86_64 CI KVM lane remains the authority** for the
  production architecture.

## Consequences

- Requires an image **re-bake + `RefreshImage`** (agentd wire change, decision e) and a
  browser-bundle republish (launcher + build.sh); the launcher/wrapper halves alone are
  rebundle-only.
- A dying Xvfb/x11vnc now self-heals at the next StartBrowser click instead of wedging the
  session's browser forever; failures leave logs in `/tmp/engram-browser*.log`.
- The bundle runs on any glibc base image, decoupling base-image choice from the browser
  feature; the bundle's binaries are pinned to bookworm's glibc by construction.
- Guests no longer accumulate zombies; agentd carries a small always-on reaper whose one
  invariant (never touch a tokio-tracked pid) is enforced structurally via `spawn_tracked`.
