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
| `x11vnc` | RFB server bound to **`127.0.0.1:5900`** (`-localhost`), `-forever -shared`, `-randr` for resize |
| `engram-browser` (launcher script) | brings up `Xvfb → openbox → x11vnc → chromium` in order; position-independent (self-locates from `$0` like the playwright wrappers), surfaced on `PATH` via `mount.json` |

Chrome runs with its **normal full UI** (address bar + tabs — deliberately *not* `--kiosk`, since the
human needs to navigate), sized to the framebuffer, with first-run/default-browser prompts
suppressed and `--user-data-dir` under the writable tmpfs (`/tmp`); renderer shared memory uses the
already-mounted `/dev/shm`. With no desktop, file manager, or terminal present and Chrome the only
client, the VNC framebuffer shows **only the browser** — "nothing else" by construction.

The bundle is mounted only on sessions whose **profile enables it** (ADR 0053), via the same
`session_env`-gated activation the `skills`/`playwright` bundles use. A session without the bundle
has no `engram-browser` on `PATH` and no `BROWSER` tab.

### 2. Lazy spawn — `engram-agentd` `StartBrowser`

A new `WireRequest::StartBrowser` variant, modeled 1:1 on `WireRequest::StartShell`
(`crates/engram-agentd/src/shell.rs`):

- **Lazy:** nothing browser-related runs at boot. The launcher is exec'd on the first `StartBrowser`,
  and the call blocks until `:5900` accepts a TCP connection (the same readiness-probe +
  spawn-timeout logic ttyd uses), so the relay only dials a port that's provably bound.
- **Idempotent / respawn:** subsequent calls re-probe and respawn only if the prior process exited,
  guarded by a process-wide `tokio::Mutex` so concurrent calls serialize on the spawn decision
  (mirrors the ttyd mutex — one browser stack per VM).
- **No console blocking:** the launcher's stdout/stderr redirect to `/var/log/engram/browser.log`,
  not `/dev/console` (the same hazard the harness and ttyd paths already avoid).

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
  `SHELL` (dial ttyd `:7681`, today) or a **raw-TCP client** for `VNC` (dial `127.0.0.1:5900`, new).
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
spawn). noVNC remote-resize is enabled (`x11vnc -randr` + Xvfb RANDR) so Chrome's viewport tracks the
panel size.

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

- **Reachability:** `x11vnc` binds `127.0.0.1` only, so the VNC server is unreachable except through
  the auth-gated relay. No protocol-layer VNC password is needed (same posture as ttyd, which has no
  VNC/SSH-layer auth — the gate is the orchestrator).
- **Egress:** the human's browsing flows through the per-session **egress proxy** (ADR 0006, MITM,
  policy-controlled), so it's bound by the **same domain allowlist as the agent**. Profiles that
  enable the browser will typically pair it with a permissive egress policy; this ADR does not change
  egress behavior.
- **"Nothing else":** the display contains only Chrome — no shell, file manager, or desktop. A user
  *could* type `file://` into Chrome to read guest files, but that is no broader than the `SHELL` tab
  the same owner already has. Access is restricted to the session owner (or admin) by the existing
  guard.

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
- **Resolution tracks the panel via RANDR** (`x11vnc -randr` + Xvfb RANDR + noVNC remote-resize)
  rather than a fixed framebuffer.

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
