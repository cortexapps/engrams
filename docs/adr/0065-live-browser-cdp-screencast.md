# ADR 0065 — Live in-guest browser via headless Chrome + CDP screencast

Status: **Proposed**

> **Builds on** the generic inbound port tunnel from **[ADR 0064](0064-live-host-ports-vanity-subdomains.md)**
> (`ProxyPort` / `PortRelayService`), the RO-bundle engine + the existing headless-browser
> tooling ([ADR 0027](0027-ro-mounted-shared-bundle-engine.md) / [ADR 0055](0055-dynamic-per-session-directory-mounts.md)),
> the shell-tunnel auth pattern (`makeGuard`), session profiles ([ADR 0053](0053-session-profiles.md)),
> and the egress proxy ([ADR 0006](0006-host-agent-egress-proxy.md)).
>
> **Relationship to the two in-flight "0064"s (read this first).** Two unmerged branches both
> claimed `0064`: *our* generic-ports stack (`docs/adr/0064-live-host-ports-vanity-subdomains.md`,
> PRs #478–#492) and a coworker's live-browser-over-VNC branch (`docs/adr/0064-in-guest-browser-vnc.md`,
> PR #498). The agreed resolution: **generic ports = 0064, live browser = 0065 (this ADR).** This ADR
> is the live-browser decision of record; it *reuses substantial pieces* of PR #498 (see
> [§7](#7-what-we-reuse-from-pr-498-vnc-and-what-we-change)) but **changes the default substrate from
> headful-Chrome-over-VNC to headless-Chrome-over-CDP-screencast**, and reframes the VNC path as a
> deferred Tier-3 escalation. PR #498's `0064-in-guest-browser-vnc.md` should be retired or folded into
> this ADR as part of landing P0 (coordination item, see [§11](#11-open-questions--risks)).

---

## TL;DR

Give a user a **live, interactive browser pane** in a session: they click "Open Browser", see the
agent's real Chrome navigating in real time, and can **take over** (click/type) themselves. The
browser runs **headless** inside the session's microVM (egress identity + isolation preserved), and
we stream it out as **CDP `Page.startScreencast` JPEG frames over the ADR-0064 port tunnel** — *not*
VNC, *not* a headful desktop. The web renders a **synthetic but real-looking browser chrome**
(tab strip, address bar, back/forward, a working "new tab" button) reconstructed from CDP
`Target.*` data, with the page painted into a `<canvas>` from the frames. Human takeover and the
agent both drive **one shared Chrome** over CDP, so the human watches the agent's actual clicks land.

Two extra pieces make the experience good:
1. **A synthetic browser UI** so the pane *looks* like Chrome with real, creatable tabs — even though
   CDP screencast only yields the page viewport, never the browser's own toolbar.
2. **Cursor/keyboard choreography** so the agent's Playwright drives — which teleport — *render* as a
   smooth human-like cursor gliding to targets and typing character-by-character.

Heavyweight substrates (headful+VNC, WebRTC/H.264, a pixel-grounding computer-use harness) are
explicitly **out of scope for v1** but the seams are designed to admit them later as escalation rungs.

---

## 1. Context — what exists, and the gap

### 1.1 What engrams runs today

engrams runs an AI coding agent inside an isolated microVM (Firecracker in prod, VZ on macOS,
Process in dev). The agent **already drives a headless browser**: the `playwright` RO-bundle
(ADR 0027/0055) ships `chromium-headless-shell` + Microsoft's `@playwright/cli` (`playwright-cli`)
and a `show-your-work` skill. The agent runs `playwright-cli open http://localhost:3000`,
`snapshot` (accessibility tree + element refs), `click e7`, `fill e12 "…"`, `screenshot`,
`video-start/-stop`, etc. `playwright-cli` keeps **one headless Chromium open across commands**.

So we already have: a headless Chrome in the guest, driven semantically by the agent, producing
screenshots as async disk artifacts (via `engram-share`). What we **don't** have: any *live* view —
no real-time stream, no cursor, no human takeover, no "watch it navigate" pane.

### 1.2 The substrate this depends on — ADR 0064

ADR 0064 built the missing primitive: a **generic, raw-byte inbound tunnel** from a guest port out
to the orchestrator/web. Key pieces this ADR consumes (do not rebuild them):

- `crates/engram-protocol/proto/host_service.proto` — `rpc ProxyPort(stream ProxyPortMessage)` with
  `ProxyPortOpen{ sandbox_id, port }` first frame, then raw `ProxyPortData` / `ProxyPortClose`.
- `crates/engram-protocol/proto/engram/app/v1/session.proto` — `PortRelayService.Relay` with
  `PortOpen{ session_id, port }`.
- `crates/engram-host-agent/src/proxy_port.rs` — `open_tcp_tunnel_at(guest_ip, port, netns)` +
  `pump_tcp_through_tunnel` (raw-byte pump; reuses `proxy_shell::connect_tcp_in_netns_linux`, which
  ADR 0064 made `pub(crate)` and port-parameterized — cold path direct, warm path `setns` into the
  per-VM netns).
- `crates/engram-coordinator/src/grpc_app/port_relay.rs` — `AppPortRelayService` (mirrors
  `shell_relay.rs`: `ensure_active` auto-resume + lease pin against idle eviction + RAII release).
- `orchestrator/src/routes/preview-proxy.ts` — HTTP reverse-proxy through the tunnel
  (`tunnelSocket`, host-header rewrite); `orchestrator/src/routes/preview-ws.ts` — WebSocket
  passthrough through the tunnel.
- The port-exposure registry (`orchestrator/src/db/port-exposures.ts`, table `port_exposure`) +
  the `makeGuard` owner/admin/share auth.

**The CDP debug endpoint is HTTP + WebSocket** (`GET :9222/json` for target discovery,
`ws://:9222/devtools/page/<id>` per target for the protocol). That is *exactly* what ADR 0064's
HTTP + WS preview proxy already tunnels. So "expose the browser's CDP" is, mechanically, "expose a
guest port through the existing relay" — minimal new tunnel code.

### 1.3 The product target (Devin-style)

A right-hand "Browser" pane showing a real Chrome with tabs the agent is driving, plus a
"Live"/takeover affordance. Observed in the wild (Devin, Browserbase live view): the page content is
a **frame stream** (CDP screencast / MJPEG-over-WebSocket), and the browser chrome (tab strip,
omnibox) is either a captured headful window *or* a synthetic UI the product draws. We choose the
synthetic-UI-over-headless-screencast path (cheapest in-guest; see §4).

---

## 2. Goals / Non-goals

**Goals (v1):**
- A live, low-latency view of the agent's headless Chrome in the web app.
- A **synthetic browser UI** that looks like Chrome: real tab strip, address bar, back/forward/reload,
  and a working **"+" new-tab** button the *user* can use.
- **Human takeover**: the user can click and type into the live page; the agent pauses during takeover.
- **The agent's Playwright actions render as smooth human-like motion** (cursor glide + per-char typing),
  not teleports.
- One **shared** Chrome instance: agent and human drive the same browser; the human sees the agent act.
- Owner/admin auth identical to the shell/preview surfaces. Opt-in per profile (most sessions never
  start a browser; no resident browser unless asked).
- Minimal sandbox footprint: **headless**, reuse the existing `chromium-headless-shell`, no Xvfb/x11vnc.

**Non-goals (v1) — designed-for but deferred (see [§8](#8-deferred-escalation-rungs)):**
- VNC / headful whole-desktop streaming (anti-bot, native apps, non-Chrome windows).
- WebRTC/H.264 smooth-video transport (video-like content; needs a separate media data plane).
- A pixel/grounding **computer-use harness** (the agent driving by screenshot→coordinates rather than
  Playwright). That is a separate model/harness decision — its own future ADR.

---

## 3. Why headless + CDP screencast (the core decision)

There are three "rungs" for streaming a browser; we pick rung 1 and design seams for 2–3.

| Rung | Substrate | In-guest cost | When |
|------|-----------|---------------|------|
| **1 (this ADR)** | headless Chrome + CDP `Page.startScreencast` (JPEG frames over WS) | **lowest** — 0 extra processes, reuses existing headless Chrome, event-driven (idle ≈ free) | the default; agentic browsing is bursty-static |
| 2 (deferred) | headful Chrome on Xvfb + x11vnc, VNC/RFB | heavy — Xvfb + WM + x11vnc + full chromium; x11vnc *polls* (idle CPU) | anti-bot, whole-desktop, native dialogs |
| 3 (deferred) | headful + GStreamer/Chrome-as-peer → H.264 → **WebRTC** | heaviest in-guest (software x264, no GPU on FC) + a new UDP/ICE/TURN data plane | smooth video-like content |

Rationale, established by analysis (recorded so the implementer doesn't relitigate):

- **CDP screencast adds zero in-guest processes.** It is a feature of the Chrome we already run; the
  JPEG encode happens inside Chrome's compositor path. VNC and GStreamer bolt a whole
  display+capture+encode stack into the VM. For density (snapshots, idle packing, the FC ~12
  aux-drive ceiling) headless screencast is decisively the cheapest.
- **Event-driven beats polling.** Screencast emits frames only on visual change; x11vnc scans the
  framebuffer on a timer (idle CPU). Most sessions are idle most of the time.
- **The transport (JPEG-over-WS vs WebRTC) is orthogonal to headless-vs-headful.** WebRTC does *not*
  fix any "headless problem" (no browser chrome, no OS cursor, anti-bot detection) — those live on
  the capture axis. WebRTC only changes wire smoothness/bandwidth, and on FC (no GPU) it *increases*
  in-guest CPU. So WebRTC is reserved strictly for genuinely video-like content, never as a default.
- **JPEG fits agentic content.** Mostly-static pages with bursts on navigate/click → frames flow on
  change, idle is free. JPEG's weakness (full-frame, no inter-frame compression → bad for continuous
  motion) doesn't bite here; that's the rung-3 case.

**Transport detail:** send **binary** JPEG frames over the WS and `createImageBitmap(blob)` → draw to
`<canvas>` — do *not* forward base64 data URLs (CDP hands you base64; decode it server-side). base64
adds ~33% size + main-thread decode cost for zero benefit. Tune `Page.startScreencast` with
`format:"jpeg"`, `quality:~70`, `maxWidth/maxHeight` to the panel size, and keep the
**screencastFrameAck loop close to the guest** (the ack-gating means our tunnel RTT would otherwise
cap the framerate).

---

## 4. Design

### 4.1 One shared headless Chrome with a CDP endpoint

The crux: **the agent and the human drive the same Chrome.** Today `playwright-cli` launches its own
Chromium over a private CDP pipe; a second client can't attach. We change the model so a single
Chromium runs with `--remote-debugging-port` (e.g. `127.0.0.1:9222`, bound so the host-agent can dial
it like ttyd), and *both* the agent (`playwright-cli` via `connectOverCDP`) and the orchestrator's
view bridge attach to it.

Two implementation options for who owns the Chromium (P0 picks one — recommend **B**):

- **A — `playwright-cli` owns it:** launch `playwright-cli` so its Chromium opens with
  `--remote-debugging-port`; the view attaches a second CDP client. Smallest change, but the browser
  only exists once the agent has run a `playwright-cli` command, and lifecycle is the CLI's.
- **B — an agentd-managed browser daemon owns it (recommended):** a small in-guest control surface
  (modeled on PR #498's `engram-agentd/src/browser.rs` `StartBrowser`, **minus** Xvfb/openbox/x11vnc)
  ensures **one** `chromium-headless-shell` is running with `--remote-debugging-port`, lazily on first
  use, with a TCP readiness probe before replying. `playwright-cli` is pointed at it via
  `connectOverCDP(ws://127.0.0.1:9222)`. This decouples the browser from the CLI (the user can "Open
  Browser" before the agent touches it) and gives one clean lifecycle/owner. Reuse PR #498's
  lazy-spawn + readiness + process-group/`killpg` teardown pattern verbatim; drop the desktop stack.

Either way: **headless, in-guest, reachable on the VM's routable IP** (the host-agent's `proxy_port`
dials `vm_internal_ip:9222` from outside the guest, exactly as `proxy_shell` reaches ttyd — bind
`0.0.0.0`/the VM IP, not `127.0.0.1`-only; see PR #498's bind-address pitfall). The browser shares the
VM's **egress proxy** (ADR 0006) and network identity — the whole reason it stays in-guest.

### 4.2 The CDP data path (reuse ADR 0064)

The Chromium debug port is **just a guest port**. Expose it through ADR 0064's `ProxyPort` /
`PortRelayService` — but as an **internal** exposure, **never** a public vanity slug:

> **Security boundary (critical).** Raw CDP is a full remote-control protocol (it can read guest files
> via `Page`, run arbitrary script, etc.). We must **not** mint the CDP port as a public preview slug
> or proxy raw CDP to the browser client. The orchestrator is the **only** CDP speaker; it exposes a
> **restricted, mediated** surface to the web (screencast frames out; a small allow-listed command set
> in — `Input.dispatch*`, `Target.*`, `Page.navigate`, `Page.goBack/Forward`, `Page.reload`,
> `Page.startScreencast/stopScreencast/screencastFrameAck`). The web client never speaks raw CDP.

So the flow is:

```
web (canvas + synthetic chrome)
  → orchestrator  GET /api/v1/sessions/:id/browser  (makeGuard owner/admin BEFORE upgrade)
  → orchestrator CDP bridge: attaches to guest CDP via ADR-0064 PortRelay (port 9222),
      runs Page.startScreencast, enumerates Target.*, mediates the allow-listed command set
  → PortRelayService.Relay (orch ↔ coord) → ProxyPort (coord ↔ host-agent)
  → host-agent proxy_port.rs dials vm_internal_ip:9222 (cold direct · warm netns)
  → chromium-headless-shell --remote-debugging-port=9222 (in-guest, headless)
```

The orchestrator↔web leg is a WebSocket carrying: **binary JPEG frames** (guest→web) + a small JSON
control channel (web→orch: input events, tab ops, navigation; orch→web: tab-list/url updates).

### 4.3 Tabs + synthetic browser chrome

CDP screencast yields **only the page viewport** — never the toolbar. So the "looks like a real
browser with real tabs" UI is **reconstructed** in the web app from CDP `Target.*`:

- **Tab list:** `Target.setDiscoverTargets({discover:true})` → live `targetCreated` /
  `targetInfoChanged` / `targetDestroyed` (each gives `targetId`, `title`, `url`, `type:"page"`).
  Render a Chrome-styled tab strip from this. (Favicons: derive from the page or the site origin.)
- **Address bar:** the active target's `url` (live from `targetInfoChanged`).
- **Back / forward / reload:** `Page.goBack` / `Page.goForward` / `Page.reload` (or
  `Page.navigateToHistoryEntry`).
- **New tab ("+"):** `Target.createTarget({url:"about:blank"})` → a new page target → screencast it.
- **Switch tab:** `Target.activateTarget(targetId)` + the orchestrator re-points the screencast
  session at that target (stop on old, start on new).
- **Close tab:** `Target.closeTarget(targetId)`.

Only the **active** target is screencast at a time (one stream); switching tabs re-attaches the
screencast. This is the lightweight path that yields the full tabbed experience without a headful
window.

### 4.4 Human takeover

The pane is bidirectional. Viewer mouse/keyboard map to CDP `Input.dispatchMouseEvent` /
`Input.dispatchKeyEvent` against the active target (the canvas maps client coords → page coords using
the screencast frame metadata's `offsetTop`/`pageScaleFactor`/device metrics). Takeover semantics:

- A "Take control" toggle. While the human is in control, **pause the agent** (the harness should not
  issue Playwright actions mid-takeover — surface a control event; reuse the ADR-0030 interrupt seam
  if the agent is mid-run). Resume on release.
- No mode switch in the protocol — agent input and human input are both `Input.dispatch*` into the
  same target; "takeover" is purely *who is currently allowed to emit* + the agent-pause.

### 4.5 Making the agent's Playwright drives look real (cursor/keyboard choreography)

The problem: Playwright **teleports** — `click(selector)` dispatches press/release at the element's
coords with no cursor travel, `fill()` sets the value instantly, and **headless Chrome paints no OS
cursor at all**. So a naive stream shows "nothing → snap → page changed." Two independent causes of
jerkiness: (a) choreography (teleport, no cursor), (b) framerate. This ADR fixes (a); rung-3 would
fix (b). Both are needed for true smoothness, but (a) alone gets ~80% of the feel at screencast fps.

**The choreography must happen in-guest** (the cursor must be rendered into the DOM so it appears in
the screencast frames; the moves must be real CDP `mouseMoved` so `:hover`/tooltips fire). Components:

1. **Synthetic cursor overlay.** Inject (via CDP `Page.addScriptToEvaluateOnNewDocument` /
   Playwright `addInitScript`, so it survives navigation) a fixed-position, high-`z-index`,
   `pointer-events:none` cursor element + a `window.__engramCursor.moveTo(x,y)` / click-ripple hook.
2. **Interpolated, human-ish motion.** Before a click, move the **real** pointer along a path of
   `Input.dispatchMouseEvent({type:"mouseMoved"})` steps (so hover/tooltips fire) while updating the
   overlay to the same coords. Generate the path with Bézier curves + slight overshoot + Fitts's-law
   timing — use **`ghost-cursor`**'s path math (built for anti-bot human-likeness; reuse the path
   generation, not the evasion intent). Then the real press/release. Optionally decouple: animate the
   cosmetic glide (~300–500ms) then fire the real (instant) click at the end.
3. **Per-character typing.** Replace instant `fill` with per-char typing + delay (Playwright
   `locator.pressSequentially(text, { delay: 40 })`); click-to-focus first so the caret is visible.
4. **Animated scroll + click ripple** for the rest of the "human" feel.

**Where the choreography lives** (P4 decision — recommend the shim): `playwright-cli` is a
third-party binary we don't control and it teleports. Recommended: an engrams **driver shim** on
PATH that exposes the same verbs the `show-your-work` skill uses, intercepts `click`/`fill`/`scroll`,
performs the choreography over a CDP session against the shared Chromium, then delegates. This avoids
forking `@playwright/cli`. (Alternative: a thin wrapper library if/when engrams owns the browser
skill directly.)

**Hide the overlay from the agent's own vision.** The agent's `playwright-cli screenshot` /
`snapshot` (and any future vision screenshots) must **not** include the cosmetic cursor. Either inject
the overlay only while a human viewer is attached, or strip the known overlay element id before agent
captures (it is `pointer-events:none`, so it never affects hit-testing).

### 4.6 Lifecycle & snapshots

The browser stack is **ephemeral and never part of a snapshot** (a resident Chrome would bloat
snapshots and violate the small-snapshot assumptions behind ADR 0028 / 0034). Reuse PR #498's
`crates/engram-host-agent/src/vnc_grace.rs` pattern (rename to `browser_grace`): a viewer connect
**cancels** any pending teardown; a viewer disconnect **schedules** `stop_browser` after a short grace
(so a page refresh reconnects to the same Chrome); a per-entry generation token keeps the timer map
from leaking across sandboxes. Belt-and-suspenders: the pre-snapshot / idle-evict path also kills the
browser. After resume, the next connect lazily respawns.

### 4.7 Auth & capability gating

- **Auth:** identical to shell/preview — `makeGuard` (owner OR admin; non-owner → 404 anti-enumeration;
  unauth → 401), gated **before** the WS upgrade. No new auth code.
- **Capability:** opt-in per profile (ADR 0053). Surface it the way PR #498 did — ride
  `ProfileSnapshot` (embedded on `TaskSessionRef`) with the profile's selected bundle/capability set,
  and derive `browserEnabled` client-side (no extra round-trip). The BROWSER tab shows iff enabled.
  Register the capability/bundle in `orchestrator/src/skills/catalog.ts` (save-time validation) +
  `web/src/hooks/useSkills.ts` (editor picker) so a profile can actually select it.

### 4.8 Local dev / VZ / Process parity

One code path, config differs. The CDP port is a guest port on every backend (FC TAP / VZ virtio-net
/ Process loopback); the host-agent already reaches the guest IP. No IAP in dev. No guest-side changes
beyond the headless-Chrome-with-debug-port + the choreography shim, both shipped in the bundle. The
Process backend has no real VM — `start_browser` can launch a local headless Chromium for dev parity,
or the feature is simply FC/VZ-gated (match PR #498's default-impl approach in the `SandboxBackend`
trait).

---

## 5. Affected components / file map

**Rust**
- `crates/engram-protocol/proto/host_service.proto` + `engram/app/v1/session.proto`: **reuse**
  ADR-0064 `ProxyPort` / `PortRelayService` for the CDP port. If a dedicated "ensure browser up"
  control RPC is wanted (option B), add `start_browser`/`stop_browser` on the host service (mirror
  PR #498's additions) — or piggyback on the relay open (the relay can ensure-browser-then-dial like
  PR #498's `proxy_vnc` does `start_browser` before dialing).
- `crates/engram-core/src/traits/{sandbox.rs,host_client.rs}`: `start_browser(id)->port` /
  `stop_browser(id)` on `SandboxBackend` + `HostClient` (default impls for Process/test). Mirror
  PR #498 exactly (it added these for VNC; same shape, headless).
- `crates/engram-sandbox-firecracker/src/lib.rs` + `crates/engram-sandbox-vz/src/backend.rs`:
  implement `start_browser`/`stop_browser` by sending an agentd `StartBrowser`/`StopBrowser` wire
  request (mirror PR #498's FC `start_browser`; **drop Xvfb/x11vnc** — just headless Chromium +
  `--remote-debugging-port`).
- `crates/engram-agentd/src/browser.rs`: lazy-spawn + readiness-probe the headless Chromium with
  `--remote-debugging-port` (adapt PR #498's `browser.rs`; remove the Xvfb→openbox→x11vnc launcher;
  the launcher just runs `chromium-headless-shell --headless=new --remote-debugging-port=$P …`).
- `crates/engram-host-agent/src/browser_grace.rs`: ephemeral teardown timers (port PR #498's
  `vnc_grace.rs`).

**Bundle**
- `deploy/bundles/browser/` (or extend `deploy/bundles/playwright/`): ensure a Chromium build that can
  run with `--remote-debugging-port` headless, plus the **choreography driver shim** + the
  `addInitScript` cursor overlay asset, packaged via the ADR-0055/0061 erofs bundle mechanism. Prefer
  reusing the `playwright` bundle's `chromium-headless-shell` over shipping a second browser. Heed
  PR #498's bundle pitfalls (dlopen NSS/sqlite closure, file perms) **if** a fuller Chromium is
  needed; headless-shell may avoid the Xvfb/xkb class entirely.

**Orchestrator (Bun/Hono)**
- `orchestrator/src/routes/browser.ts`: the `GET /api/v1/sessions/:id/browser` WS route + CDP bridge
  (attach to guest CDP via the ADR-0064 relay; `startScreencast`; mediate the allow-listed command
  set; forward binary frames + tab/url state). Model the relay/queue/backpressure helpers on
  `routes/shell.ts` / `routes/preview-ws.ts`. **Restrict the CDP surface** (§4.2).
- Capability surfacing on `ProfileSnapshot` (proto + `rpc/profiles.ts`) + `skills/catalog.ts`.
- `orchestrator/src/index.ts`: mount the route; wire the upgrade handler (see `preview-ws.ts` for the
  Bun upgrade-handler pattern).

**Web (React/Vite)**
- `web/src/components/BrowserPane.tsx`: the canvas renderer (binary JPEG → `createImageBitmap` →
  canvas), input capture → CDP input over the control channel, the takeover toggle.
- `web/src/components/BrowserChrome.tsx` (new): the **synthetic browser UI** — tab strip, omnibox,
  back/forward/reload, "+" new-tab — driven by the `Target.*` state from the control channel.
- `web/src/pages/SessionDetail.tsx`: a BROWSER tab beside TRANSCRIPT/SHELL/RAW, gated on
  `browserEnabled`, mounted-once/`display:none` across tab switches (like the shell `TerminalPane`).
- `web/src/components/ports/ExposedPortsSection.tsx` (**absorbed from ADR-0064 P2c / #483**): the
  live-host port-exposure rail (a liveness dot + an external link per exposed guest port). It lives
  **interim in the Diagnostics drawer** (ADR 0064); the BROWSER panel composes it *beside*
  `<BrowserPane/>` (the live agent tab) and **retires the drawer placement** — nothing is rebuilt, it
  relocates. The localhost bridge: an agent tab pointed at an *exposed* guest port surfaces an
  "open it yourself ↗" external link (full fidelity in the user's own browser) rather than only a
  screencast — see [ADR 0064](0064-live-host-ports-vanity-subdomains.md).

---

## 6. Phasing (one PR per phase; worktree-per-phase; stacks on the ADR-0064 branches)

> Each phase builds on ADR 0064's `ProxyPort`/`PortRelay`. Until #478–#492 merge, base the
> implementation phases on the ADR-0064 stack tip (or main once merged). Author this ADR (Proposed)
> first; update it between phases with divergences; flip to **Accepted** at the end with the commit
> chain.

- **P0 — In-guest shared headless Chrome + CDP endpoint.** `engram-agentd::browser` lazy-spawn of
  `chromium-headless-shell --remote-debugging-port` (option B) + readiness probe; `SandboxBackend` /
  `HostClient` `start_browser`/`stop_browser` (FC + VZ); `playwright-cli` repointed via
  `connectOverCDP` so agent + view share one instance; `browser_grace` teardown. *Accept:* a unit/FC
  test brings the browser up and a TCP/CDP `:9222/json` handshake succeeds through `start_browser`.
- **P1 — CDP over the tunnel.** Expose `:9222` through ADR-0064 `ProxyPort`/`PortRelay` as an
  internal exposure; prove `Target.getTargets` + a `Page.startScreencast` frame flow guest→coord→orch.
  *Accept:* an integration test pulls ≥1 screencast frame through the relay.
- **P2 — Orchestrator CDP bridge + screencast route.** `routes/browser.ts`: `makeGuard` auth,
  `startScreencast`, binary-frame forwarding, the allow-listed mediated command set, tab/url state.
  *Accept:* `bun test` — owner allowed / non-owner 404 / unauth 401; a fake relay yields a frame
  round-trip; a disallowed CDP method is rejected.
- **P3 — Web synthetic-chrome BrowserPane.** `BrowserChrome` (tabs/omnibox/nav/new-tab from
  `Target.*`) + `BrowserPane` (canvas render + input + takeover) + the gated SessionDetail tab,
  composing **ADR-0064 P2c's `ExposedPortsSection` (#483)** as the exposed-ports rail and **retiring
  its interim Diagnostics-drawer home**. *Accept:* `pnpm test`/render; clicking "+" creates a tab;
  switching tabs re-points the stream; the ports rail renders beside the live view.
- **P4 — Choreography (make Playwright look real).** Cursor-overlay `addInitScript`, Bézier
  interpolated `mouseMoved` (ghost-cursor) + overlay, `pressSequentially` typing, animated scroll,
  click ripple, agent-screenshot overlay-strip; via the driver shim. *Accept:* a test asserts a click
  emits N intermediate `mouseMoved` events + the overlay element exists; the agent's own screenshot
  excludes the overlay.
- **P5 — Capability gating, lifecycle polish, e2e + CI.** `ProfileSnapshot` capability + catalog +
  picker; grace teardown wired through the relay disconnect (port PR #498's gRPC-server grace hook);
  **FC + VZ e2e** (CDP handshake + screencast frame through the full relay) **wired into `ci.yml`'s
  `--test` list** (new FC tests must run in CI — see CLAUDE.md). *Accept:* the e2e runs in CI; the
  BROWSER tab only appears for browser-enabled profiles.

---

## 7. What we reuse from PR #498 (VNC) — and what we change

**Reuse (lift largely intact):**
- `engram-agentd/src/browser.rs` lazy-spawn + readiness-probe + process-group/`killpg` teardown.
- `engram-host-agent/src/vnc_grace.rs` → `browser_grace.rs` (cancellable ephemeral teardown w/
  generation tokens).
- The `SandboxBackend`/`HostClient` `start_browser`/`stop_browser` trait additions (FC + VZ impls).
- The "**our relay is already a websockify**" framing (orchestrator-terminates-WS + host-dials-TCP).
- Capability-via-`ProfileSnapshot` surfacing (no bespoke `/capabilities` endpoint).
- The gRPC-server grace hook (cancel-on-connect / schedule-on-disconnect) in `grpc_server.rs`.
- The bundle/erofs mechanics and its hard-won pitfalls (bind address, dlopen closure, perms).

**Change (the substrate fork):**
- **Headless `chromium-headless-shell` + `--remote-debugging-port`** instead of headful chromium +
  **Xvfb + openbox + x11vnc**. Drops the entire desktop stack → far lighter sandbox + smaller bundle.
- **CDP `Page.startScreencast` (JPEG frames)** instead of **raw RFB/VNC**. The transport rides
  ADR-0064 `ProxyPort` (generic port) rather than PR #498's `ProxyShell` `target=VNC` discriminant —
  one generic inbound primitive, not a second target enum (see ADR 0064's "subsume, don't sit
  alongside").
- **Synthetic browser chrome + CDP `Target.*` tabs** (incl. user-creatable tabs) instead of a captured
  headful window's real toolbar.
- **noVNC viewer → a `<canvas>` JPEG renderer + CDP `Input.dispatch*`** for view + takeover.
- **Shared instance**: agent (`connectOverCDP`) + human attach to one Chrome — so the human watches the
  agent's real actions. PR #498 ran a *separate* headful Chrome from the agent's headless one (the
  human did not watch the agent's browser); this ADR unifies them.

PR #498's VNC substrate is **not discarded** — it is exactly the **Tier-2/3 escalation** (§8) for
whole-desktop/anti-bot. Folding `0064-in-guest-browser-vnc.md` into this ADR (as the escalation
section) or renumbering it is a coordination item (§11).

---

## 8. Deferred escalation rungs (designed-for, not built)

The seams above keep these as additive future work:

- **Tier 2/3 — headful + VNC (PR #498's substrate).** For sites that detect headless, native
  dialogs/file pickers, or true whole-desktop. The orchestrator bridge already abstracts "a stream +
  an input channel"; swap the in-guest source (headless+CDP → headful+x11vnc) behind it. Heavier
  sandbox; gate per-profile/per-session.
- **Rung 3 — WebRTC/H.264.** For genuinely video-like content (smooth scroll/video/canvas). Needs a
  separate real-time **media data plane** (UDP/ICE/TURN) distinct from our TCP relay — a networking
  project, not a flag. Keep the view component's "frames in / input out" interface codec-agnostic so a
  WebRTC track can replace the JPEG canvas later. Reference architecture: Neko / Pion / Selkies.
- **True computer-use harness.** A model driving by screenshot→coordinates (Anthropic/OpenAI/Gemini
  computer-use tools, or open grounders like UI-TARS/OS-Atlas, or a hybrid DOM+set-of-marks). This is
  a **new harness type** (its own agentic loop), model-agnostic behind an action-space seam
  (click/type/scroll/key/screenshot). Stacks on this ADR's live view + takeover with no new plumbing.
  **Own future ADR.**

---

## 9. Alternatives considered

- **Headful + VNC as the default (PR #498).** Rejected as default: heavier sandbox (extra processes,
  full chromium, polling CPU, bigger bundle) for an experience CDP screencast delivers on headless.
  Retained as the Tier-2/3 escalation.
- **Browser outside the sandbox, at the orchestrator.** Rejected: a browser is the largest
  untrusted-content attack surface; running it in the control plane means attacker-controlled web
  content with orchestrator network access/blast radius, and it **bypasses the per-session egress
  proxy**. (A *dedicated browser sandbox* — its own isolation boundary, Browserbase-style — is a
  legitimate *future* home that decouples browser lifecycle/pooling; the ADR-0064 tunnel carries CDP
  to it unchanged. Out of scope for v1.)
- **Render outside the sandbox via DOM reconstruction (RBI-style).** The in-guest browser ships a
  sanitized DOM/paint stream; the viewer's browser re-renders. Rejected: fidelity gaps
  (canvas/WebGL/video), two divergent render trees (the agent needs the *real* rendered state), and
  reconstructing untrusted DOM in the trusted viewer is its own security project. CDP screencast gets
  ~95% of "head outside the sandbox, no desktop" without it.
- **base64 JPEG data URLs over WS** (what some products literally do, straight from CDP's base64
  output). Rejected in favor of **binary** frames + `createImageBitmap` — same architecture, drops the
  ~33% size + main-thread base64 cost.
- **A second tunnel abstraction (`target` discriminant on the shell relay, per PR #498).** Rejected:
  ADR 0064's generic `ProxyPort` (parameterized by port) subsumes it — CDP is "just port 9222."

---

## 10. Testing & CI

- **agentd** (`browser.rs`): lazy-spawn + readiness + respawn-on-exit. `engram-agentd` is
  `cfg(target_os="linux")` → vacuous under macOS nextest; run via the Linux lane + compile-check with
  `--target aarch64-unknown-linux-musl`.
- **host-agent**: relay pump + `browser_grace` (cancel-on-connect / fire-on-timeout) unit tests.
- **Firecracker e2e** (`crates/engram-host-agent/tests/` or sandbox-firecracker tests): bring up the
  headless Chrome, dial `:9222` through the full relay, assert a CDP `Target.getTargets` response +
  ≥1 `Page.startScreencast` frame survives the round trip. **Wire into `ci.yml`'s `--test` list** (not
  local-only) — and fail-loud if the browser bundle is unstaged (per PR #498's CI lesson, so the e2e
  can't silently skip). VZ parity check on macOS.
- **orchestrator** (`bun test`): `routes/browser.ts` guard (owner/non-owner/unauth), a fake-relay
  frame round-trip, and **rejection of a non-allow-listed CDP method**.
- **web** (`pnpm test`): `BrowserChrome` tab rendering from `Target.*` events; `BrowserPane` canvas
  draw from a frame; new-tab/switch-tab behavior.
- **Validation lesson (from PR #498):** asserting a transport handshake is necessary-but-not-sufficient
  — also assert the **browser is actually alive and painting** (a real screencast frame), not just that
  the port accepts. A dead Chrome can still accept a TCP connect.

## 11. Open questions / risks

- **De-risk spike (do first):** confirm one `chromium-headless-shell` with `--remote-debugging-port`
  accepts **`playwright-cli connectOverCDP` driving AND a second CDP client doing
  `Page.startScreencast` + `Input.dispatch*` on the same target simultaneously.** This is the load-
  bearing assumption; a couple-hours spike against a live session before committing P0–P2.
- **Ack-loop placement:** `screencastFrameAck` is gated; if the orchestrator acks across the full
  orch→guest RTT per frame, the tunnel RTT caps the framerate. Decide where the ack loop runs (near
  guest, or pipeline acks) — measure.
- **Choreography vs. a third-party CLI:** `playwright-cli` teleports and we don't control it. P4 must
  decide shim-wrap vs. owning the browser skill. Recommend the shim (§4.5).
- **Headless detection:** some sites fingerprint headless (`navigator.webdriver`, headless UA). For
  v1 that's an accepted limitation; it is the trigger to drop to the Tier-2 headful/VNC escalation —
  independent of this ADR's transport.
- **ADR-number coordination:** retire/fold PR #498's `0064-in-guest-browser-vnc.md` (it collides with
  our ports `0064`). Land that rename when P0 lands; coordinate with the #498 author. (Same-repo doc
  rename — not a cross-repo change.)
- **Egress policy:** enabling the browser does not change a profile's egress allowlist (ADR 0006);
  operators set the bundle and the allowlist independently (as PR #498 decided).

## 12. References

- [ADR 0064](0064-live-host-ports-vanity-subdomains.md) — generic inbound port tunnel (`ProxyPort` /
  `PortRelayService`) — **the substrate this consumes**.
- PR #498 (`adr-0064-in-guest-browser-vnc`) — the VNC live-browser approach this reuses pieces of and
  reframes as the Tier-2/3 escalation.
- [ADR 0027](0027-ro-mounted-shared-bundle-engine.md) / [ADR 0055](0055-dynamic-per-session-directory-mounts.md)
  — the RO-bundle engine + the existing `playwright` bundle (`chromium-headless-shell` + `playwright-cli`).
- [ADR 0006](0006-host-agent-egress-proxy.md) — egress proxy (the browser's network identity).
- [ADR 0053](0053-session-profiles.md) — profiles (capability gating).
- [ADR 0028](0028-eviction-durability-under-host-roll.md) / [ADR 0034](0034-idle-eviction-control-plane-and-detection.md)
  — snapshot/eviction durability (why the browser is ephemeral + out of snapshots).
- [ADR 0030](0030-session-conversation-redesign-and-operator-interrupt.md) — the interrupt seam (pause the agent during human takeover).
- `orchestrator/src/routes/shell.ts` — the WS-relay route pattern to model `routes/browser.ts` on.
- Chrome DevTools Protocol: `Page.startScreencast` / `screencastFrame` / `screencastFrameAck`,
  `Target.*`, `Input.dispatchMouseEvent` / `dispatchKeyEvent`.
- `ghost-cursor` — Bézier human-like cursor path generation (reused for the choreography).
