# ADR 0058: CLIs and MCP servers as first-class integration tooling

Status: 2026-06-24 — **P1 Accepted** (shipped + prod-validated). CLI integrations are live:
`gh` (#416) and Datadog `pup` (#417, replacing `datadog-ci`) are both proven end-to-end in a
prod Firecracker session — the CLI carries only a dummy placeholder; the proxy injects the
real credential host-side. The multi-credential connector UI (#418), an honest TestConnection
probe (#419), and the granted-power-opens-egress fix (#420) followed. **P2–P4 deferred** — see
[Phasing](#phasing-each-phase--one-pr-on-its-own-worktree-linear-stack) and the **P3/P4 pickup
notes** below, which pin the details a cold pickup would otherwise have to re-derive. The
**uploaded-binary arm** (`binSource:"uploaded"` — custom-connector tooling parity) is also
**shipped + prod-validated** (UB1–UB4: #422 / #423 / #424; a real uploaded static binary ran
on PATH in a prod FC session) — see the [Uploaded CLI binaries](#uploaded-cli-binaries--the-binsource-uploaded-arm-custom-connector-parity)
section below. Still **deferred:** P2 (`in-guest-token` + SigV4) and P3/P4 (MCP).

Builds on **ADR 0057** (profiles as the unified session-policy object + the runtime-managed
connector catalog), **ADR 0056** (generic integrations — the interceptor gate/inject/observe
+ connector config), **ADR 0006** (the host-agent egress proxy), **ADR 0027/0035/0055** (the
read-only host-mounted bundle engine + content-addressed generations + per-session dynamic
mounts), and **ADR 0023** (the per-session forge-credential broker seam).

## Context

ADR 0057 made integrations a first-class, runtime-managed product surface:
`capability → connector → {mint | inject} → egress policy`, admin-authored at
`/settings/integrations`, with credentials sealed in the coordinator org-secret store and
resolved **host-side** so they never enter the guest. The visible result is "magic curl" — a
session `curl`s `api.datadoghq.com` and the egress proxy injects `DD-API-KEY` on the way out
(`engram-egress-proxy/src/inject.rs`).

But the agent reaches integrations only through **raw HTTP** today. Real coding agents reach
for **native tooling** — `gh`, `datadog-ci`, `kubectl`, `sentry-cli` — and increasingly for
**MCP servers**. Neither is a first-class integration consumer:

- **CLIs** must be baked into an image, and there is no story for *authenticating* them
  without leaking a long-lived secret into the guest.
- **MCP** has never been wired. The open question the user raised is whether MCP's reliance
  on OAuth makes it unworkable for the machine/service users a sandbox runs as.

This ADR makes both first-class consumers of the *existing* credential + mount machinery.

### The premise that drives the design

A CLI or a remote MCP server is, to the egress proxy, **just another authenticated HTTPS
host** — exactly what ADR 0056/0057 already authenticate. Almost nothing here is new
substrate; it is composition of prod-validated rails:

- **Egress header injection with overwrite semantics** (`inject.rs`, ADR 0056 P3): the proxy
  *strips* any guest-supplied same-name header and writes the brokered one. The unit test
  `overwrites_an_existing_same_name_header` is the `gh`-issues-its-own-`Authorization` case
  verbatim.
- **Broker placeholder substitution + anti-exfil** (`substitute.rs`): swap a recognizable
  placeholder for the real value on allowed hosts; close the connection if it leaks elsewhere.
- **The RO-mount bundle engine**: content-addressed squashfs on reserved `dyn_0..dyn_11`
  slots; `mount.json` (`engram-mount-manifest`) already supports **many skills + many `bins`
  per drive** (`SkillEntry { name, bins, requires_env }`).
- **`session_env`** (agentd-owned): durable env applied to harness, `/exec`, and the ttyd shell.
- **The connector catalog** (`orchestrator/src/connectors/registry.ts`): the declarative
  `capability → auth + hosts + operations` source, admin-CRUD over gRPC (ADR 0057 C1/C3).

### Is OAuth a blocker for MCP? No — with eyes open

The MCP authorization spec (2025-11-25) gives two escape hatches from interactive OAuth:
**(1)** stdio transport is *explicitly exempt* — credentials come from the environment; many
top integrations ship stdio servers keyed on an API-key env var (GitHub, Sentry, PagerDuty,
Postgres, Datadog). **(2)** many remote HTTP servers accept a static PAT/API-key as
`Authorization: Bearer` (Linear, GitHub, Datadog, Atlassian-with-admin-enable). The genuine
OAuth-only holdouts are a minority (Slack, Notion, GitLab).

Crucially for engrams: a **remote MCP server is just another HTTPS host**, so the *existing
inject rail already authenticates it* — point the guest's MCP config at the URL with **no
token**, and the proxy injects `Authorization` host-side. *(Amended 2026-08-03, ADR 0106
addendum: for oauth-facet connectors the platform now DOES acquire, hold — KEK-sealed in
the ADR 0106 credential store — and proactively refresh tokens; org secrets no longer
store acquired tokens. BYO client id/secret/signing-secret remain org secrets.)*

But there is a deeper, engrams-specific prior: **ADR 0027 already chose CLI over MCP**
(§"Why not an MCP server"). MCP couples to the harness; `.mcp.json` is cwd-relative and
*prompts for approval* (fatal headlessly); a custom harness may not speak MCP at all; the
CLI is ~4× more token-efficient. So **CLIs are the first-class path and MCP is the
secondary, opt-in path** — for integrations that are MCP-only or materially better as MCP.

## Decision

### 1. CLIs: one shared bundle, one discovery skill, dummy-credential auth

- **One admin-curated "integrations CLI" bundle** — a `deploy/bundles/` recipe (peer of
  `skills`/`playwright`) carrying every supported CLI binary as a single content-addressed
  squashfs occupying **one** `dyn_*` slot, staged once per host, mounted by any profile that
  enables ≥1 CLI integration. Binaries symlink onto PATH via the existing `bins` wiring in
  `engram-session-bundles::activate()`. (Chosen over per-domain "toolboxes": one shared drive
  is simpler, dedups perfectly, and the slot budget is a non-issue — see §4.)
- **One "integrations" discovery skill** (a `SKILL.md`) that teaches the agent which CLIs
  exist and how to use them with the enabled integrations. This **replaces** the per-tool
  skills: `create-pull-request`'s bespoke wrapper + skill and `share-file` collapse into
  CLIs-in-the-bundle + lines in the one discovery doc. `gh` reuses the GitHub installation
  token already minted for git, retiring the bespoke `engram-pr` flow.
- **Per-session discovery filtering reuses the `requires_env` gate — no per-session render,
  no message.** This is exactly how `create-pull-request` already works: its `mount.json`
  entry is `{ name, requires_env: "ENGRAM_FORGE_TOKEN" }`, and `activate()` wires it only when
  that env var is present. The CLI bundle does the same — each CLI's wiring gates on its
  integration's `dummyEnv` var (`gh` on `GH_TOKEN`, `datadog-ci` on `DD_API_KEY`), so the
  agent sees only the enabled CLIs. The static discovery `SKILL.md` points at a small
  `engrams integrations` helper that reads the enabled-CLI catalog (provider/doc/bins) from
  **one env var** the orchestrator sets — so custom-connector docs surface too, still purely
  via `session_env`. Everything rides the two channels that already flow on `CreateSession`
  (`selected_skills` + `env_vars`); there is **no `CreateSessionRequest` change**.

### 2. The credential-delivery model is an open strategy, not a boolean

Model "how the real credential reaches the upstream request" as a **typed, open strategy on
the connector** (in `registry.ts`). This is the explicit "don't close the SigV4 door"
requirement — but the door is held open *at the contract*, not by plumbing a dead enum
through Rust. **P1's `inject` arm needs no host or coordinator change**: the egress proxy
already injects credentials generically (static org secrets *and* per-session minted tokens,
via the `mint_provider` path), so `inject` is purely an orchestrator-side compile decision
(emit the `dummyEnv` + select the bundle). A future non-inject arm (`in-guest-token`,
`request-signing`) is where real host work — and a host-side dispatch site — lands; until
then the strategy stays where the decision is made.

- **`inject`** *(P1, no proxy-logic change)* — host-side header overwrite. The CLI carries a
  **fixed dummy** (`GH_TOKEN=x-engrams-managed`, or a stub config file agentd writes like
  `/etc/gitconfig` today) to satisfy its local auth gate; `inject_headers` strips that header
  and writes the real credential (`secretRef` → org store → `SessionEgressPolicy.injects`) on
  the connector's `hosts`. The secret never enters the guest. Covers the header/body-auth
  majority (`gh`, `datadog-ci`, `kubectl` w/ token, `sentry-cli`, `linear`, most REST CLIs).
- **`substitute`** *(already shipped)* — broker placeholder swap for body/query cases.
- **`in-guest-token`** *(P2)* — mint a short-lived token via the broker/askpass seam (ADR
  0023 pattern, generalized) and materialize it in guest env/file only for CLIs that refuse a
  dummy.
- **`request-signing` / `sigv4`** *(future — the door we keep open)* — request-signing
  schemes (AWS SigV4 HMACs the canonical request with the secret key) cannot be satisfied by
  overwriting a header. Two implementations the seam admits: **(a)** a host-side re-signer in
  the egress pipeline (feasible *because the proxy already MITMs and holds the request
  plaintext*); or **(b)** in-guest key delivery via `in-guest-token`. **P1 must not encode
  `inject` as a special case that variant (a) would have to unwind** — dispatch is
  per-strategy from the start.

### 3. CLIs and MCP are declared on the connector — built-in *and* custom

The `cli` and `mcp` facets live on the `Connector` shape and are validated by
`parseConnector` (the ADR 0057 C1 admin-trust boundary), so **custom DB-authored connectors
carry them too** — an admin can attach a CLI or an MCP server to a novel integration with no
redeploy.

```
cli?: {
  bins: string[];                              // PATH command names this connector contributes
  binSource?: "bundled" | "uploaded" | "npx";  // where the binary comes from (provenance)
  dummyEnv?: Record<string,string>;            // fixed harmless values to satisfy local gates
  dummyFiles?: {...};                          // stub config files agentd writes
  credentialDelivery?: <strategy>;             // §2 open strategy; default "inject"
  doc: string;                                 // how-to text folded into the discovery skill
}
mcp?: { transport: "http" | "stdio"; url?: string; command?: string; args?: string[] }
```

**Binary provenance** is the one wrinkle for custom CLIs: declaring a CLI is runtime, but
getting its *binary* into the guest is the dependency. `bundled` (reuse a bundled CLI) works
immediately; a *novel* binary (`uploaded`) depends on the binary-bearing upload path ADR 0055
P2 explicitly deferred (markdown-only today); `npx` is runtime-fetched through the egress
proxy. **MCP has no such wrinkle**: a remote server needs only a URL + inject auth, and an
stdio server is `npx`-fetched — so *custom connectors get MCP with zero baking*, the cleanest
fully-runtime integration story.

### 4. Remote MCP reuses inject; stdio MCP reuses session_env

- **Remote MCP** — register via **`--mcp-config <file>`** in `build_claude_argv`
  (`engram-harness-claude`), the headless-safe path that dodges the cwd-`.mcp.json` approval
  prompt; agentd writes the file from the enabled MCP connectors. Auth = the inject rail (no
  token in the guest). Headless **tool pre-approval** must be wired
  (`enableAllProjectMcpServers` / the hook-bridge permission machinery) so MCP tools don't
  block the unattended loop.
- **stdio MCP** — key via `session_env`; server binary `npx`-fetched at runtime or shipped in
  the CLI bundle. The machine-user-friendly path for GitHub/Sentry/PagerDuty/Postgres/Datadog
  stdio servers.

### 5. Mount-budget reconciliation

The 12-slot ceiling (FC x86 virtio-mmio GSI pool, `pci=off` — hard; `pci=on` correctly
rejected in ADR 0055) stops being a live constraint: skills → 1 · browser → 1 · **all CLIs →
1 shared bundle slot** · remote MCP → **0** · stdio MCP binaries ride the CLI bundle (0
extra). A heavy profile lands at ~3 drives, not ~12. ADR 0055's size-aware packing stays the
documented fallback if a single bundle ever grows unwieldy.

## Phasing (each phase = one PR on its own worktree; linear stack)

- **P1 — CLI bundle + discovery skill + dummy-cred `inject`.**
  `deploy/bundles/integrations-cli` recipe (binaries + a discovery `SKILL.md` + an `engrams
  integrations` helper + `mount.json`); `cli` facet on the connector model (built-in + custom,
  validated by `parseConnector`); the open `credentialDelivery` strategy as an orchestrator
  compile concept (`inject` only; others parse-rejected). `compileCliIntegrations` →
  `tasks.ts` unions the bundle into `selected_skills` and merges `dummyEnv` into `env_vars`;
  the enabled-CLI catalog rides one more env var. Migrate `create-pull-request`/`share-file`
  into the bundle (`gh` on the existing GitHub mint token). **No `CreateSessionRequest` /
  coordinator / proxy change** — it rides the same mount + env-gate path `create-pull-request`
  uses. Validate the inject-overwrite path end-to-end on dev-vm FC.
- **P2 — `in-guest-token` + the SigV4 door.** Generalize the broker/askpass seam to
  mint+materialize a short-lived credential in-guest for the `in-guest-token` strategy. Land
  the `request-signing`/`sigv4` strategy *shape* (variant + dispatch arm) even if a concrete
  signer isn't built — proving the seam is real, not aspirational.
- **P3 — Remote MCP.** `mcp` connector facet (built-in + custom); agentd writes
  `--mcp-config`; harness wires `--mcp-config` + headless tool pre-approval; auth via inject.
  `/settings/integrations` gains the MCP facet form.
- **P4 — stdio MCP.** session_env key delivery; server binaries via `npx` or the CLI bundle.

### P3/P4 pickup notes (what the phasing leaves to a spike)

The design above is settled; three implementation details are deliberately un-pinned because
they're harness-version-coupled or postdate authoring. A P3 implementer should resolve these
**first** — ideally with a short spike against the pinned `claude` CLI version — before writing
the `mcp` facet + compile code:

1. **The `--mcp-config` file format + the headless pre-approval flag — the real risk, and the
   one thing not derivable from this repo.** `build_claude_argv` already accepts the flag, but
   the JSON schema `claude` expects is not captured here and drifts with the CLI. Verify against
   the pinned version, but expect: an object `{ "mcpServers": { "<name>": { … } } }` where an
   **http** server is `{ "type": "http", "url": "https://…" }` (no token — the proxy injects
   `Authorization`) and a **stdio** server is `{ "command": "…", "args": […], "env": { … } }`.
   The *second* unknown is headless tool approval: `--mcp-config` dodges the cwd-`.mcp.json`
   *approval prompt*, but the MCP tools still need pre-approval or they block the unattended
   loop — confirm whether that's `--permission-mode`, a `settings.json` key
   (`enableAllProjectMcpServers` / `enabledMcpjsonServers`), or the existing hook-bridge
   permission path. **Spike this against the live harness before committing to a shape.**

2. **The wire path: a new `harness_env` var + an agentd writer, mirroring P1 — no new wire
   field.** P1 needed *zero* `CreateSessionRequest`/coordinator change: the enabled-CLI set
   rides `ENGRAM_CLI_INTEGRATIONS` and the bundle's `requires_env` gate. P3 follows the same
   shape — a `compileMcpIntegrations` (peer of `compileCliIntegrations` in
   `connectors/registry.ts`) emits the enabled MCP set, `tasks.ts` sets it as one more
   `harness_env` var, and **agentd** reads it to write the `--mcp-config` file into the guest at
   session start (agentd already owns `session_env` and the harness spawn). Keep it wire-free
   unless the spike forces otherwise.

3. **Egress reachability is already solved — just declare `hosts` on the connector.** This ADR
   predates **#420** (`fix(adr-0057): a granted integration power opens egress to its
   connector's hosts`): `compileIntegrationPolicy` now unions every granted connector's `hosts`
   into the session's egress allow-list. So a remote-MCP connector that lists its server host in
   `hosts` gets DNS/egress reachability *and* the `Authorization` inject for free — no separate
   network-policy step. Without #420, P3 would have rediscovered the same silent "could not
   resolve host" the `pup` e2e hit (the inject opens the credential, not the route).

## P1 implementation notes (divergences from the plan)

P1 shipped on `adr-0058-p1-cli-integrations`. What landed, and where it simplified the design:

- **Zero new wire / host / coordinator change** (the big simplification, from tracing the
  existing skill mechanism). The CLI bundle + discovery reach the session exactly like
  `create-pull-request` did: `selected_skills` + `env_vars` + `activate()`'s `requires_env`
  gate. `compileCliIntegrations` (`connectors/registry.ts`) → `tasks.ts` unions the
  `integrations-cli` bundle into `selected_skills` and merges `dummyEnv` into the harness env.
  The proxy already injects both static (Datadog) and minted (GitHub) credentials, so the
  dummy placeholder is the *only* new in-guest artifact.
- **Discovery rides one env var + a helper, not a per-session render.** The orchestrator sets
  `ENGRAM_CLI_INTEGRATIONS` to the JSON catalog of enabled connectors' `cli` facets
  (`{provider, displayName, bins, doc}`); the bundle's `engrams-integrations` helper prints it
  (jq → python3 → raw fallback). The discovery `SKILL.md` is static + generic; per-connector
  how-to lives in the connector's `cli.doc` (so custom connectors surface theirs too). The
  `integrations` skill `requires_env: ENGRAM_CLI_INTEGRATIONS`, so it only wires when ≥1 CLI is
  enabled.
- **`credentialDelivery` is an orchestrator-only typed concept.** `inject`/`substitute` are
  implemented; `in-guest-token`/`request-signing` are valid types but `parseConnector`
  rejects them ("not yet wired") so no silently-unauthenticated CLI ships — the SigV4 door is
  open at the contract, not via dead Rust.
- **`dummyFiles` is modeled but unused by the built-ins** (`gh`/`datadog-ci` are satisfied by
  `dummyEnv`); it's a connector-authoring option for config-file-only CLIs.
- **`create-pull-request` fully retired** — replaced by `gh` from the bundle (its PR/issue
  guidance moved to `github.json`'s `cli.doc`); every code reference swept. Git push stays
  brokered via the skills bundle's askpass/gitconfig.
- **Commit chain:** `docs(adr-0058)` (Proposed) · `feat` cli-facet+credentialDelivery ·
  `docs` simplify · `feat` tasks.ts wiring · `feat` bundle+connector-facets · `refactor`
  retire create-pull-request · `ci` build/publish/stage the bundle.

- **Bundle build dev-vm-validated; `gh`/`datadog-ci` run; two recipe bugs fixed.** Building +
  running the squashfs on the KVM dev-vm surfaced (1) `collect()` tripping `set -e`/`pipefail`
  on `gh` (a *static* Go binary makes `ldd` exit non-zero), and (2) `datadog-ci` (a Node SEA)
  segfaulting under a bundled libc/loader, plus a bundled libstdc++ (built against glibc 2.36)
  refusing to load on an older base. **Resolution: ship the binaries directly with no bundled
  libs/loader/wrappers** — `gh` is static; `datadog-ci` runs against the **base image's own
  glibc + libstdc++**. Validated: `gh` 2.62.0 + `datadog-ci` v2.48.0 both run, the helper
  renders, 48 MB squashfs. **Constraint this imposes:** the bundle targets a *full glibc base*
  (libstdc++ present) — a slim glibc image must `apt-get install libstdc++6`, the same
  constraint the binaries carry themselves (consistent with the playwright bundle being
  glibc-only). Bundling a matched-glibc libstdc++ for true any-base portability is a documented
  follow-up if a libstdc++-less base ever selects a CLI integration.

- **Post-P1 follow-ons (all merged + prod-validated).** (1) `datadog-ci` → **`pup`** (#417), the
  official Datadog agent CLI — a glibc Rust binary that sidesteps the Node-SEA libc fragility
  the dev-vm hit, and the connector model gained **N injected headers per connector** (`pup`
  needs `DD-API-KEY` *and* `DD-APPLICATION-KEY`). (2) The web gained a **multi-credential**
  connect/rotate/author flow (#418). (3) **TestConnection** now probes *every* injected header
  on an **honest path** (`test.path`, default `/`) — Datadog's `/` 307-redirects to a public
  page, so the old probe passed vacuously; it now hits `/api/v1/dashboard`, which needs both
  keys (#419). That PR also renamed the coarse Datadog grant `read` → `observability:read` so it
  humanizes as "Read observability". (4) The **full prod FC e2e** ran: in a real `demo-claude`
  session, `gh` (mint token) and `pup` (two static keys) both authenticate against the live API
  while the guest holds only `x-engrams-managed` dummies — surfacing and fixing the egress-allow
  gap in #420 (see P3/P4 pickup note 3).

- **Datadog profiling CLI bridge (2026-08-13).** `pup` 1.4.0 can search spans, but it does not
  support Continuous Profiler. The Datadog MCP server is the supported agent surface for
  profile types, allocation flame graphs, call graphs, and profile time series. We did not
  enable harness-native MCP (P3/P4 remains deferred). Instead, the shared bundle now includes
  a small engrams-owned `datadog` CLI. It is a Streamable HTTP client for the fixed
  `?toolsets=profiling` endpoint and a generic REST client for allowed Datadog API paths. The
  `profiling:read` power opens only that profiling toolset endpoint. The existing `apm:read`
  power also opens the previously missing span-aggregate path. `pup` stays available for its
  broad command surface; this avoids a large usability regression for products that the small
  wrapper does not model.

  Datadog's REST API and MCP server use different header names for the same two keys. REST uses
  `DD-API-KEY` and `DD-APPLICATION-KEY`; MCP uses `DD_API_KEY` and `DD_APPLICATION_KEY`.
  Connector inject entries can therefore name an optional subset of the connector's top-level
  hosts. The compiler applies each header only to that host subset. The connect UI deduplicates
  aliases by `secretRef`, so an administrator still enters and rotates exactly two stored keys.
  The application key must have Datadog's `mcp_read` and `continuous_profiler_read`
  permissions for `profiling:read` to work.

**Deferred (documented, not dropped):** `in-guest-token` + the `request-signing`/SigV4 arm
(P2); MCP facet + `--mcp-config` + headless approval (P3/P4); `binSource: uploaded`/`npx` (the
ADR 0055 P2 binary-upload + runtime-npx paths). **Pre-merge:** the full FC-session e2e
(`gh`-with-dummy-token through a live egress-injected GitHub App token) — the bundle build +
CLI execution are dev-vm-validated, the orchestrator compile/wire + `activate()` mount-wire +
the `inject.rs` overwrite are unit-tested, so this is the integration of validated pieces.
**Post-merge:** the engrams-internal host re-bake + image re-enable to stage the bundle on the
prod fleet.

## Uploaded CLI binaries — the `binSource: "uploaded"` arm (custom-connector parity)

P1 made CLIs first-class for *built-in* connectors; a custom connector reaches the same parity
only for tools already in the baked bundle (`gh`, `pup`). The remaining gap is **binary
provenance for a novel tool** — the deferred `binSource: "uploaded"`. This section is its design.
(Remote/stdio **MCP** is the other half of custom parity and stays tracked under P3/P4.)

**The machinery already exists — the gap is one hardcoded field.** The ADR 0055 P2
`mount_catalog` upload path is *not* markdown-gated by code, and everything downstream of the
upload is binary-agnostic: `skill_pack.rs` accepts arbitrary tar/zip (rejecting only
symlinks/hardlinks/devices), content-addresses + packs a deterministic squashfs, and writes a
`mount_catalog` row; catalog rows UNION straight into the bundle pin set so hosts auto-stage
`<sha>.squashfs` on the next heartbeat (no `current.json`/node-assets change — catalog names
resolve via the table); and `resolve_selected_skills` → `patch_drive` → `activate()` are fully
generic — `activate()` symlinks *any* mounted `mount.json`'s declared `bins` onto
`/usr/local/bin`. The sole blocker: the pack-time manifest is hardcoded to
`MountManifest::single_skill(name)` with **empty `bins`**, so an uploaded binary stages and
mounts but never lands on PATH. **Zero host-agent / Firecracker / guest-activation change** — only
the upload contract, the connector compile, and the authoring UI move.

Decisions:

- **The upload declares its bins.** `RegisterSkillRequest` gains `repeated string bins`;
  `pack_skill` validates each is a regular file *inside* the archive (no traversal/symlink) and
  marks it executable, then folds them into the generated `mount.json` — with **no
  `requires_env`** (an uploaded bundle is mounted only when a connector referencing it is granted,
  so `selected_skills` membership *is* the gate). Empty `bins` = today's markdown behaviour,
  byte-for-byte. Bins ride the existing `mount_catalog.mount_json` column — **no migration**.
- **Raise the size ceiling for binary bundles.** The markdown-era caps
  (`MAX_SKILL_UNPACKED_BYTES = 16 MiB` / 2 MiB compressed) are too small (`gh` alone is ~30 MB).
  Raise to a binary-appropriate ceiling (~128 MiB unpacked / ~64 MiB compressed, tuned against
  `gh`/`kubectl`/`aws`-class binaries), keep the file-count cap, keep the caps enforced.
  squashfs + content-address dedup means re-using a binary across connectors costs one
  fleet-wide copy.
- **The connector references the uploaded bundle by name (Model A).** The `cli` facet gains
  `binSource: "uploaded"` (lift the `parseCli` rejection) + `bundle: "<mount_catalog name>"`;
  `compileCliIntegrations` adds `cli.bundle` to `selected_skills` for a granted uploaded-CLI
  connector instead of the shared `INTEGRATIONS_CLI_BUNDLE`. Everything else (dummy env, the
  discovery doc, the inject rail, the #420 host-open) is unchanged. Reference-by-name keeps the
  binary a content-addressed catalog artifact with its own lifecycle — one uploaded bundle can
  back several connectors — while the UI may *present* it as one inline "upload + reference" step.
  (Rejected — Model B, embedding the binary in the connector row: couples a content-addressed
  artifact to one connector and duplicates the catalog's storage/GC.)
- **Trust is unchanged.** Uploads are admin-only (`registerSkill` checks the role) and connectors
  validate at the same `parseConnector` boundary; the binary runs in an **isolated microVM**,
  content-addressed with an `owner` + soft-delete/GC (0055 P2). The glibc-base constraint (above)
  carries over — an uploaded dynamically-linked binary runs against the base image's libc;
  static/musl binaries are safest. Not a new trust tier — the one skills/bundles already occupy.

Phasing (its own linear stack, peer of the P-series above):

- **UB1 — upload-declares-bins** (foundation, connector-agnostic): proto `bins` + `pack_skill`
  validation + `MountManifest` bins builder + the cap raise. Verifiable via `selected_skills`
  alone — dev-vm FC: upload a tiny binary, select it, confirm it's on PATH, before any connector
  wiring exists.
- **UB2 — connector wiring**: `parseCli` accepts `uploaded` + `cli.bundle`;
  `compileCliIntegrations` routes the bundle into `selected_skills`; unit tests + an end-to-end
  custom-connector resolve.
- **UB3 — authoring UI**: `CustomConnectorModal` gains the `cli` section (missing today) + the
  inline binary-upload affordance.
- **UB4 — prod e2e + Accepted-for-uploaded**: upload a real CLI, author a custom connector, run
  it against a live API in a prod FC session (mirroring the `gh`/`pup` proofs).

**What landed (UB1–UB4 — shipped + prod-validated).** The whole arm is on main and proven in a
real Firecracker session:

- **UB1** (#422) — `RegisterSkillRequest.bins`; `pack_skill` validates each declared bin is a
  regular file in the archive (rejects ghosts + traversal), chmods it 0755, folds it into the
  generated `mount.json` as `skills/<name>/<bin>`; caps raised to 64 MiB / 256 MiB with the
  `MountCatalogService` decode cap → 80 MiB. Zero host/Firecracker/guest change — the only gap
  was that the pack-time manifest hardcoded empty `bins`.
- **UB2** (#423) — `parseCli` accepts `binSource:"uploaded"` + a `cli.bundle` reference;
  `compileCliIntegrations` routes the catalog bundle into `selected_skills` (alongside the shared
  integrations bundle, now mounted for any enabled CLI so the discovery helper is present).
- **UB3** (#424) — `CustomConnectorModal` gains the CLI section + an inline binary-upload flow;
  `connectorModel.ts` parses the `cli` facet. UX note: the upload takes a prepared tar/zip
  carrying `bin/<tool>` + a top-level SKILL.md; a client-side tar-wrapper for a raw binary is a
  documented follow-up.
- **UB4 — prod e2e (passed).** Uploaded a real 2.3 MB static binary (a renamed `jq`) via the
  catalog `RegisterSkill(bins)`, selected it on a `demo-claude` session, exec'd it: it landed on
  PATH at `/usr/local/bin/e2e-cli → /opt/engram/dyn/0/skills/e2e-uploaded-cli/bin/e2e-cli` (the
  patch_drive'd slot — proving it's the upload; there is no base-image `jq`), executed
  (`jq-1.7.1`), and ran a real filter. Validates the full chain — `pack_skill` (bins manifest) →
  content-address → pin set → host materialize → `resolve_selected_skills` → patch_drive →
  `activate()` PATH symlink → execute — in real Firecracker. The driven e2e hit the coord surface
  directly (`selected_skills` = exactly what UB2's compile emits); UB2's compile + UB3's authoring
  are unit-tested.

**Commit chain:** UB1 #422 · UB2 #423 · UB3 #424. CI-integrity follow-on surfaced by this work:
#425 folded the former macOS workflow into `ci.yml` and made the e2e stack actually gate
(quarantining the #403 flake) — the `CI Gate` had silently not been blocking on e2e.

## Consequences and risks

- **No host/coordinator/proxy change in P1**: the inject-overwrite path already ships and
  already covers both static + minted credentials; CLIs ride the same mount + `requires_env`
  gate `create-pull-request` uses. P1 is an orchestrator + bundle + docs change. Much lower
  blast radius than the original framing suggested.
- **CLI bundle re-bake**: the integrations bundle is a new `deploy/bundles/` recipe staged
  into the FC-host image — a host-image roll, like adding any bundle (ADR 0035/0055).
- **`parseConnector` gains surface**: `cli`/`mcp` facets can open egress + inject org secrets
  + add PATH binaries — validated at the same admin-trust level as today's connector hosts.
- **Custom-connector binary provenance** is addressed by the `binSource: "uploaded"` arm
  (above), which lifts the ADR 0055 P2 binary-upload deferral: novel custom CLI binaries upload
  to the catalog and ride the existing stage/mount/activate path. Bundled CLIs work now; remote/
  stdio MCP (P3/P4) needs no binary at all.
- **SigV4 / non-HTTP** (`aws`, ssh, DB wire protocols) genuinely cannot ride host-side
  overwrite — handled by the open strategy (P2+), not by pretending they work in P1.
- **MCP harness-coupling + headless approval** are real constraints; MCP stays opt-in and
  CLI-first, consistent with ADR 0027.
- **Backwards-incompatible wire/schema changes** are acceptable — 0 users; clean breaks.

## Alternatives considered

- **Per-domain "toolbox" bundles** (cloud / k8s / observability, each its own slot). Rejected
  for one shared bundle: simpler, perfect dedup, and the slot budget isn't pressured (§5).
  Size-aware packing remains the ADR 0055 fallback if the single bundle ever grows unwieldy.
- **Materialize the real credential in session_env for every CLI.** Rejected as the default:
  it puts long-lived org secrets in the guest. The dummy + host-side inject keeps the secret
  host-side; `in-guest-token` (short-lived, minted) is the bounded exception for CLIs that
  need a real local credential.
- **`credentialDelivery` as a boolean (`inject | in-guest`).** Rejected: it would force a
  later SigV4 re-signer to unwind a special-case. The open strategy is the "simplify via
  abstractions" shape — one dispatch site, N arms.
- **MCP-first / MCP as the primary tooling rail.** Rejected per ADR 0027's CLI-over-MCP
  finding (harness-coupling, headless approval, token cost). MCP is the opt-in complement.
