# ADR 0063: harness.toml descriptor + harness-derived env wiring

Status: 2026-06-30 — **Accepted** (control-plane half shipped; see the commit chain in §Phasing).
Builds on **ADR 0062** (per-session harness selection — the infra half + the harness catalog
this ADR consumes), **ADR 0051** (the TypeScript orchestration tier — the coordinator stays
harness-agnostic; the orchestrator owns harness-specific naming), **ADR 0053** (session
profiles), and **ADR 0057** (unified session policy + org secrets — the vehicle for the
programmatic credential). This is the control-plane / UX half; ADR 0062 is the infra half.

## Context

With the harness chosen per session (ADR 0062), the control plane needs to (a) let a profile
declare a *default* harness, overridable at session create, and (b) stop hardcoding the Claude
harness's environment contract everywhere.

Today the harness's env wiring is hardcoded to Claude:

- `CLAUDE_OAUTH_ENV_VAR = "CLAUDE_CODE_OAUTH_TOKEN"` is a literal constant in
  `orchestrator/src/db/user-secrets.ts`, referenced by the injection path
  (`task-create.ts`), the storage routes (`routes/me.ts`, `POST /me/claude-token`), and the web
  (`TokensPanel.tsx` "Claude Code", `StartScreen.tsx` token nudge).
- The model is a free-form `ANTHROPIC_MODEL` string a profile author types into `env_vars`
  (`EnvVarsEditor.tsx` even hints "Set the model here, e.g. `ANTHROPIC_MODEL`. There is no
  separate model field."). There is no model enum, no effort control, and nothing harness-aware.

A second harness (OpenCode, say) has a *different* token env var, a *different* model env var
(possibly two vars per model — a provider/base-URL var and a model-name var), and possibly a
different effort knob. None of that can be expressed today.

We want a **harness-agnostic descriptor** — `harness.toml` — that each harness ships, declaring
its env contract, and we want **everything currently hardcoded to be derived from it**.

## Decision

### 1. The `harness.toml` descriptor

Each harness ships a `harness.toml` inside its bundle (replacing today's `artifact.toml`); ADR
0062's catalog stores it so the orchestrator/web can read it without touching the squashfs. The
schema uses **per-option `env` maps** so it is truly harness-agnostic — each model/effort option
declares the exact env var(s) it sets, which the orchestrator dict-merges with zero
harness-specific branching:

```toml
name  = "claude"            # stable id; == catalog key == CreateSessionRequest.harness
label = "Claude Code"
description = "Anthropic's Claude Code agent…"   # optional; admin Harnesses tab (§6)

[auth]
org_env  = "ANTHROPIC_API_KEY"        # programmatic credential (org/automation)
user_env = "CLAUDE_CODE_OAUTH_TOKEN"  # optional; human/interactive credential

[[models]]
id = "opus"
label = "Claude Opus 4.8"
default = true
env = { ANTHROPIC_MODEL = "claude-opus-4-8" }

[[models]]
id = "sonnet"
env = { ANTHROPIC_MODEL = "claude-sonnet-4-6" }

[[effort]]                  # optional
id = "high"
env = { MAX_THINKING_TOKENS = "32000" }
```

Why per-option `env` maps rather than a flatter `model_env = "ANTHROPIC_MODEL"` + values-list:
the flat shape cannot express a harness whose model selection needs *more than one* env var
(OpenCode's provider+model pair) or whose effort is a different var/flag entirely. The repeated
env-var name lives once, in the harness's own shipped descriptor; the orchestrator's mapping
stays a pure dict-merge — the ADR 0051 north star (orchestrator maps ids→env, coordinator injects
verbatim). `org_env`/`user_env` stay single strings (a credential is one var).

Parsed by a new `engram-core::types::harness::HarnessDescriptor` (`HarnessAuth`, `HarnessOption`,
helpers `model(id)`/`effort(id)`/`default_model()`/`default_effort()`, env-name validation
against `^[A-Za-z_][A-Za-z0-9_]*$`, `deny_unknown_fields`). `harness.toml` *also* carries the
coordinator-internal **launch contract** — `exec` (the entry path within the harness's catalog
subtree, default `harness`) and `args` (ADR 0062 §5) — so a single descriptor file fully
describes a harness. The proto `HarnessDescriptor` projection (for the orchestrator/web) omits
`exec`/`args` (they are coordinator-only) and carries
`name`/`label`/`description`/`auth`/`models`/`effort`; all `env` values are **config, never
secrets**, so they are safe on the wire. The list projection `HarnessSummary` additionally carries
a `built_in` flag (true for the platform built-in `claude`, which rides the fleet stamp + the
coordinator-embedded descriptor rather than a catalog row) so the admin tab (§6) can hide the
delete affordance for built-ins.

### 2. Profiles carry a harness + default model/effort

The profile gains a **required** `harness` column + nullable `model`/`effort`. **A profile always
names a concrete harness** — the original "null `harness` ⇒ inherit the deployment-default
harness" semantics were **superseded** (a real dropdown of all harnesses, built-in ∪ custom, is
clearer than a magic inherit value; existing rows were backfilled to `claude` and the column made
NOT NULL — drizzle `0011`). null `model`/`effort` ⇒ the descriptor's `default_model`/`default_effort`.
The orchestrator validates at create/update (`assertHarnessValid`, mirroring
`assertImageEnabled`/`assertSkillsValid`): the harness must exist in the catalog and a set
model/effort id must exist in that harness's descriptor enum. Surfaced member-visible on the
`Profile` proto and rendered as dropdowns in the profile editor (a `useHarnessCatalog()` hook
mirrors `useEnabledImages()`); the editor defaults a new profile to the first registered harness.

**Web surface (this was a shipped correctness bug).** The dashboard reads the catalog through the
orchestrator, which fronts all web traffic (ADR 0051). `HarnessCatalogService` was omitted from the
orchestrator passthrough `SURFACE`, so `useHarnessCatalog` silently returned nothing — the
harness/model/effort selectors in *both* the profile editor and the launch screen were dead (empty
dropdowns; the launch override control rendered nothing). The read methods (`ListHarnesses`/
`GetHarness`) are now forwarded member-readable (the catalog is config, not secrets — a new
`Harness` ability subject). The write methods (`RegisterHarness`/`DeleteHarness`) stay
orchestrator-internal until the admin Harnesses tab forwards them admin-only.

### 3. Session-create override

`CreateTaskRequest` gains optional `harness`/`model`/`effort`. The compile path
(`task-create.ts::compileSessionCreateInput`) resolves the **effective** values
(`override ?? profile ?? deployment-default` for harness; `override ?? profile ??
descriptor.default_*` for model/effort), validates against the catalog, and sets
`CreateSessionRequest.harness` (consumed by ADR 0062's coordinator to mount the bundle). This is
the "Claude Code for planning, OpenCode + small model for execution" UX: same profile, per-launch
override.

### 4. Credential injection — strict by run type

The descriptor's two credential slots map to engrams' two credential tiers, chosen **strictly by
run type** (mutually exclusive, no cross-fallback):

- **Human / interactive** (chat UI, `task.type === "chat"`): inject the user's token under
  `user_env`. The user-token store (`user_session_secrets`) is **already keyed by `(userId,
  envVarName)`** — multi-harness user tokens (a Claude OAuth token *and*, say, an OpenAI key) need
  **no new table**, only a non-hardcoded env-var name. Storage routes generalize from
  `/me/claude-token` to `/me/harness-tokens[/:harness]` (resolving `user_env` from the
  descriptor), with a thin `/me/claude-token` shim kept for one release so
  `principal.has_claude_token` keeps working until the web migrates. The injection path resolves
  the name from `descriptor.auth.user_env` instead of the deleted `CLAUDE_OAUTH_ENV_VAR` constant.

- **Programmatic** (service-account principals — cron, CI/API keys): inject the **org**
  credential under `org_env`. **Critically, org-secret *values* never leave the coordinator (ADR
  0057)** — so the orchestrator cannot read `org_env` and place it in `harness_env`. Instead it
  appends an `IntegrationSecretJson { secret_ref: org_env, env_var: org_env, mode: "literal" }` to
  the compiled session policy, which ships in `CreateSessionRequest.integration_policy_json` and is
  resolved **host-side** by `resolve_policy_secrets` exactly like profile secrets. Convention: the
  org secret is named after the env var (an admin creates an org secret `ANTHROPIC_API_KEY` via the
  existing org-secret UI); an unresolvable ref is skipped + warn-logged and the session still boots.

The single programmatic discriminator guarantees a session never gets both credentials.

*Amended 2026-07-14*: the discriminator is the **principal**, not `task.type`. The original
`type === "chat"` gate booted Slack sessions credential-less ("Not logged in", session
`e721311e`) even though the Slack mention was email-matched to a real engrams user whose token
was on file and whose profile set `include_user_tokens`. A human owner now rides their own token
on every surface (chat UI, Slack thread, …); only service-account creators
(`ownerIsServiceAccount` → `programmatic`) take the `org_env` path. Cron remains programmatic by
construction (service-account principal), so the original cron/CI behavior is unchanged.

### 5. Model + effort → env mapping

After resolving the effective harness descriptor + model/effort ids, the compile path
dict-merges `descriptor.model(id).env` and `descriptor.effort(id).env` into the harness-env map.
**Precedence:** user-token / org-inject < CLI dummy < `profile.envVars` < **model env < effort
env** < trigger extras — the explicit picker wins over any stale `ANTHROPIC_MODEL` left in a
profile's `env_vars` (the migration intent; `EnvVarsEditor`'s "set the model here" hint is
removed, since there is a model field now). The coordinator stays agnostic: one flat
`harness_env`, injected verbatim and persisted for resume.

### 6. Admin Harnesses tab

A new admin-only `/settings/harnesses` tab (mirroring the org-secret / integrations panels) is the
operator surface for the catalog. It is the home for the three things an admin needs that the
profile editor (which only *picks* a harness) does not cover:

- **See what's available.** Each harness from `ListHarnesses` renders its `label`, `name`,
  `description`, the model/effort options it offers, and a `built-in` badge for the platform
  built-in.
- **Configure the programmatic credential.** Each harness's `auth.org_env` is surfaced with a
  "configured / not set" status, cross-referenced against the org-secret store
  (`useOrgSecrets`), and a **Set / Replace** action that writes the value via the existing
  `OrgSecretService.PutSecret` (keyed on the `org_env` name — the convention §4 relies on). This
  closes the loop: §4's programmatic inject resolves an org secret named after `org_env`, and this
  is where an admin actually sets it. The `auth.user_env` is shown read-only with a pointer to
  *My Tokens* (the per-user, human-run credential — never an admin concern).
- **Register / delete custom harnesses.** A "Register harness" action (`name` + OCI `oci_ref` →
  `RegisterHarness`) and a per-row delete for non-built-ins (`DeleteHarness`). The orchestrator
  forwards these two write methods **admin-only** (`manage("all")`) — the read methods stay
  member-readable (§2's note). Built-ins are not deletable (the coordinator rejects it; the tab
  hides the affordance via the `built_in` flag).

No new orchestrator service: the tab composes the existing `HarnessCatalogService` (now fully
forwarded) and `OrgSecretService`. The only backend change beyond exposing the write methods is the
two descriptor projection additions in §1 (`description`, `built_in`).

## Phasing (living checklist; each phase = one worktree + one PR)

- [x] **A1** (shared with ADR 0062) — `harness.toml` parse type + proto `HarnessDescriptor`.
- [x] **B1** — profile default harness/model/effort (migration, schema, proto, catalog
  validation, web editor). *(#484, `9819e404`)*
- [x] **B2** — session-create override (`CreateTaskRequest`/`CreateSessionRequest.harness`,
  compile threading, web pickers; defaults `harness = "claude"` so the orchestrator always
  sends a selection). *(#486, `160b5343`)*
- [x] **B3** — de-hardcode `user_env` (`/me/harness-tokens`, injection swap, TokensPanel;
  killed the `CLAUDE_CODE_OAUTH_TOKEN` concept). *(#488, `022f4c55`)*
- [x] **B4/B5** — model/effort→env merge + precedence + programmatic `org_env` inject (strict
  by run type). *(#490, `dc0d17ea`)*
- [x] **UI completion (PR A)** — expose `HarnessCatalogService` (read) through the orchestrator
  passthrough so the selectors actually populate; profile harness is a concrete dropdown (no
  "inherit"); Model/Effort enabled; launch override renders. Backfill migration `0011`.
- [x] **Admin Harnesses tab (PR B)** — `/settings/harnesses`: display + per-harness org-secret
  config (`PutSecret` keyed on `org_env`) + register/delete of custom harnesses. Proj additions
  `HarnessSummary.built_in` + `HarnessDescriptor.description`; the two write methods forwarded
  admin-only. *(§6)*

The B-stack was authored on ADR 0062's pre-A5 A-stack, then rebased onto `main` after ADR 0062
landed (incl. the A5 built-in-harness redesign) — B1/B2 validate against `ListHarnesses`
(builtin ∪ catalog), so the now-built-in `claude` resolves with no catalog row.

## Consequences

- The Claude-specific env contract is **data, not code** — a second harness ships its own
  `harness.toml` and the orchestrator/web work with no harness-specific branches.
- Model + effort become first-class, validated, per-session-overridable selections instead of
  free-form env strings.
- The org/user credential split is explicit and respects the ADR 0057 boundary (org-secret
  values stay coordinator-side).
- The coordinator remains harness-agnostic (ADR 0051): it mounts the named bundle (ADR 0062) and
  injects the orchestrator-computed env verbatim.

## Alternatives considered

- **Flat `model_env` + values list.** Rejected — cannot express multi-var model selection
  (OpenCode) or non-env effort knobs; see §1.
- **Keep model as a free-form `ANTHROPIC_MODEL` env var.** Rejected — no validation, no picker,
  no per-harness model enum, and re-couples the UI to Claude.
- **Read `org_env` in the orchestrator and inject via `harness_env`.** Rejected — violates ADR
  0057 (org-secret values must not cross to the orchestrator tier); the integration-policy
  secret-inject path resolves it host-side instead.
- **A per-harness user-token table.** Unnecessary — `user_session_secrets` is already
  `(userId, envVarName)`-keyed.
