# ADR 0064 — In-guest browser sharing (Xvfb + VNC)

Status: **Proposed**

> Builds directly on the shell-tunnel work (ADR 0014 issue #6, `ProxyShell`), the dynamic
> per-session mount mechanism ([ADR 0055](0055-dynamic-per-session-directory-mounts.md)) and the
> erofs bundle packaging from [ADR 0061](0061-vz-builtin-skills-erofs.md). Interacts with the
> egress proxy ([ADR 0006](0006-host-agent-egress-proxy.md)), session profiles
> ([ADR 0053](0053-session-profiles.md)) and eviction durability
> ([ADR 0028](0028-eviction-durability-under-host-roll.md) /
> [ADR 0034](0034-idle-eviction-state-machine.md)).
> Numbered 0064 by request (0063 reserved by an in-flight branch).

## TL;DR

We want a user to click **"Launch Browser"** in a session and get a real, interactive Chrome —
running *inside that session's microVM* — rendered live in the engrams UI. The human drives it
(clicks, types, navigates); the agent is not involved. The VM exposes **only** the browser window,
nothing else.

The pleasant surprise: we have almost all the plumbing already. The **shell tab** (ADR 0014 issue
\#6) tunnels an in-guest WebSocket server (`ttyd`) out to the browser through a gRPC bidi relay with
an auth gate. A browser-over-VNC feed is the *same shape* — so the bulk of this ADR is (a) a new
opt-in **`browser` bundle** that ships `Xvfb` + `chromium` + `x11vnc` into the guest, (b) a small
**`StartBrowser`** lazy-spawn in `engram-agentd` modeled on the existing `StartShell`, and (c)
**generalizing the shell relay to carry any guest byte-stream** so it can also pump raw VNC. The
browser viewer is `noVNC` in a new `BROWSER` tab.

---

## The problem, as a story

A user is reviewing a session and wants to *see something in a browser* — log into a staging app,
poke at a deployed preview, reproduce a bug a human has to click through. Today their only window
into the VM is the transcript and a shell. There's no way to drive a GUI browser that lives inside
the isolated environment, with the VM's egress policy and network identity.

We want: a button that brings up Chrome inside the VM and streams its screen — mouse and keyboard
included — into the dashboard. And we want it **optional**: most sessions never need it, the base
image shouldn't carry a few hundred MB of browser for everyone, and the browser process shouldn't be
running (or sitting in snapshots) unless someone actually asked for it.

## Why this is mostly already built

The shell tab already solves the hard part — getting an interactive, bidirectional byte stream from
*inside* a per-VM network namespace (which coordinator/orchestrator pods have no route to) out to a
browser, with auth and ownership enforced. The chain is:

```
web (xterm/WebSocket)
  → orchestrator  GET /api/v1/sessions/:id/shell   (auth + ownership guard BEFORE upgrade)
  → ShellRelayService.Relay (orchestrator ↔ coordinator bidi, app session.proto)
  → ProxyShell             (coordinator ↔ host-agent bidi, host_service.proto)
  → host-agent proxy_shell.rs  dials ttyd  (cold: 127.0.0.1:7681 · warm: via netns)
  → ttyd (in-guest WebSocket server, lazily spawned by agentd WireRequest::StartShell)
```

The only reason this is shell-specific is that **`ttyd` happens to speak WebSocket natively**, so the
host-agent dials it as a WebSocket client. A VNC server (`x11vnc`) speaks **raw RFB over TCP** instead.

### The key realization: our relay is already a websockify

`noVNC` is a VNC client in JavaScript. Browsers can't open raw TCP sockets, and VNC is a raw-TCP
protocol, so a normal noVNC deployment needs **websockify** — a shim that bridges a browser
WebSocket to a raw-TCP VNC server. But look at the chain above: the orchestrator end *already*
terminates a browser WebSocket, and the host-agent end *already* dials a guest socket. **That chain
is structurally a websockify.** If the host-agent dials the VNC server over raw TCP and ships the
bytes through the relay as binary frames, websockify's job is absorbed by infrastructure we already
have — no extra process in the guest, no double WebSocket framing. (Approach A below; the rejected
alternatives B/C put websockify back in the guest.)

---

## Design

### 1. The `browser` bundle (opt-in)

A new dynamic bundle at `deploy/bundles/browser/`, built and packaged exactly like the existing
`playwright` bundle (ADR 0055) — a read-only **erofs** drive built with `mkfs.erofs -b 4096` (the
macOS-host-16K / guest-4K block-size gotcha from ADR 0061), mounted per-session at
`/opt/engram/dyn/<i>` and activated via `mount.json`. Contents:

| Component | Role |
|-----------|------|
| `Xvfb` | virtual X display `:99` (e.g. `1280x800x24`) — no physical device needed |
| `chromium` (**full**, with UI) | the browser the human drives — *not* the headless-shell the playwright bundle ships |
| `openbox` (minimal WM, no panel/menu) | window focus + auto-maximize so Chrome fills the framebuffer; nothing about it is user-visible |
| `x11vnc` | RFB server bound to **`0.0.0.0:5900`** (all interfaces, like ttyd — *not* `-localhost`; see the reachability pitfall), `-forever -shared`, `-xrandr` for resize |
| `engram-browser` (launcher script) | brings up `Xvfb → openbox → chromium → x11vnc` in order (chromium under a respawn supervisor — relaunched if it exits); **drops to the unprivileged `engram-browser` uid via `setpriv` before any of them start** (see §7); position-independent (self-locates from `$0` like the playwright wrappers), surfaced on `PATH` via `mount.json` |
| `xkbcomp` + `xkb-data` (`/usr/share/X11/xkb`) | XKB keyboard stack Xvfb needs to compile a keymap at boot — see the pitfall below |
| NSS modules (`libsoftokn3`, `libfreebl3`, `libnssckbi` + `.chk`) | chromium `dlopen`s these for its cert DB/crypto — not `DT_NEEDED`, so the `ldd`-walk misses them — see the pitfall below |

**Pitfall (live testing).** The bundle is assembled by copying the binaries plus their `ldd`
shared-library closure — i.e. the **`DT_NEEDED` link closure only**. That misses every dependency
loaded another way, of which there are three classes here, each producing the *same* opaque noVNC
"connection closed unexpectedly" (the symptom is generic: x11vnc never serves a usable display, so
the relay tears down):

1. **Runtime data.** Xvfb compiles a keymap at startup and aborts hard if it can't (`Failed to
   compile keymap` / `Failed to activate virtual core keyboard`) — so the bundle must carry the
   `xkb-data` tree and the `xkbcomp` binary (neither is a `.so`). Xvfb also execs a **hard-coded**
   `/usr/bin/xkbcomp` (the launcher symlinks the bundled one there; `-xkbdir` relocates only the
   data).
2. **`dlopen`'d modules *and their own closure*.** chromium loads the NSS softoken stack
   (`libsoftokn3.so` + its `libfreebl3`/`libnssckbi` and the `.chk` integrity files) by SONAME at
   runtime. They are *not* linked deps of chrome, so the `ldd`-walk never sees them, and a minimal
   glibc base has no `libnss3`. Missing `libsoftokn3.so` makes chromium abort before it paints a
   single frame (`FATAL:crypto/nss_util.cc … libsoftokn3.so: cannot open shared object file`). One
   layer deeper: the modules carry their *own* `DT_NEEDED` closure that chrome doesn't link —
   notably **`libsqlite3.so.0`**, the backing store for softoken's `sql:` cert DB — so `build.sh`
   must `collect` (ldd-walk) the NSS modules too, not just copy them, or chromium aborts one frame
   in on `libsqlite3.so.0`.
3. **File perms.** Some chromium payload files ship `0600` (notably `libGLESv2.so`), which then
   `dlopen`s in-guest as `cannot open shared object file: Permission denied` (the bundle is consumed
   by a process that need not be the build uid). `build.sh` normalizes the tree to world-readable
   (`chmod -R a+rX`).

A fourth, unrelated trap: the x11vnc resize flag is **`-xrandr`**, not `-randr` (an unrecognized
option makes x11vnc abort before binding the RFB port). All of the above are guarded fail-loud in
`build.sh`. **Validation lesson:** asserting the **RFB banner** (`RFB 003.008`) is *necessary but not
sufficient* — x11vnc serves the banner even when chromium is dead, so the banner can pass while the
tab shows a blank/closing screen. A complete check confirms **chromium itself stays alive and
paints** (no `FATAL` in the launcher log, the chrome process group survives past startup), not just
that the port accepts. The FC `e2e_vnc` test asserts the banner; the chromium-liveness gap is why
this regressed after the xkb fix landed. **As-built mitigation:** rather than try to *detect* a dead
chromium (which agentd can't act on cheaply — x11vnc is still up, so the port still accepts and a
re-probe stays green), the launcher *supervises* chromium in a respawn loop, so closing the last tab
or a crash relaunches it within ~1 s without disturbing x11vnc or the live VNC connection. agentd's
readiness probe still requires the RFB banner (a real byte exchange, not a bare accept); chromium
liveness is held at the source by the supervisor.

**Pitfall (the bind address — the one that survives every bundle fix).** Even with a perfectly
self-contained bundle and a chromium that runs and paints, the tab still shows "connection closed
unexpectedly / no bytes over the VNC endpoint" if x11vnc binds the wrong interface. The host-agent's
`proxy_vnc` dials the VM's **routable IP** (`vm_internal_ip`, e.g. `192.168.64.2:5900`) from *outside*
the guest — identical to how `proxy_shell` reaches ttyd. ttyd binds `0.0.0.0:7681`, so the shell tab
works. x11vnc launched with **`-localhost` binds `127.0.0.1` only**, so the host's dial to
`guest_ip:5900` is **refused** — x11vnc is up and serves RFB perfectly *on loopback* (a same-guest
`curl telnet://127.0.0.1:5900` gets the banner), but the host never reaches it and zero bytes flow.
Fix: the launcher omits `-localhost` so x11vnc binds all interfaces, matching ttyd. The per-VM network
is the isolation boundary (prod FC: a per-VM netns reachable only by the host-agent; VZ dev: the same
vmnet posture ttyd already depends on), so this does not widen exposure beyond the already-accepted
shell. Diagnostic that pinpointed it: from inside the guest, `curl telnet://127.0.0.1:5900` returned
the banner while `curl telnet://<guest_ip>:5900` was refused and `curl http://<guest_ip>:7681`
(ttyd) succeeded — proving the stack was healthy and the *only* fault was the bind address. (The
`proxy_vnc.rs` module doc-comment that claimed the cold path "dials `127.0.0.1:5900`" was stale — the
caller passes `vm_internal_ip` — and is corrected in this change.)

Chrome runs with its **normal full UI** (address bar + tabs — deliberately *not* `--kiosk`, since the
human needs to navigate), sized to the framebuffer, with first-run/default-browser prompts
suppressed and `--user-data-dir` under the writable tmpfs (`/tmp`); renderer shared memory uses the
already-mounted `/dev/shm`. It runs **unprivileged with its sandbox enabled** — the launcher has
already dropped to the `engram-browser` uid and `--no-sandbox` is gone — so a page the human
navigates to can't escalate beyond a confined, secret-free process (see §7). With no desktop, file
manager, or terminal present and Chrome the only client, the VNC framebuffer shows **only the
browser** — "nothing else" by construction.

The bundle is mounted only on sessions whose **profile enables it** (ADR 0053), via the same
`session_env`-gated activation the `skills`/`playwright` bundles use. A session without the bundle
has no `engram-browser` on `PATH` and no `BROWSER` tab.

### 2. Lazy spawn — `engram-agentd` `StartBrowser`

A new `WireRequest::StartBrowser` variant, modeled 1:1 on `WireRequest::StartShell`
(`crates/engram-agentd/src/shell.rs`):

- **Lazy:** nothing browser-related runs at boot. The launcher is exec'd on the first `StartBrowser`,
  and the call blocks until x11vnc is provably serving on `:5900` (the same readiness-probe +
  spawn-timeout shape ttyd uses), so the relay only dials a port that's provably up.
- **Readiness = a real RFB banner (as-built, hardening).** The probe doesn't merely TCP-connect — it
  reads x11vnc's 12-byte RFB ProtocolVersion banner (`RFB 003.00x\n`). A bare accept is too weak: a
  wedged x11vnc, or the original loopback-bind bug, accepts the connection yet serves zero bytes —
  the exact "no messages over the endpoint" symptom — so "ready" must mean "actually speaking RFB".
- **Idempotent / respawn:** subsequent calls re-probe and respawn only if the prior stack stopped
  serving, guarded by a process-wide `tokio::Mutex` so concurrent calls serialize on the spawn
  decision (mirrors the ttyd mutex — one browser stack per VM). A respawn tears the *whole* old
  process group down with `killpg` (not just the leader via `kill_on_drop`), so it never orphans the
  launcher's backgrounded children — Xvfb, openbox, and the chromium respawn loop.
- **Drained, not console-blocked (as-built, hardening).** agentd pipes the launcher's stdout/stderr
  and drains them to tracing. (The earlier plan named a `/var/log/engram/browser.log` redirect; the
  load-bearing property is that they're *drained at all*.) chromium logs to stderr continuously and
  the whole stack inherits the launcher's fds, so an undrained 64 KiB pipe fills and blocks every
  writer — freezing the display.

A `DEFAULT_VNC_PORT = 5900` constant sits beside `DEFAULT_TTYD_PORT`.

### 3. The tunnel — generalize the relay to carry any guest stream (Approach A)

The relay is generalized at **both** seams so the byte-stream it carries is parameterized by a
**target**, defaulting to the existing shell behavior:

- **`host_service.proto` (`ProxyShell`):** add a `ProxyTarget target` to **`ProxyShellOpen`**
  (`SHELL` | `VNC`). The typed `open` message is exactly where a discriminant belongs — an earlier
  design carried a `ProxyShellKind` on every frame and was deliberately simplified into the typed
  `open` (see the comment at `host_service.proto:532`), so this restores the discriminant in the
  blessed place.
- **`session.proto` (`ShellRelayService.Relay`):** the open frame of `RelayShellRequest` gains the
  same target.
- **host-agent `proxy_shell.rs`:** the *upstream* becomes pluggable — a WebSocket client for
  `SHELL` (dial ttyd `:7681`, today) or a **raw-TCP client** for `VNC` (dial the guest's x11vnc on
  `:5900`, new).
  The frame pump (`ProxyShellMessage` ↔ upstream, cold-direct vs warm-netns) is shared; only upstream
  construction differs. RFB bytes ride the existing `binary` frame variant. For `VNC` the handler
  sends `StartBrowser` to agentd (and waits for readiness) before dialing, mirroring how it ensures
  ttyd is up before dialing `:7681`.

**Names unchanged (rename deferred):** the relay now carries any guest stream, which makes
`ShellRelayService` / `ProxyShell` (and the `ProxyShell*` / `RelayShell*` message families) slightly
misnamed. We deliberately **keep the names** for this change to avoid churn across the generated Rust
and TypeScript stubs on both sides of the wire; a rename to `GuestStream…` is a clean-up candidate
for a follow-up commit. Only the `target` field + raw-TCP upstream are functional here.

### 4. Orchestrator — `/vnc` route + capability gating

A new `GET /api/v1/sessions/:id/vnc` WebSocket route, a near-copy of `routes/shell.ts`: **the same
`makeGuard` auth + ownership gate fires before the upgrade** (owner or admin only; non-owners get
`404` for anti-enumeration), then it bridges the browser WebSocket ↔ the relay with `target = VNC`.
Binary RFB frames pass straight through — this orchestrator end is what makes the chain behave as a
websockify. No new auth code: VNC access is governed identically to shell access
(`can("read", "Session", { createdByUserId })`).

**Capability surfacing (as-built).** The browser is an optional capability gated on the session's
profile selecting the `browser` skill bundle. Rather than a bespoke REST endpoint (the early
`GET /api/v1/sessions/:id/capabilities` was removed) — the web↔orchestrator surface is all-proto —
the capability rides data the web already fetches: `ProfileSnapshot` (embedded on `TaskSessionRef`
via the `TaskService` proto) gains a `repeated string skills`, and the web derives
`browserEnabled = profile.skills.includes("browser")` client-side, from the same snapshot the
`ProfileChip` renders. No extra round-trip, single source of truth.

For a profile to *select* `browser`, it must be an offered skill: `browser` is registered as a
built-in skill in **both** `orchestrator/src/skills/catalog.ts` (`BUILTIN_SKILLS`, the save-time
validation) and `web/src/hooks/useSkills.ts` (the editor's picker). Omitting it there is what made an
"added" browser silently fail to persist (the editor never offered it / validation rejected it), so
`browserEnabled` stayed false even on a profile the operator believed had it.

### 5. Web — `BROWSER` tab + noVNC

A fourth tab beside `TRANSCRIPT` / `SHELL` / `RAW` in `SessionDetail`, shown only when
`browserEnabled`. Inside it, a `BrowserPane` component mirroring `TerminalPane`: lazy-mount on first
open and **stay mounted across tab switches** (`display:none` preserves the canvas + socket). It uses
the `@novnc/novnc` `RFB` client (pure JS, self-contained — compatible with the strict CSP), pointed
at the `/vnc` WebSocket, with a "Launch Browser" call-to-action that opens the connection (→ lazy
spawn). noVNC scales the fixed remote framebuffer to the panel (`scaleViewport`); it also *requests*
remote-resize (`resizeSession`), but Xvfb+x11vnc don't honor it in practice — the framebuffer stays
at the launcher's startup geometry — so Chrome is sized to that fixed framebuffer and aspect
differences with the panel show as letterbox bars (see the Decisions note).

### 6. Lifecycle & snapshot

The browser stack is **ephemeral and never part of a snapshot** — Chrome's resident memory would
bloat snapshots and violate the small-snapshot assumptions behind eviction durability (ADR 0028 /
0034). Concretely:

- **Spawn:** lazily, on the first `/vnc` connect.
- **Teardown:** when the viewer WebSocket closes, agentd reaps the stack after a short **grace
  window** (so a page refresh reconnects to the same Chrome rather than relaunching).
- **Idle-eviction:** an idle session has no viewer attached, so the grace timer has already reaped
  the stack before the idle threshold; as a belt-and-suspenders the pre-snapshot path also kills it.
- **Resume:** after restore, the next connect launches a fresh stack.

### 7. Egress & security

- **Reachability:** `x11vnc` binds all interfaces (`0.0.0.0:5900`), exactly like ttyd, because the
  host-agent's `proxy_vnc` dials the VM's routable IP from outside the guest — `-localhost` (loopback
  only) would make that dial unreachable (see the bind-address pitfall above). The isolation boundary
  is the **per-VM network**, not a loopback bind: in prod FC the per-VM netns is reachable only by the
  host-agent, and the relay in front of it is auth-gated. No protocol-layer VNC password is needed
  (same posture as ttyd, which has no VNC/SSH-layer auth — the gate is the orchestrator).
- **Egress:** the human's browsing flows through the per-session **egress proxy** (ADR 0006, MITM,
  policy-controlled), so it's bound by the **same domain allowlist as the agent**. Profiles that
  enable the browser will typically pair it with a permissive egress policy; this ADR does not change
  egress behavior.
- **"Nothing else":** the display contains only Chrome — no shell, file manager, or desktop. A user
  *could* type `file://` into Chrome to read guest files, but that is no broader than the `SHELL` tab
  the same owner already has. Access is restricted to the session owner (or admin) by the existing
  guard.
- **Page/renderer containment (as-built hardening).** The preceding bullets bound *who can reach* the
  browser; this bounds *what a page rendered in it can reach inside the VM*. The original bring-up ran
  the whole stack — chromium included — **as root**, with the full `session_env` (every secret) handed
  to it as environment variables. For a tab pointed at arbitrary, untrusted sites that opens two
  exfiltration paths if a page lands a renderer exploit: (1) read any file or open any socket in the
  VM as root, and (2) lift the session's tokens straight out of the browser process's
  `/proc/self/environ`. Closing both, on three axes:
  - **Unprivileged — and the image needs nothing.** The bundle's launcher drops the whole stack to an
    unprivileged uid (`engram-browser`, default `9000`, overridable via `ENGRAM_BROWSER_UID/GID`)
    **before** bringing up any of Xvfb/openbox/chromium/x11vnc. No user is baked into the guest image:
    the launcher, running as root for only its first few lines, creates the uid's `$HOME` + a transient
    `/etc/passwd` entry (the per-session rootfs is writable + ephemeral) and symlinks the bundled
    `xkbcomp` onto the X server's hard-coded `/usr/bin/xkbcomp` (the one root-only step), then re-exec's
    itself unprivileged via **`setpriv --reuid --regid --clear-groups --inh-caps=-all
    --bounding-set=-all`** — and **`setpriv` ships inside the bundle**, so enabling the `browser` skill
    carries everything it needs and the image stays generic. The drop lives in the launcher, not
    agentd, for two reasons: the workspace is `unsafe_code = "forbid"`, so agentd can neither `pre_exec`
    a `setgroups`/`setgid`/`setuid` sequence nor use the nightly-only `CommandExt::groups` (safe stable
    Rust can set uid/gid but cannot *clear supplementary groups*, which we require); and the launcher is
    the only actor that knows the bundle's mount path for the xkbcomp symlink. A renderer compromise is
    now confined to an unprivileged uid with no supplementary groups, no inheritable caps, and an empty
    capability bounding set.
  - **Chromium sandbox ON.** `--no-sandbox` is **removed** (and with it `--test-type`, which existed
    only to silence the bad-flags warning `--no-sandbox` raised). chromium uses its normal namespace
    (zygote) sandbox — running unprivileged is precisely what lets it. The guest kernel ships every
    prerequisite — `CONFIG_NAMESPACES / USER_NS / PID_NS / NET_NS`, `CONFIG_SECCOMP[_FILTER]` (verified
    in `deploy/kernel/microvm-kernel-ci-x86_64-6.1.config`) — and is a mainline build with no Debian
    `unprivileged_userns_clone=0` patch, so unprivileged user namespaces are permitted and **no setuid
    `chrome-sandbox` binary is needed**. (The VZ dev guest runs a different kernel; userns there is a
    verification step, not an assumption — production drives the design.)
  - **No secrets in the browser env.** agentd no longer passes the browser `session_env`; it passes a
    strict **allowlist** — `PATH`, locale (`LANG`/`LANGUAGE`/`LC_*`/`TZ`), and the `ENGRAM_BROWSER_*`
    knobs — and drops the rest. A denylist would be wrong: `session_env` is an opaque flat map from the
    coordinator, so any *new* secret key would leak by default. The browser needs nothing secret —
    egress is **transparent** (host iptables REDIRECTs guest tcp/443 + DNS to the proxy; there is no
    `*_PROXY` var to forward) and the egress-proxy CA is trusted at the OS level (`cacerts.rs`), both
    independent of the browser's environment.

---

## Alternatives considered

- **B — websockify in the guest.** Run websockify (`:6080 → :5900`) so the guest presents a
  WebSocket like ttyd, and add a parallel relay path. *Rejected:* duplicates the relay, ships an
  extra guest dependency (Python or a C/Go port), and wraps the bytes in WebSocket framing twice. Its
  only upside — zero new host-agent code — isn't worth a fatter guest and a redundant hop.
- **C — full noVNC server in the guest.** Run noVNC's own web server + websockify inside the VM and
  tunnel its HTTP. *Rejected:* heaviest in-guest surface, least aligned with the existing relay.
- **Bake the browser into the base image instead of a bundle.** *Rejected:* grows the base rootfs by
  hundreds of MB for every session to serve a rarely-used feature; the bundle mechanism exists for
  exactly this.
- **`--kiosk` Chrome.** *Rejected:* hides the address bar and tabs — unusable for a human who needs
  to navigate. We run full Chrome UI with no window manager chrome instead.

## Testing & CI

- **agentd `StartBrowser`** lazy-spawn + readiness + respawn-on-exit. Note `engram-agentd` is
  `cfg(target_os = "linux")`, so it is **vacuous under macOS nextest** — run via
  `just test-linux engram-agentd` and compile-check with `--target aarch64-unknown-linux-musl`.
- **host-agent raw-TCP relay** unit test against a fake TCP echo upstream, asserting bytes survive
  the `binary`-frame round trip.
- **Firecracker integration test** for the end-to-end VNC handshake through the relay — **wired into
  `ci.yml`'s `--test` list** (per CLAUDE.md, new FC tests must run in CI, not be local-only), plus a
  VZ parity check on macOS.
- **orchestrator** `/vnc` guard test (owner allowed, non-owner `404`, unauth `401`).
- **web** `BrowserPane` lint/render; **bundle** build smoke mirroring the playwright bundle.
- **Hardening (§7) gates** — an agentd unit test pins the env allowlist (secrets dropped, `PATH` /
  locale / `ENGRAM_BROWSER_*` kept). Live (FC + VZ): chromium boots **as `engram-browser`, not root**
  with the **sandbox engaged** (e.g. `chrome://sandbox` reports the namespace sandbox, no
  `--no-sandbox` fallback in the launcher log); a renderer cannot read a root-owned token file; and
  `/proc/<chrome-pid>/environ` carries **no** session secret. userns availability on the VZ guest
  kernel is verified as part of this lane.

## Rollout / phases

To be filled in as the work lands; flip to **Accepted** with the commit chain at the end. Expected
shape: (P0) bundle + agentd `StartBrowser`; (P1) relay generalization (`target` field) + host-agent
raw-TCP upstream; (P2) orchestrator `/vnc` route + capability flag; (P3) web `BROWSER` tab + noVNC;
(P4) FC/VZ integration tests + CI wiring.

## Decisions on the secondary questions

- **Egress policy stays orthogonal.** Enabling the `browser` bundle does *not* change a profile's
  egress policy; the operator sets the bundle and the egress allowlist independently.
- **Single advertised viewer.** `x11vnc -shared` technically permits multiple connections, but we do
  not advertise or design for multi-viewer; concurrent viewers are treated as undefined for now.
- **Fixed framebuffer + client-side scaling (as-built).** The original plan was for the resolution to
  track the panel via RANDR + noVNC remote-resize; that didn't survive contact. Xvfb+x11vnc do **not**
  honor noVNC's `resizeSession` (the framebuffer reads back unchanged at its startup size over a raw
  RFB handshake), and Xvfb can't grow past its `-screen` startup geometry anyway. So the framebuffer
  is **fixed** at the launcher's startup geometry (default `1440x1080`, override
  `ENGRAM_BROWSER_GEOMETRY`); chromium is sized to fill it (`--window-size`, not `--start-maximized`,
  which raced the WM) and noVNC `scaleViewport` scales it to the panel, with aspect differences shown
  as letterbox bars.

## References

- ADR 0014 issue #6 — the `ProxyShell` shell tunnel (the template this generalizes)
- [ADR 0055](0055-dynamic-per-session-directory-mounts.md) — dynamic per-session mounts (bundle mechanism)
- [ADR 0061](0061-vz-builtin-skills-erofs.md) — erofs bundle packaging (`-b 4096` gotcha)
- [ADR 0006](0006-host-agent-egress-proxy.md) — egress proxy
- [ADR 0053](0053-session-profiles.md) — session profiles
- [ADR 0028](0028-eviction-durability-under-host-roll.md) / [ADR 0034](0034-idle-eviction-state-machine.md) — eviction/snapshot durability
- `crates/engram-protocol/proto/host_service.proto` — `ProxyShell` / `ProxyShellMessage`
- `crates/engram-protocol/proto/engram/app/v1/session.proto` — `ShellRelayService.Relay`
- `crates/engram-agentd/src/shell.rs` — ttyd lazy-spawn pattern
- `orchestrator/src/routes/shell.ts` — the WebSocket relay route to copy
