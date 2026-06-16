# ADR 0052: Session profiles — admin-curated starting points replace the free-form new-session form

Status: 2026-06-16 — **Proposed.** No code yet. Builds on ADR 0051 (the
TypeScript orchestration tier: orchestrator-native `TaskService`, CASL
authorization, the orchestrator's own Postgres, and the generic control-plane
passthrough) and ADR 0036 (image enablement / enable jobs). It changes the
shape of `CreateTask` and the web "new session" flow defined there.

## Context

Today a task — and therefore a session — is created from free-form inputs.
`CreateTaskRequest` carries `image_uri` and an optional `prompt`; the web
`NewSessionDialog` surfaces an image dropdown, a mode toggle, and an optional
prompt field. Two problems follow:

1. **Every technical choice is pushed onto the user at create time.** Which
   image, which model, which environment, whether the session should carry the
   user's Claude credentials — all of it is decided per-session, by whoever is
   starting the session, every time. Most of these are not choices an end user
   should be making, and the defaults drift by habit rather than policy.
2. **There is no governance surface.** Any member can point a session at any
   enabled image with any prompt and (today) get their Claude token injected.
   There is nowhere for an admin to say "these are the three ways to start a
   session here, and this is what each one is allowed to carry."

We want admins to curate a small set of named, opinionated starting points — a
**profile** — and have users simply pick one. A profile bundles the image, the
mode, a locked starting prompt, a model and environment, and an explicit
decision about whether the user's Claude token is injected. The new-session
flow becomes "pick a profile," not "fill in a form." Profiles are also the
natural future home for capability scoping (enabling/disabling specific MCP
servers and tools per profile) — explicitly **not** built here, but the model
is shaped to grow into it.

## Decision

### 1. What a profile is

A **profile** is an admin-owned, named bundle of the inputs that today are
typed into the new-session form. It is orchestrator data — it lives entirely in
the orchestration tier (ADR 0051 §2.2/§10), in the orchestrator's Postgres.

`profiles` (orchestrator DB, Drizzle):

```
id                  uuid    pk
name                text                  -- shown in the picker
description         text                  -- shown in the picker
icon                text                  -- a Phosphor icon name
image_id            uuid                  -- logical ref to the coordinator's enabled_images.id (§3)
mode                text                  -- "agent" | "shell"
starting_prompt     text    null          -- agent mode only; locked, admin-set (§5)
include_user_tokens boolean               -- inject the user's Claude token (§5)
env_vars            jsonb                 -- { KEY: VALUE }; admin-only; the model lives here (§5)
sort_order          int                   -- picker ordering
created_at          timestamptz
updated_at          timestamptz
deleted_at          timestamptz null      -- null = active; soft delete only (§4)
```

Deliberate omissions, each decided rather than overlooked:

- **No `model` column.** The model is just one entry in `env_vars` (the harness
  reads it from the environment). A first-class column would duplicate that and
  invite a parallel plumbing path. One mechanism, not two.
- **No `created_by_user_id`.** Profiles are global org configuration, not
  user-owned content; authorship attribution buys nothing here.
- **No `is_active` flag.** `deleted_at IS NULL` *is* the active predicate; a
  second boolean would be a redundant, drift-prone state.

### 2. Where it lives, and the `task_session` link

Profiles are an orchestration concern: they exist only to assemble the inputs
the orchestrator already hands to the control plane's `CreateSession`. The
control plane has no notion of a profile and never will — it keeps taking
`image_uri` / `mode` / `prompt` / `secrets` / `harness_env` (ADR 0051 §2.1).

We record which profile created a session on the existing pointer table:

```
task_session.profile_id  uuid  null  references profiles(id)
```

- This is a **real intra-DB foreign key** — both tables live in the
  orchestrator's database, so referential integrity is free here (unlike the
  image reference in §3).
- It is **nullable** for the pre-feature sessions that predate profiles, and
  for any future out-of-band session created without one.
- Because profiles are only ever soft-deleted (§4), the FK target always
  exists; `ON DELETE` behavior is therefore moot. The column is a durable
  record of *which profile started this session*, not a live pointer — see §5
  on why edits don't reach back into running sessions.

### 3. The image reference: logical, not a database FK

`enabled_images` lives in the **coordinator's** database (Rust, sqlx,
`deploy/migrations`), not the orchestrator's. The two tiers never share tables
(ADR 0051 §10). So a profile's reference to its image **cannot be a real
Postgres foreign key** — a hard cross-tier FK would couple two independent
migration pipelines and rot the boundary the whole architecture is built on.

The profile therefore stores `image_id` (uuid) as a **logical** reference to
`enabled_images.id`, with integrity enforced in application code at two points:

- **On profile create/update**, the orchestrator validates the `image_id`
  against the coordinator's catalog (`ImageService.ListEnabledImages`) and
  rejects an id that is absent or disabled.
- **At session-create time**, the orchestrator resolves `image_id` → the
  image's *current* `image_uri` and passes that to `CreateSession`. Storing the
  id (not the uri) means image refreshes / re-tags flow through automatically:
  the profile keeps pointing at the same logical image even as its uri changes.

We chose the id-with-resolution shape over storing a denormalized `image_uri`
snapshot (which would drift on refresh) or storing both (more bookkeeping than
the indirection is worth).

**Restrict on disable.** An enabled image may not be disabled while any active
profile references it. The coordinator already 409s `DisableImage` when live
*sessions* reference an image, but it knows nothing about orchestrator
profiles — so the profile-level guard lives in the **orchestrator's passthrough
layer**: before forwarding `DisableImage` to the coordinator, the orchestrator
checks `profiles WHERE image_id = ? AND deleted_at IS NULL` and, if any exist,
rejects with `failed_precondition` and the names of the blocking profiles so
the admin knows what to repoint or archive. The coordinator's independent
session-level guard is unchanged; the two guards stack.

### 4. Soft delete only, with an admin archive

Profiles are **never hard-deleted**. `DeleteProfile` sets `deleted_at`. This
keeps every historical `task_session.profile_id` resolvable forever — "which
profile started this session, six months ago, even though we retired it?" stays
answerable.

- Active profiles (`deleted_at IS NULL`) are what the picker and the default
  listing show.
- Soft-deleted profiles remain visible **to admins only**, in an "Archived"
  section of the admin UI — for context on old sessions and for a future
  restore affordance. Non-admins never see archived profiles.

### 5. Creating a session through a profile

`CreateTaskRequest` changes shape: it **drops `image_uri` and `prompt`** and
**gains `profile_id`**. The client no longer sends the technical inputs; it
sends the profile the user picked, and the orchestrator's `CreateTask` handler
(orchestrator-native, ADR 0051 §3) resolves everything server-side:

1. Load the profile by id; reject if missing or soft-deleted.
2. Resolve `image_id` → current `image_uri` (§3); reject if the image is no
   longer enabled (defense in depth behind the §3 restrict guard).
3. Set the upstream `mode` from the profile (mapping below) and, for agent
   mode, the locked `starting_prompt` as the session's first prompt.
4. **Assemble `harness_env`** in this precedence (lowest → highest):
   1. the user's Claude token (`CLAUDE_CODE_OAUTH_TOKEN`, resolved from the
      sealed secret store, ADR 0051 §2.3) — **only if** `include_user_tokens`
      is true;
   2. the profile's `env_vars` — **override** anything below, including the
      token key if an admin set it explicitly. Admin intent wins over the user
      token by design.
5. Call the control plane's `CreateSession`, then write the `task` and
   `task_session` rows, recording `profile_id` on the latter.

**`mode` mapping.** A profile's `mode` is `"agent"` or `"shell"` in
product terms; the control plane's `CreateSessionRequest.mode` is
`"agent"` or `"dev_vm"`. Profile `"shell"` maps to upstream `"dev_vm"`.
For `shell` profiles, `starting_prompt` is not used (a dev VM has no agent
turn) and `include_user_tokens` governs only whether the token reaches the
environment.

**`include_user_tokens` is a capability grant, not a convenience toggle.** It
is the admin's explicit declaration that sessions started from this profile are
permitted to carry the user's Claude credentials. For an untrusted or
externally-facing image, an admin leaves it off and the token never enters the
sandbox. Today the only such token is the Claude OAuth token; the boolean is
named and modeled to generalize, but we do not enumerate token *types* yet
(YAGNI, §7).

**Edits don't reach running sessions.** A session is created from a *snapshot*
of the profile's values at create time; `task_session.profile_id` is a
historical record, not a live binding. Changing a profile's model or prompt
later affects only sessions created after the change. This falls out of the
design for free — nothing re-reads the profile after creation.

### 6. ProfileService and the picker

A new **`ProfileService`** — orchestrator-native, defined in
`engram/app/v1/profile.proto`, implemented directly on the Connect router
beside `TaskService` (ADR 0051 §3), never a passthrough (it is pure
orchestrator data):

| RPC | Caller | Notes |
|-----|--------|-------|
| `ListProfiles(include_archived?)` | member | active only by default; `include_archived` requires admin (§4); `env_vars` stripped for non-admins |
| `GetProfile(id)` | member | same field-filtering |
| `CreateProfile(...)` | **admin** | validates `image_id` against the catalog (§3) |
| `UpdateProfile(...)` | **admin** | |
| `DeleteProfile(id)` | **admin** | soft delete (§4) |

**Authorization** uses the same CASL machinery as everything else (ADR 0051
§6). The three mutations and the `include_archived` listing map to
`manage / all` (admin). `ListProfiles` / `GetProfile` are readable by members —
users must see the menu they pick from. Because these are native handlers (not
the generic passthrough), the **field-level filtering happens in the handler**:
for a non-admin caller the handler omits `env_vars` from the response. This is
one message type, conditionally populated — not two message shapes.
`include_user_tokens` stays visible to everyone: knowing that a profile will
carry your Claude token is useful context for choosing between profiles, not a
secret. Non-admins thus see the full profile config *except* `env_vars`.

**The picker** replaces `NewSessionDialog`'s image/mode/prompt fields entirely.
The new-session entry opens a picker of profile cards (icon, name, description)
ordered by `sort_order`; one selection creates the task. There is **no escape
hatch** — no free-form/custom option that bypasses profiles. Concretely:

- **No profiles configured:** the dialog still opens and shows a "No profiles
  configured — contact an admin" empty state. The entry point stays
  visible (no mysteriously disabled button); the dead-end is explained in the
  one place the user looks.
- **Exactly one profile:** still show the picker. The one-profile short-circuit
  is not worth the branch; consistency beats saving one click.

Profiles are **global** to the org. Team-scoping is not modeled (§7).

### 7. Deliberately deferred

Out of scope for this ADR, modeled-around but not built:

- **Capability scoping** — enabling/disabling specific MCP servers and tools
  per profile. This is the main forward-looking reason profiles exist as a
  first-class object rather than a saved form; it lands as its own ADR. The
  `env_vars` jsonb and the profile object are shaped to host it.
- **Multiple user-token types.** `include_user_tokens` is a single boolean over
  "the Claude token" today; a per-token-type grant waits until a second token
  type exists.
- **Team / per-team scoping**, **per-profile session timeouts**, **profile
  categories/tags**, and **profile restore** UI (the data supports restore via
  clearing `deleted_at`; the affordance is deferred).

## Rationale

- **Admin-time choices, not user-time choices.** Image, model, environment, and
  the credential decision are governance, not per-session preferences. Moving
  them into an admin-curated object is the whole point; the picker is the
  user-facing consequence.
- **Profiles are orchestrator data, full stop.** They assemble exactly the
  inputs the orchestrator already feeds `CreateSession`; the control plane stays
  the AWS-shaped resource API that knows nothing about them (ADR 0051). That is
  why the table, the service, and the authz all live in the orchestrator, and
  why `ProfileService` is native rather than passthrough.
- **Logical image reference because the boundary forbids a real FK.** The image
  catalog is the coordinator's; a hard FK across tiers would couple their
  migrations. Storing `image_id` + resolving at use keeps integrity in
  application code where the cross-tier seam already is, and survives image
  refreshes that a stored uri would not.
- **The disable guard goes where the data is.** The orchestrator owns profiles,
  so the orchestrator enforces "don't disable an image a profile needs" — as a
  pre-flight on the passthrough, before the coordinator (which can't see
  profiles) ever hears about it.
- **Soft delete because the link must outlive the profile.** A
  `task_session.profile_id` is a permanent historical fact; hard-deleting
  profiles would either orphan it or force a cascade that erases the history. An
  admin archive turns "retired" into a first-class, reversible state.
- **One env mechanism for the model.** Threading the model as `env_vars` rather
  than a column means there is a single code path for "configured environment,"
  and the harness already reads its model from the environment.
- **No escape hatch by intent.** A free-form bypass would defeat the governance
  the feature exists to create; the empty state handles the only legitimate
  "there's nothing to pick" case.

## Implications

- **Proto / contract:** new `profile.proto` in the app package; `buf generate`
  emits the orchestrator handler stubs and the connect-es/connect-query client.
  `CreateTaskRequest` changes (drops `image_uri`/`prompt`, adds `profile_id`) —
  a breaking change to that message, caught by `buf` breaking-change CI and
  coordinated with the web cutover. The control-plane contract is **unchanged**.
- **Orchestrator:** a Drizzle migration adds the `profiles` table and
  `task_session.profile_id`; `CreateTask` is rewritten to resolve a profile
  (image resolution + `harness_env` assembly + mode mapping); a `DisableImage`
  pre-flight guard is added to the passthrough; the CASL policy map gains the
  five `ProfileService` methods; `ListProfiles`/`GetProfile` handlers strip
  `env_vars` for non-admins and gate `include_archived` on admin.
- **Coordinator:** no changes. It keeps its own image-disable session guard;
  the profile guard sits in front of it.
- **Web:** `NewSessionDialog` becomes a profile picker (card grid, empty state,
  no custom option); a new admin Profiles surface provides CRUD + the Archived
  section; the shared CASL ability file gates the admin affordances. The client
  sends `profile_id` to `CreateTask` and no longer assembles image/mode/prompt.
- **New failure modes to surface in the UI:** "no profiles configured" (empty
  picker), "image disabled out from under a profile" (create-time rejection,
  should be rare behind the §3 guard), and "cannot disable image — N profiles
  use it" (admin image-disable rejection with the blocking names).
- **Migration / rollout:** until at least one profile exists, no sessions can be
  created through the UI. The rollout step is therefore "create the initial
  profiles" before or with the web cutover — the old free-form path is removed,
  not left as a fallback.
