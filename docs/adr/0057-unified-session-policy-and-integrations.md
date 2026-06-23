# ADR 0057: Profiles as the unified session-policy object + first-class integrations

Status: 2026-06-22 — **Proposed.** Implemented across the phase chain in §Phasing;
this ADR is updated between phases and flipped to **Accepted** at the end (phase D2)
with the commit chain.

Builds on ADR 0056 (generic integrations — the interceptor gate/inject/observe +
connector config + the slim `Integration` trait), ADR 0053 (session profiles), ADR
0047 (the stateless coordinator — Postgres is the authority), ADR 0031 (per-user
auth + KEK-sealed secrets), and ADR 0006 (the host-agent egress proxy).

## Context

A session's configuration is **scattered across three places** today:

1. **The image manifest** (`ImageManifest`, `engram-core/src/types/image.rs`) declares
   `secrets` (the `SecretSchema` map + per-secret `allow_hosts`), `secret_mode`
   (`Broker`/`Literal`), `network` (the `NetworkPolicy` egress allow-list), and `git`
   (the forge binding) — all of which are session *policy*, not image identity.
2. **The profile** (ADR 0053) carries `env_vars`, `skills`, and `capabilities`.
3. **Connectors** (ADR 0056) live as static JSON files in the orchestrator
   (`orchestrator/src/connectors/*.json`), and integration credentials are provisioned
   **out-of-band**: Datadog's inject key in GCP Secret Manager, the GitHub App key in
   coordinator boot env (`--github-app-id` / `ENGRAM_GITHUB_APP_PRIVATE_KEY`).

`build_egress_policy` (`session_boot.rs`) stitches the per-session egress policy from
all three. Two structural problems follow:

- **The manifest-sourced halves (network + secrets) are re-derived from the manifest on
  every resume** — a latent correctness gap: editing a manifest silently changes a live
  session's reachability on its next resume. Integrations, by contrast, are already
  profile-compiled and PG-persisted (the `session_integration_policy` blob, migration
  0071), so they survive resume coherently.
- **Adding an integration is not a product action.** It means dropping a JSON file in the
  orchestrator and redeploying, plus provisioning a credential out-of-band.

## Decision

### 1. The profile is the single session-policy object

`network`, `secrets`, `secret_mode`, and the git-forge binding **move off the image
manifest onto the profile** (a clean break — images re-bake; 0 users). The manifest keeps
only what is image-intrinsic: `name`/`description`, `env` defaults, `workdir`,
`resources`, `harness`, `warm`. Git-forge binding is subsumed by `github:*` profile
capabilities (ADR 0056), retiring the manifest `[git]`/`GitConfig` block entirely.

The orchestrator compiles a profile into **one `SessionPolicy`** —
`{ network, secrets (schema: ref + env-var + mode + allow_hosts), injects, observes }` —
shipped on `CreateSession`, persisted in a `session_policy` table (superseding 0071), and
re-read on resume. `build_egress_policy` builds the *entire* egress policy from the
persisted `SessionPolicy` + the secret store — **never the manifest**. This also closes
the resume gap (the policy is fixed at create, immune to later manifest/profile edits).

### 2. One secret-value store, as a `SecretStore` *backend* (not a parallel store)

`SecretStore` (`traits/secrets.rs`) is already the pluggable, multi-backend secret seam.
We add a first-party **org-secret store** — KEK-sealed rows in the coordinator's Postgres,
admin-managed via the UI — and **compose** it in front of the deployment backend via a
`LayeredSecretStore` (org store first, deployment fallback). Profile secrets, integration
inject credentials, *and* the GitHub App mint key all resolve through the same
`state.services.secrets` — **unchanged call sites**. Self-hosters keep backing other
secrets with GCP SM / Vault.

This is a deliberate correction of an earlier sketch (a parallel `OrgSecretStore` type):
forking one clean abstraction is exactly the "code-on-code" the repo avoids. Resolution
stays coordinator-side and PG-authoritative (ADR 0047) — required, because resume re-reads
the persisted policy and re-resolves with no orchestrator round-trip.

**Storage follows the established precedent** for coordinator-owned sealed PG rows
(`session_broker_tokens`, `mount_catalog`): `MetadataStore` methods + an `engram-postgres`
impl, *not* a standalone crate. The read side is a thin `SecretStore` adapter over
`meta` + `kek`; the write side (the admin gRPC) reuses the `meta` + `kek` already in
`Services`. `LayeredSecretStore` is a generic combinator in `engram-core`.

### 3. Integrations become a first-class, runtime-managed feature

- The **connector catalog** moves from files-on-disk to a DB-backed table; built-ins
  (`github`/`datadog`) become read-only seeds; new connectors are admin-authored.
  `parseConnector` becomes an **admin-trust validation boundary** (was a dev-error guard).
- A `/settings/integrations` admin UI with **two credential planes**: **Plane A (mint)**
  e.g. GitHub — a data-driven config form generated from a coordinator **mint-kind
  registry** (`MintKindDescriptor`); **Plane B (inject)** e.g. Datadog/Sentry — a
  declarative form-builder authoring the whole connector + a write-only credential
  (→ org secret).
- The profile editor's capability field becomes a **catalog-driven picker**.

The "add integration" write fans out (mirroring the skills-upload precedent,
`rpc/mount-catalog.ts`): connector config → orchestrator DB; sealed credential →
coordinator org-secret store (proxied over app-gRPC, sealed at the coordinator).

### 4. Scope

**Single-tenant** (one global "org", not multi-tenant). **GraphQL deferred** (REST/HTTP
only this pass; the `protocol` axis is additive when it lands — see ADR 0056 §3). The mint
*logic* stays bespoke Rust (the registry is the *frame* around minting, not the generic
config-driven mint ADR 0056 §9 deferred).

## Phasing (each phase = one PR on its own worktree; linear stack)

`main → A1 → B1 → B2 → C1 → C2 → C3 → C4 → D1 → D2`

- **A1** — Org-secret store: `MetadataStore` org-secret methods + `engram-postgres` impl
  (migration `0072_org_secrets.sql`, `pg_notify('org_secret_changed')`), a `SecretStore`
  read-adapter, `LayeredSecretStore` composed into the coordinator's secret store, and the
  admin `OrgSecretService` app-gRPC + orchestrator proxy. *(this PR; ADR Proposed)*
- **B1** — Profile gains `network` + `secrets` (Drizzle + proto + editor; additive).
- **B2** — `SessionPolicy` cutover, split into two PRs to keep the high-blast-radius
  coordinator rewrite (which needs dev-vm FC/resume validation) isolated and reviewable:
  - **B2a** *(additive; deploy-safe)*: the policy (`IntegrationPolicy`) carries `network` +
    `secrets`; the orchestrator compiles them from the profile and ships them. The
    coordinator doesn't consume them yet → zero behavior change. The type keeps its
    `IntegrationPolicy` name + the `integration_policy_json` wire field — the cosmetic rename
    to `SessionPolicy` is a deferred follow-up (cf. ADR 0056's deferred `ForgeOp`→
    `IntegrationOp` rename).
  - **B2b** *(the clean break)*: the coordinator sources network + secrets **solely** from
    the policy — no manifest fallback — on both create and resume (per-secret resolution:
    literal→env, broker→placeholder+egress entry; resume re-reads the persisted policy via
    `load_session_policy`). A session with no policy gets deny-all + no secrets (the secure
    default). **Strip** `secrets`/`secret_mode`/`network` from `ImageManifest` (drop
    `deny_unknown_fields` so pre-strip stored manifests still parse during rollout). Edges:
    `cold_boot_spec` takes an explicit `network` — base-snapshot **capture** uses allow-all
    (trusted build step; the snapshot is network-agnostic), disk-only **recovery** rebuilds
    from the session's persisted policy; the session-level `SessionEgressPolicy.secret_mode`
    is now vestigial (per-entry substitution). Policy-less callers updated: `engram-cli`
    sends an allow-all policy; e2e/lifecycle creates run deny-all (they don't assert egress).
    Operational: default/dogfood **profiles must be seeded** (≥ `api.anthropic.com`) before
    this rolls, and images re-bake. The `IntegrationPolicy`→`SessionPolicy` rename stays
    deferred (cosmetic).
- **C1** — DB-back the connector catalog (orchestrator-only; built-ins as read-only seeds).
- **C2** — Mint-kind registry + GitHub cred migration (creds sourced from the org store;
  retire `--git-forge`/`--github-app-*`).
- **C3** — `IntegrationService` proto + orchestrator service (connector CRUD, `ListMintKinds`,
  org-secret put/list/delete — all proxied).
- **C4** — Web `/settings/integrations` UI (Plane A/B).
- **D1** — Catalog-driven capability picker in the profile editor.
- **D2** — Retire `[git]`/`GitConfig` into capabilities; **ADR Accepted**.

## Consequences and risks

- **Resume correctness improves**: persisting the full `SessionPolicy` removes today's
  silent manifest-edit-changes-live-session behavior. New wire fields are
  `#[serde(default)]` so in-flight policies decode.
- **Image re-bake migration**: stripping the manifest is a clean break — all images re-bake
  and default/dogfood profiles must be seeded with their network + secrets + `github:*`
  caps before sessions can reach anything (deny by default).
- **GitHub cred migration (one-time)**: after C2, an admin enters the App ID + PEM once via
  the UI; the `ENGRAM_GITHUB_APP_*` boot env is dropped. The empty-caps default-scope
  fallback keeps existing profiles working.
- **`parseConnector` becomes security-relevant** (admin-trust): an uploaded connector can
  open egress + inject org secrets. Hardened + tested. Same trust level as setting
  `env_vars` / uploading skills today.
- **Secret-name collision**: PG-first layering means an admin-entered name could shadow a
  deployment-backed secret; guarded by naming discipline (the org store holds only what
  admins put there).
- **Backwards-incompatible wire/schema changes** are acceptable — 0 users; clean breaks.

## Alternatives considered

- **A parallel `OrgSecretStore` type** (integration secrets resolve from it, not
  `SecretStore`). Rejected: forks one clean abstraction; locks image secrets out of the
  UI-managed store forever. The backend-+-compose shape subsumes both.
- **Manifest stays the source of network/secrets as profile-creation defaults.** Rejected
  for simplicity: the profile is the single runtime source; defaults are seeded by hand.
- **A standalone `engram-secrets-pg` crate owning the table.** Rejected: coordinator-owned
  sealed PG rows already live in `engram-postgres` via `MetadataStore` (broker tokens,
  catalog); a 4-method table + a thin read-adapter doesn't meet the "deserves its own
  crate" bar and would force a `Services` field + connection duplication.
