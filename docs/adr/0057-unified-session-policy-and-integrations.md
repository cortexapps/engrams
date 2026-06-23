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

`main → A1 → B1 → B2 → C0 → C1 → C2 → C3 → C4 → D1 → D2`

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
- **C0** — Org-secrets management UI (web-only): a `/settings/secrets` admin panel
  (list / add / replace / delete) over the A1 `OrgSecretService` proxy — so org secrets are
  UI-manageable ahead of the integration catalog (the profile secret-ref picker already reads
  their names). Write-only credential surface: values are sealed coordinator-side and never
  echoed (List is metadata only). Pulled in before C1 so admins can author the secrets that
  profiles + connectors reference. (Distinct from C4's connector UI, but a sibling under it.)
- **C1** — DB-back the connector catalog (orchestrator-only; built-ins as read-only seeds).
  *As built:* a `connector` Drizzle table (migration `0007_connector_catalog.sql`) holds only
  admin-authored connectors; `github`/`datadog` stay as file seeds (read-only). The sync
  `connectorRegistry()` (file seeds) stays as the pure fns' default + test fixture; a new async
  `loadRegistry(source)` = seeds ∪ DB, validated + cached (`invalidateRegistry()` for C3), with
  **built-in precedence** (a custom row can't shadow a seed), per-row skip-on-invalid, and
  degrade-to-seeds if the DB read fails. `compileIntegrationPolicy`/`grantsCapability` stay sync;
  the two prod callers (`tasks.ts` create, `profiles.ts` capability-save) `await loadRegistry`.
  `parseConnector` is hardened into the admin-trust boundary (host wildcard/scheme/path checks,
  HTTP header-name + `{}`-template + no-CRLF validation, provider-id + count bounds).
- **C2** — Mint-kind registry + GitHub App key sourced from the org store.
  *As built:* `MintKindDescriptor`/`MintFieldSchema`/`MintFieldKind`/`ResolvedFields` in
  `engram-core`; `engram_git_github::github_app_descriptor()`; `mint_kind_registry()` in the
  coordinator. The `IntegrationBroker` builds engines **lazily from the composed `SecretStore`**
  keyed `<kind>.<field>`, with a version-keyed cache that `pg_listener` invalidates on
  `org_secret_changed` (A1 already `NOTIFY`s on write). `forge.rs` + `inject_forge_env` resolve the
  engine async. **Boot-env kept as fallback, not retired**: the `--git-forge`/`--github-app-*`
  values ride as a `StaticSecretStore` layered **org store → boot-env fallback → deployment
  backend**. The fallback sits *ahead of* the deployment backend deliberately — GCP SM can't
  resolve the synthetic `github_app.*` name (empty repo + a `.` → an invalid secret id → a `400`
  the `LayeredSecretStore` would propagate *before* reaching the fallback), so layering it first
  keeps minting working unchanged in prod. The org store still wins, so dropping the boot env is
  the deliberate **post-C4 cutover** (you can't enter the PEM in a UI that doesn't exist yet).
  `ListMintKinds` gRPC is **regrouped into C3** (with the `IntegrationService` proto + proxy) to
  avoid a one-off coordinator proto with no caller.
- **C3** — Connector catalog CRUD + the mint-kind registry over gRPC.
  *As built:* two services. **`MintService`** (coordinator `mint.proto`, build.rs-compiled like
  `OrgSecretService`): `ListMintKinds` maps the coordinator's `mint_kind_registry()` → proto; an
  admin-gated orchestrator proxy (`rpc/mint.ts`) + web hooks expose it for the Plane-A form.
  **`IntegrationService`** (orchestrator-native `integration.proto`, NOT build.rs-compiled, like
  `ProfileService`): `ListConnectors` (built-in file seeds read-only ∪ DB) / `UpsertConnector` /
  `DeleteConnector` over the C1 `connector` table — admin-gated, `config_json` validated by
  `parseConnector` (the admin-trust boundary) + `invalidateRegistry()` on write; built-in providers
  are rejected. Deviation from the plan's "IntegrationService.ListMintKinds": `ListMintKinds` lives
  on its own `MintService` (coordinator-sourced, proxied) to avoid cross-proto message imports.
  Org-secret put/list/delete already shipped in C0 (the `OrgSecretService` proxy + panel), so they
  are not re-done here. The `connectors` dep in `tasks.ts`/`profiles.ts` narrows to the read-only
  `CustomConnectorSource`; the full-CRUD `ConnectorStore` backs `IntegrationService`.
- **C4** — Web `/settings/integrations` UI (Plane A/B).
  *As built:* `useIntegrations.ts` (connectors CRUD + `useMintKinds`); `IntegrationsPanel` at
  `/settings/integrations` (admin) + a Settings → Org nav entry. **Plane A** is a data-driven mint
  form from `ListMintKinds` — each field is written as an org secret named `<kind>.<field>`, so the
  GitHub App ID + PEM land exactly where C2 resolves them (this is the cred-migration entry point).
  **Plane B** is a connector form-builder (provider / hosts / header / secret-ref / template +
  repeatable operation rows) → `UpsertConnector` (server-validated by `parseConnector`) plus the
  write-only credential → org secret under the ref. Built-in connectors are read-only.
- **D1** — Catalog-driven capability picker in the profile editor.
- **D2** — Retire `[git]`/`GitConfig` into capabilities; **ADR Accepted**.

## Consequences and risks

- **Resume correctness improves**: persisting the full `SessionPolicy` removes today's
  silent manifest-edit-changes-live-session behavior. New wire fields are
  `#[serde(default)]` so in-flight policies decode.
- **Image re-bake migration**: stripping the manifest is a clean break — all images re-bake
  and default/dogfood profiles must be seeded with their network + secrets + `github:*`
  caps before sessions can reach anything (deny by default).
- **GitHub cred migration (one-time)**: C2 makes the App key resolvable from the org store but
  keeps the boot-env values as a fallback layer, so nothing breaks mid-stack. The actual cutover
  is **post-C4** (once the UI exists): an admin enters the App ID + PEM once via `/settings/integrations`
  (org store wins over the fallback), then `ENGRAM_GITHUB_APP_*` is dropped from the coord Deployment.
  The empty-caps default-scope fallback keeps existing profiles working.
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
