# ADR 0023: Engrams on engrams — dogfooding the product plane

Status: 2026-05-29 — **Accepted.** P1 (the dogfood loop) is shipped; see the commit chain and
the one design divergence below. The micro-VM substrate is mature (ADRs 0019–0022). This
ADR opens the *product* plane: the surfaces that turn engrams from a sandbox orchestrator into
a product an org actually drives — git handoff, integrations (Slack/Linear), identity — and
the dev loop where we build those surfaces by **running agents inside engrams sessions**. P1
(scoped below) is the first slice: a guarded non-FC backend, a provider-agnostic `GitForge`,
an in-session forge seam, and the built-in `create-pull-request` skill (the dynamic,
centrally-managed skills platform is designed here for a later phase). Later phases are
designed here but deferred. Nothing in this ADR touches the substrate's latency wins.

**P1 commit chain.** `a2a5d83` (#45 — `GitForge` trait + types + `engram-git-{github,dev}`,
guarded `ProcessBackend`, forge API + per-session broker token + `[git]` manifest block,
ProcessBackend loopback transport) → `2728fd1` (Firecracker vsock forge bridge) → `76c2308`
(dogfood image `deploy/dev-engrams` + built-in `create-pull-request` skill) → `0e577f1`
(Cargo.lock catch-up for the two new crates + agentd deps) → `080c217` (real-microVM
forge-bridge e2e test + CI wiring). Verified: `just check` green (854 tests) + the FC
`forge_loopback` integration test green on the dev-vm.

**One divergence from the proposed design.** §3 proposed multiplexing the forge RPC onto the
existing harness vsock channel (`GuestRequest`/`GuestResponse` — "no new vsock port, no
per-backend plumbing"). The implementation instead added a **dedicated forge vsock port**
(`FORGE_VSOCK_PORT` = 1028) with its own `ForgeRequest`/`ForgeResponse` framing and a
`ForgeSink` that mirrors the existing `HarnessSink`. This keeps the forge protocol decoupled
from the harness `GuestRequest` enum and its accept loop, and lets the coordinator wire the
forge channel exactly like the harness sink — one more instance of the established seam rather
than a widening of the harness contract. The cost is the per-backend plumbing the proposal
hoped to avoid (FC `spawn_forge_listener`), but it is a direct mirror of the
`spawn_harness_listener` already present in each backend, so the marginal surface is small.

## Context

There are two planes of engrams development with different physics:

- **Substrate plane** — Firecracker, host-agent, snapshots, networking, chunked storage. KVM
  is required and **does not nest**, so this plane is developed and tested on Linux+KVM (the
  laptop's dev-vm or a real host). It cannot be dogfooded inside an engrams session.
- **Product plane** — the coordinator's HTTP surface for git handoff, auth, and third-party
  integrations, plus the web app. This is ordinary async Rust + Postgres + React. It needs no
  KVM: building and testing it requires only the `ProcessBackend`, which already exercises the
  full coordinator API + session state machine in `crates/engram-coordinator/tests/api.rs`.

The product plane is therefore *dogfoodable*: a Claude Code agent in an engrams session can
build and test it, then hand back a reviewable artifact (a PR). Making that loop real is the
goal. It requires four things, three of which P1 delivers.

### What's greenfield (evidence)

- **Backend selection** is `Firecracker | Vz` only (`crates/engram-coordinator/src/config.rs`).
  `ProcessBackend` is test-wired but not operator-selectable; `process` was deliberately removed
  from `SandboxBackendChoice` once harness dispatch moved onto the trait (`config.rs` test).
- **Git** is not a platform concern today. ADR 0005 retired git-as-*durability* but explicitly
  kept it as "the egress channel the agent uses for handoff (PR creation), mounted via the
  `[secrets.X]` machinery"; ADR 0001 **Track F.6** deferred a `POST /sessions/:id/pr` +
  "GitHub App + Workload Identity" path "until the prod auth path lands." This ADR is that path.
- **Identity** is a single deployment-wide bearer token (`api/auth.rs`); `sessions.user_id` is
  an unused caller hint. No users/orgs/integrations tables. Fully greenfield.

## Decision

### 1. A guarded, non-FC product-plane runtime (`--sandbox-backend=process`)

Re-expose `ProcessBackend` as an operator-selectable backend so a full coordinator can run the
product plane without KVM (laptop, or an engrams session). The backend has **no isolation**
("never use with untrusted input" — `crates/engram-sandbox-process/src/lib.rs`), so it is
hard-gated: refuse to boot unless `ENGRAM_ALLOW_INSECURE_PROCESS_BACKEND=1`, with a loud
startup banner. It bypasses the `PooledBackend` wrapper (no chunk store / egress / materialize).
This reverses the earlier removal of the `process` variant — a deliberate, documented decision,
not a regression.

### 2. `GitForge` — a provider-agnostic git authority (mint + change-request)

A `GitForge` trait in `engram-core` (mirroring `SecretStore` / `BlobStorage`) is a
**platform-side authority** with two provider-agnostic operations:

- `mint_repo_token(repo) -> ScopedToken` — a short-lived, repo-scoped credential.
- `create_pull_request(repo, spec) -> PullRequest` — open a **change request**, neutral over
  GitHub Pull Requests, GitLab Merge Requests, Gitea/Bitbucket PRs. Neutral types
  (`head_branch`/`base_branch`/`title`/`body`/`draft` → `url`/`id`/`state`) verified to map
  onto both GitHub (`head`/`base`/`body`/`html_url`/`number`) and GitLab
  (`source_branch`/`target_branch`/`description`/`web_url`/`iid`).

The **agent drives the work** — it runs `git push`, decides when to open a change request, and
supplies the title/branches — but invokes engrams' uniform op rather than shelling out to a
provider-specific `gh`/`glab`. The abstraction (not the agent) owns provider differences, and
the platform captures the resulting URL. GitHub ships first (`engram-git-github`, a GitHub
App: RS256 JWT → installation token); GitLab/Gitea are the agnosticism payoff, deferred.

This honours ADR 0005: the platform does no git data operations (no clone/fetch/push); it
mints credentials and brokers the change-request API call. The agent still does the git.

### 3. Credentials on demand — the control plane is the token authority

GitHub App **installation tokens live exactly one hour**; a coding session routinely runs
longer, and the push-at-the-end is when a stale token bites. Comparable open-source tools
converge on one answer: **Coder** injects `GIT_ASKPASS` and fetches a server-held token on
demand ("the workspace stores no long-lived secret"); **Gitpod** mounts a `gp` credential
helper for "temporary credentials"; **Codespaces** registers `gh` as the helper; **OpenHands**
stores tokens server-side and refreshes lazily. The shape: **the control plane is the token
authority and holds the only durable secret (the App private key); the sandbox holds nothing
durable and fetches a fresh credential on demand via a `GIT_ASKPASS` helper; refresh is lazy
and server-side.** We adopt it.

Mechanism — the **in-session forge seam**: a coord HTTP surface
(`GET /sessions/:id/git-credential`, `POST /sessions/:id/pull-request`) authorized by a
**per-session broker token** (the only engram credential placed in the guest — session-scoped,
least-privilege, the cloud-metadata-server identity model). `git push` is stock git; the
askpass supplies a fresh token transparently per op, so **token expiry is invisible in-guest**.
Two transports:

- **ProcessBackend / `--mode=all`** — the guest shares host networking; it hits the coord
  forge API on loopback directly.
- **Firecracker** (the priority for product features) — reuse the **proven guest→host harness
  vsock channel** (port 1026, `engram-harness-proto` framing): add
  `GuestRequest::{FetchGitCredential, CreatePullRequest}` ⇄ `GuestResponse`, handled host-side
  and forwarded to the coord forge API. No new vsock port, no per-backend plumbing.
  _(Shipped differently — a dedicated forge vsock port + a `ForgeSink`; see the divergence note
  under Status.)_

Short-lived creds in-guest are acceptable (single-org trusted-tenant assumption — DESIGN.md),
so the egress-proxy secret-substitution approach is **dropped** (not deferred): it's an
exfiltration-prevention pattern, orthogonal to expiry, absent on ProcessBackend, and would
require Basic-auth-aware MITM substitution for git's base64 `Authorization: Basic` headers.

The forge seam is the **general mechanism for handing a short-lived, mediated capability to an
in-session agent** — Slack/Linear integrations ride the same seam later.

### 4. Skills — the agent-facing discovery layer

The agent must *know* `engram-pr` exists (we deliberately don't use `gh`). A **skill**
(`SKILL.md` + scripts) is how harnesses learn a capability; `create-pull-request` becomes a
built-in skill whose `SKILL.md` teaches the op and whose `engram-pr` script is the forge-seam
client. Skills generalize into the **universal delivery vehicle** for every platform
capability (PR now; `post-to-slack` / `comment-on-linear` later) — each integration ships as a
skill over a seam, not bespoke wiring.

**Phase 1 bakes the built-in skill into the image's rootfs** at `~/.agents/skills/` (the Claude
loader dir is symlinked at it). This is the simplest delivery that works on *both* backends
with **no per-backend mount code** — it's just files in the rootfs, present on FC (the booted
rootfs) and on ProcessBackend (the materialized rootfs) alike — it's ADR-0021-consistent (a
static built-in rides the warm snapshot like the baked harness), and it adds **zero** boot
cost or resume complexity. It unblocks the dogfood loop now.

The **dynamic, per-session, RO-mounted, centrally-managed skills platform** — host-synced skill
store + per-session RO mount + user uploads — is **deferred** (below). Worth recording *why* it
isn't a quick "mount it" on FC: the per-session harness *drive* was **retired in ADR 0021 P1.5**
(the harness moved into the rootfs), and Firecracker has **no virtiofs** (its device model is
virtio-blk/net/vsock/rng/balloon only). So a per-session RO *mount* on FC necessarily means an
**additional read-only virtio-blk drive** (`put_drive` with `is_read_only`) built from the
host-synced store, mounted guest-side, and **re-anchored on resume** (the snapshot embeds the
host path) — i.e. reviving the drive machinery 0021 removed. That's a sizable, FC-iterative
feature deserving its own design, and it's only needed once skills are *dynamic*; static
built-ins don't need it. Semantics when it lands: **fixed-at-boot** (a session uses the skills
present when it boots; no mid-session hot-add).

## Phasing

**P1 (this ADR's implementation) — the dogfood loop.**

1. Guarded `ProcessBackend` backend.
2. `GitForge` trait + types + `engram-git-dev` mock.
3. `engram-git-github` (GitHub App: lazy-cached installation tokens + create-PR).
4. In-session forge seam: coord API + per-session broker token + `[git]` manifest block + env
   injection, on both transports (ProcessBackend loopback; FC vsock bridge — the priority).
5. The built-in `create-pull-request` skill — `SKILL.md` + the `git-askpass` / `engram-pr`
   forge-helper scripts — **baked into the dogfood image's rootfs** (works on both backends, no
   per-backend mount code).
6. A dogfood image (`deploy/dev-engrams`) + tests (ProcessBackend e2e in the default job; FC
   forge bridge validated on the dev-vm).

**Deferred (designed here, not built in P1).**

- **Dynamic, centrally-managed skills platform**: the per-session RO-**mount** engine (a
  host-synced content-addressed skill store + an FC additional read-only virtio-blk drive with
  resume re-anchor — see §4 for why this revives 0021-retired drive machinery; ProcessBackend
  materializes a dir), plus user uploads (registry + upload API/UI, BlobStorage-backed skill
  zips, a PG `skills` table with org enable/disable mirroring `enabled_images`). Trust boundary:
  **user skills may only call already-exposed seams; they never add privileged platform
  surface** (built-in skills may pair with new coord endpoints; uploaded zips may not). Skill
  scripts execute with the session's privileges (incl. the broker token), acceptable for
  single-org/trusted-tenant; multi-tenant wants skill signing/provenance.
- **`GitForge` for GitLab / Gitea** — the agnosticism payoff.
- **Identity & tenancy**: `users` / `orgs` / `org_members`, per-user auth beyond the bearer
  token; `sessions.user_id` becomes real.
- **Integration credential storage** (per-user/org OAuth) under the `engram-crypto` KEK/DEK
  envelope, delivered over the in-session forge seam.
- **Slack / Linear connectors** (`ChatOps` / `IssueTracker` traits) + their skills.
- **Inbound webhook ingress** (HMAC-verified, distinct from the bearer token).
- **Outbound-action framework + idempotency keys** (the ADR 0001 contract: a resumed session
  must not double-fire `slack.post()`), wiring `SessionEvent`s → external effects.
- **PG-backed broker tokens** + split-mode forge-forward hardening (P1's broker map is
  in-memory, fine for single-binary `--mode=all`).
- **Web auth** (the `ProfilePanel` placeholder becomes real).

## Consequences

- The product plane becomes buildable and testable without KVM — the dogfood loop closes.
- `GitForge` + the forge seam establish the integration pattern: a platform-side trait
  authority + an in-session seam + a skill. Every future integration is a smaller instance of
  the same shape rather than new architecture.
- A dedicated guest→host **forge** vsock channel (`FORGE_VSOCK_PORT`, with a `ForgeSink`
  mirroring `HarnessSink`) — the first guest-originated RPC. (The proposal expected this to
  ride the harness channel; see the Status divergence note for why it shipped as its own port.)
- The built-in skill rides the rootfs (no per-backend mount code); the deferred dynamic-skills
  mount engine will bifurcate per backend (FC RO virtio-blk drive vs ProcessBackend dir).
- `ProcessBackend` as a runnable backend is a loaded footgun; the insecure-flag gate +
  banner are load-bearing and must stay.

## Open questions

1. **Dynamic-skills delivery (deferred engine).** Baking the built-in skill into the rootfs
   sidesteps both the FC mount mechanism (an additional RO virtio-blk drive + resume re-anchor,
   reviving 0021-retired machinery) and the harness skill-scan-timing question (a baked skill is
   present at the harness's snapshot-time scan). Both resurface when dynamic / user-uploaded
   skills land — that phase must confirm the Claude adapter re-scans at session start, not only
   at baked process start.
2. **Broker-token lifetime / revocation** across idle→resume and across coord pods (P1
   in-memory map; PG-backed is the multi-pod follow-on).
3. **Multi-installation forge.** P1's credential endpoint uses the `[git] owner` (or the App's
   sole installation). A forge App spanning several orgs needs per-request owner disambiguation
   (the helper would pass the target owner) — fine to defer; one installation covers the
   dogfood org.

## References

- ADR 0001 (Track F.6: deferred `POST /sessions/:id/pr` + GitHub App), ADR 0005 (git retired
  as durability; kept as agent handoff), ADR 0018 §12p (harness-drive resume re-anchor — the
  pattern a future dynamic-skills RO drive would revive), ADR 0021 P1.5 (retired the per-session
  harness drive; baked it into the rootfs — the boot-cost lesson this ADR follows for skills),
  ADR 0022 (the substrate's next lever).
- Prior art: Coder external-auth (`GIT_ASKPASS`, server-held tokens), Gitpod `gp` credential
  helper, GitHub Codespaces (`gh` as credential helper), OpenHands (server-side token store +
  lazy refresh). GitHub App installation tokens: 1h, `POST /app/installations/{id}/access_tokens`.
- GitHub `POST /repos/{owner}/{repo}/pulls` vs GitLab `POST /projects/{id}/merge_requests` —
  the neutral `PullRequestSpec` mapping.
