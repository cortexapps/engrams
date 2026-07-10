# 0085. In-guest IDE (code-server) as an optional profile bundle

Status: Proposed

## Context

Sessions already expose two interactive surfaces in the web dashboard's right
panel: the **shell** (ttyd, ADR 0080 guest-tools bundle) and the **browser**
(Xvfb + x11vnc + chromium, ADR 0065/0067). Users want a third: a full IDE over
the session workspace, so they can inspect and edit files the agent is working
on without dropping to the shell.

[code-server](https://github.com/coder/code-server) is VS Code served as a web
app: a single Node server that serves its own client-side JS over plain HTTP
and speaks its own WebSocket protocol on the same port. Unlike VNC (raw RFB
that noVNC must decode client-side) it needs no client library in our web app —
an iframe pointed at a proxied origin is the whole client.

Everything this feature needs already exists as generic mechanism:

- **Optional-per-profile**: `profile.skills` → `CreateSessionRequest.selected_skills`
  → a reserved dynamic-mount slot (ADR 0055). "browser" is not a boolean flag
  anywhere; it is a catalog name in the two `BUILTIN_SKILLS` lists
  (`orchestrator/src/skills/catalog.ts`, `web/src/hooks/useSkills.ts`). The
  web gates the Browser tab off `profile.skills.includes("browser")`
  (`ProfileSnapshot.skills`), with no separate capability fetch.
- **Reachability**: the generic vsock port relay (ADR 0066). agentd listens on
  vsock port 1030, the host-agent sends `RelayConnect{target_port}`, agentd
  dials `127.0.0.1:target_port` inside the guest and splices bytes. Loopback-
  bound guest services need no routable bind. The coordinator's
  `PortRelayService` and host-agent's `ProxyPort` are payload-agnostic and
  carry HTTP/1.1, WS upgrades, and RFB alike (ADR 0064's preview proxy and
  ADR 0065's VNC both ride it today, unchanged).
- **Lazy lifecycle**: `EnsureBrowser` (coordinator RPC) → `StartBrowser`
  (host gRPC) → `WireRequest::StartBrowser` (vsock) → agentd spawns the
  bundle's launcher `--ensure` and probes readiness. `StopBrowser` runs on the
  snapshot / idle-evict path so a live browser is never snapshotted.

What does **not** exist yet is the orchestrator-tier combination code-server
needs: a **session-scoped, better-auth-guarded** proxy that carries **both**
plain HTTP **and** guest-handshake WebSockets. Today's four proxies each cover
a different quadrant:

| | HTTP | WS (guest does its own handshake) | WS (raw bytes, no guest handshake) |
|---|---|---|---|
| vanity-subdomain auth (slug+token) | `preview-proxy.ts` | `preview-ws.ts` | — |
| session-path + better-auth guard | **missing** | **missing** | `vnc.ts` |

## Decision

Add an **`ide` bundle** (code-server) selectable per profile through the
existing skills mechanism, started lazily via an **`EnsureIde`** RPC chain
mirroring `EnsureBrowser`, and reached through a new session-scoped
orchestrator proxy (`/api/v1/sessions/:id/ide/*`) that reuses the preview
proxy's HTTP and WS bridge mechanics over the existing `PortRelayService`
tunnel. The web dashboard gets an **IDE tab** in the right panel that is a
plain iframe.

### 1. Bundle (`deploy/bundles/ide/`)

- `manifest.toml` — `name = "ide"`, pinned code-server version.
- `mount.json` — `{"kind": "skill", "bins": ["bin/engram-ide"]}`; the generic
  `engram-session-bundles::activate()` symlinks the launcher onto `PATH`. No
  agent-facing skill entry — this is a human surface, like the shell.
- `build.sh` — fetch the pinned code-server release tarball (sha-pinned),
  stage it plus the launcher. code-server's standalone release bundles its own
  Node but that Node is glibc-dynamic, so the build reuses the browser
  bundle's portability treatment (ADR 0067): collect `.so` deps and patchelf
  `PT_INTERP`/rpath to a stable `/tmp/engram-ide-bundle/lib` symlink the
  launcher maintains, keeping the bundle base-image-agnostic.
- `bin/engram-ide` — launcher modeled on `engram-browser`: self-locating via
  `$0`, `--ensure` idempotent bring-up under flock, `setsid` process group
  recorded in `/tmp/engram-ide.pgid`, supervised respawn, logs to
  `/tmp/engram-ide.log`. Runs code-server with
  `--bind-addr 127.0.0.1:13337 --auth none --disable-telemetry
  --disable-update-check --disable-workspace-trust`, opening the session
  workdir. Readiness = HTTP 200 from `/healthz` on the loopback port.
- Unlike the browser (untrusted page rendering → env allowlist + uid drop,
  ADR 0065 §7), the IDE is a trusted first-party surface over the user's own
  workspace, equivalent to the shell: it gets the **full session env** and
  runs as the same user the shell runs as, so its integrated terminal behaves
  identically to the Shell tab.
- `config/machine-settings.json` — bundle-owned VS Code **machine-scope**
  settings (`chat.disableAIFeatures: true` — no Copilot/AI chat in the
  in-session IDE; the agent is the AI surface). The RO mount can't be
  code-server's live config home, so the launcher seeds it into
  `<user-data-dir>/Machine/settings.json` on every bring-up: the bundle stays
  authoritative for the machine layer, user settings are never touched.
- Wired into the `bundles`, `bundles-squashfs`, and `bundles-vz` justfile
  loops and the bundle README, distribution unchanged (content-addressed
  fleet stamp, ADR 0027/0035/0055).

### 2. Guest control (agentd)

- `WireRequest::StartIde { port: Option<u16> }` / `StopIde` and
  `WireResponse::IdeReady { port, spawned }` / `IdeStopped` — appended at the
  **end** of the enums (bincode is variant-index encoded; append-only), with
  golden wire tests.
- `crates/engram-agentd/src/ide.rs` — lifecycle module modeled on
  `browser.rs`'s hardened shape (probe-first fast path, force-stop a wedged
  pidfile stack before re-ensure per issue #567, spawn via the reaper,
  bounded readiness wait) but simpler: no CDP-style secondary probe, full
  `session_env` passthrough like `shell.rs`.
- Default port **13337** (code-server's documented example port; well away
  from common dev-server ports so it doesn't collide with user workloads or
  port exposures).

### 3. Control plane

- `SessionService.EnsureIde(session_id) → {port}` — coordinator handler
  mirrors `ensure_browser` exactly: `ensure_active` (auto-resume) →
  `resolve_sandbox` → `HostClient::start_ide`.
- `HostService.StartIde/StopIde(SandboxIdMessage)` — host-agent forwards to
  `SandboxBackend::start_ide/stop_ide` (FC + VZ send the vsock wire verbs;
  Process backend follows the same shape as its shell/browser impls).
- The snapshot / idle-evict path calls `StopIde` wherever it calls
  `StopBrowser` today, so a live code-server never lands in a snapshot (its
  listeners would resurrect wedged after restore, issue #567's lesson).
- `PortRelayService`, `ProxyPort`, and the agentd vsock relay are untouched —
  the tunnel is already generic.

### 4. Orchestrator proxy (`orchestrator/src/routes/ide.ts`)

- **HTTP**: `ALL /api/v1/sessions/:id/ide/*` — `guard` (better-auth session +
  CASL, same as shell/vnc) → `sessions.ensureIde({sessionId})` → strip the
  route prefix so code-server sees root-relative paths (its client uses
  relative asset paths behind path-rewriting proxies) → reuse
  `preview-proxy.ts`'s loopback-`net.Server` + `fetch()` mechanics over
  `tunnelSocket(portRelay, sessionId, port)`, **including the
  content-encoding/content-length strip** (#616).
- **WS**: an upgrade hook in `server.ts`'s chain recognizes
  `/api/v1/sessions/:id/ide/*` upgrades, runs the same guard, then bridges
  like `preview-ws.ts`'s `bridgeClientToGuest`: terminate the client WS,
  open a real client `WebSocket` to the loopback-bridged guest port so
  code-server performs its own handshake (unlike `vnc.ts`'s raw pump), and
  forward subprotocols. Because the bridge re-handshakes guest-side,
  code-server's Host/Origin consistency check sees matching loopback values.
- The `EnsureIde` result is cached per-connection only; every request re-runs
  the guard (no share tokens, no subdomain — this is a session-scoped,
  owner-only surface exactly like the shell).

### 5. Web

- `catalog.ts` + `useSkills.ts`: `{ name: "ide", label: "IDE", … }` in both
  `BUILTIN_SKILLS` lists (kept in sync per their own contract).
- `SessionDetail.tsx`: `ideEnabled = profile.skills.includes("ide")`, tab defs
  extended in both places (`paneTabDefs` + `WorkPane` tabs).
- `WorkPane.tsx`: new `ide` member of `PaneTabId`, `IDE_TAB`, and the
  `ideEverActive` keep-mounted guard (same as shell/browser) so the iframe —
  and code-server's websockets — survive tab switches.
- `IdePane.tsx`: an `<iframe src="${API_BASE}/sessions/${id}/ide/">` with a
  connecting/error overlay. No client library.

## Consequences

- Sessions whose profile doesn't select `ide` pay nothing: no bundle slot, no
  process, no route reachable (EnsureIde fails guest-side when the launcher
  isn't on PATH, surfacing a clear error in the tab).
- The orchestrator gains the missing "session-scoped auth × full HTTP+WS
  proxy" quadrant; future in-guest web UIs can reuse `ide.ts`'s shape.
- agentd wire enum grows two verbs (append-only, golden-tested); old baked
  agentd images simply don't know `StartIde` — same skew story as
  `StartBrowser` (stale-image sessions surface the error, remedied by rebake +
  RefreshImage).
- code-server (~100 MB unpacked) rides the content-addressed bundle store; it
  is fetched once per fleet generation and shared read-only across sessions.
- Deploy: bundle publish + fleet-stamp bake for the bundle; agentd bundle
  rebake for the new wire verbs (ADR 0080 fleet-stamp delivery); coordinator /
  host-agent / orchestrator / web roll normally.

## As-built notes (P2–P5)

Divergences from the sections above, recorded as implemented:

- **`start_ide` returns a bare port, not a struct.** `BrowserStart` exists only
  to carry the issue-#569 CDP warning; the IDE has no secondary probe, so the
  chain mirrors `start_shell`'s shape instead.
- **Workdir source.** `shell.rs` never sets a cwd (ttyd inherits agentd's).
  The only real in-guest source of the session workdir is the reserved
  `ENGRAM_HARNESS_CWD` key on the SpawnHarness frame, so `HarnessSupervisor`
  now records it and `start_ide` exports it as `ENGRAM_IDE_WORKDIR`; the
  launcher falls back to `$HOME` when absent (workdir-less images).
- **agentd wire enum indices 13/14** (StartIde/StopIde, IdeReady/IdeStopped);
  golden tests also pinned the previously-unpinned index 12
  (`RefreshAgent`/`AgentRefreshed`) since the new indices are defined
  relative to it.
- **Orchestrator WS ordering.** The guard runs pre-upgrade, but `ensureIde`
  runs *post*-upgrade (inside the upgrade callback): an auto-resume can take
  seconds, and holding a raw un-upgraded socket that long risks client
  timeouts. Failure closes 1011. Same pattern as vnc.ts's ensureBrowser.
- **`server.ts` upgrade hooks generalized** from a single `previewUpgrade`
  slot to an ordered `UpgradeHook[]` (clean break); preview stays first
  because it is Host-keyed and must win even for ide-shaped paths on a
  preview origin.
- **Policy map.** `SessionService.EnsureIde` needed an entry in the
  orchestrator's fail-closed RPC policy map (same owner-scoped `shell` action
  as `EnsureBrowser`) — P1 alone left the passthrough conformance test red.
- **`proxyHttp` gained an optional `targetPath` param** (prefix-stripped IDE
  path) instead of a copy; `bridgeClientToGuest` was exported unchanged;
  `guard.ts` mechanics were extracted to a headers-level
  `authorizeSessionAccess` so the WS upgrade hook can auth without a Hono
  context. Preview behavior unchanged.
- **Bundle build gates on ELF magic**: VS Code's bundled js-debug extension
  ships win32 PE `.node` files that broke the ldd/patchelf walk under
  `set -e`. The collector now skips non-ELF files explicitly.
- **`docker/node-assets-fetch.sh`** (the FC-host bake's bundle staging) also
  needed the `ide` entry + `current.json` stamp key — without it prod hosts
  would never stage the bundle.
- **No agent-driven auto-open for the IDE tab** (the browser auto-opens on
  `playwright-cli` activity): the IDE is a human-only surface with no agent
  signal to key off.
- Bundle pins **code-server 4.127.0** (sha256-pinned per-arch, verified);
  live-smoked on a glibc-skewed container (build = bookworm 2.36, run =
  ubuntu 22.04 / 2.35): ensure/healthz/idempotent re-ensure/respawn/killpg
  all proven.

## Phases

1. **P1 wire contracts** — proto RPCs (`EnsureIde`, `StartIde`/`StopIde`,
   `IdePortResponse`), TS codegen. *(landed with this ADR)*
2. **P2 guest** — agentd wire verbs + `ide.rs` + handler dispatch + golden
   tests; `deploy/bundles/ide/` + justfile wiring.
3. **P3 control plane** — core traits, FC/VZ/Process + pooled backends,
   host-agent gRPC, coordinator `ensure_ide` + stop-on-snapshot call sites.
4. **P4 orchestrator** — `routes/ide.ts` HTTP + WS bridge, wiring, catalog
   entry.
5. **P5 web** — IdePane + tab wiring + skills mirror.
6. **P6 validation** — live smoke on the dev stack (VZ), divergences recorded
   here, ADR flipped to Accepted.
