# ADR 0052: Session profiles — admin-curated starting points replace the free-form new-session form

Status: 2026-06-16 — **Proposed.** No code yet. Builds on ADR 0051 (the
TypeScript orchestration tier: orchestrator-native `TaskService`, CASL
authorization, the orchestrator's own Postgres, and the generic control-plane
passthrough) and ADR 0036 (image enablement / enable jobs). It changes the
shape of `CreateTask` and the web "new session" flow defined there.

Revised 2026-06-16: profiles carry **no `mode`** (every profile launches an
`agent` session) and **no `sort_order`** (the picker filters by a search box
rather than admin-curated ordering). Both are recorded as discovery decisions
below and threaded through the body.

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
**profile** — and have users simply pick one. A profile bundles the image, a
model and environment, and an explicit decision about whether the user's Claude
token is injected — everything *except* the task itself, which the user still
describes at create time. The new-session flow becomes "pick a profile, then
say what to run," not "configure the whole environment every time." Profiles
are also the natural future home for capability scoping (enabling/disabling
specific MCP servers and tools per profile) — explicitly **not** built here,
but the model is shaped to grow into it.

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
icon                text                  -- a lucide icon name (the web app's icon set)
image_id            uuid                  -- logical ref to the coordinator's enabled_images.id (§3)
include_user_tokens boolean               -- inject the user's Claude token (§5)
env_vars            jsonb                 -- { KEY: VALUE }; admin-only; the model lives here (§5)
created_at          timestamptz
updated_at          timestamptz
deleted_at          timestamptz null      -- null = active; soft delete only (§4)
```

Deliberate omissions, each decided rather than overlooked:

- **No `mode` column.** Every profile launches an `agent` session in this ADR;
  the shell/`dev_vm` path is not curated through profiles yet. Fixing mode to
  `agent` keeps `CreateTask` on its single existing code path and defers the
  agent/shell split to the profile that first needs it. (§5, §7)
- **No `sort_order` column.** The picker filters by **search** rather than
  admin-curated ordering, so there is no order to persist. Profiles list in a
  stable default order (by `name`); a search box handles "find the one I want"
  at any N, and removes the reorder/drag affordance entirely. (§6)
- **No `model` column.** The model is just one entry in `env_vars` (the harness
  reads it from the environment). A first-class column would duplicate that and
  invite a parallel plumbing path. One mechanism, not two.
- **No `created_by_user_id`.** Profiles are global org configuration, not
  user-owned content; authorship attribution buys nothing here.
- **No `is_active` flag.** `deleted_at IS NULL` *is* the active predicate; a
  second boolean would be a redundant, drift-prone state.
- **No `starting_prompt`.** A profile configures the *environment*, not the
  *task*. The prompt is the user's, supplied per-session in the picker; baking a
  prompt into the profile would conflate "how the session is set up" with "what
  to do," and there is no use today for an admin-pinned task. (§5)

### 2. Where it lives, and the `task_session` link

Profiles are an orchestration concern: they exist only to assemble the inputs
the orchestrator already hands to the control plane's `CreateSession`. The
control plane has no notion of a profile and never will — it keeps taking
`image_uri` / `mode` / `prompt` / `secrets` / `harness_env` (ADR 0051 §2.1).
The orchestrator simply sends `mode: "agent"` for every profile-started session
(§5).

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

`CreateTaskRequest` changes shape: it **drops `image_uri`**, **gains
`profile_id`**, and **keeps the optional `prompt`** — now strictly the user's
task, never an admin-configured value (profiles carry no prompt). The client
sends the profile the user picked plus the task they typed, and the
orchestrator's `CreateTask` handler (orchestrator-native, ADR 0051 §3) resolves
the environment server-side:

1. Load the profile by id; reject if missing or soft-deleted.
2. Resolve `image_id` → current `image_uri` (§3); reject if the image is no
   longer enabled (defense in depth behind the §3 restrict guard).
3. Send the control plane `mode: "agent"` (profiles carry no mode yet, §1). The
   request's `prompt` (the user's task) becomes the session's first prompt.
4. **Assemble `harness_env`** in this precedence (lowest → highest):
   1. the user's Claude token (`CLAUDE_CODE_OAUTH_TOKEN`, resolved from the
      sealed secret store, ADR 0051 §2.3) — **only if** `include_user_tokens`
      is true;
   2. the profile's `env_vars` — **override** anything below, including the
      token key if an admin set it explicitly. Admin intent wins over the user
      token by design.
5. Call the control plane's `CreateSession`, then write the `task` and
   `task_session` rows, recording `profile_id` on the latter.

**Mode is always `agent`.** Profiles do not carry a mode in this ADR (§1);
`CreateTask` sends the control plane's `mode: "agent"` unconditionally, exactly
as the handler does today. The shell/`dev_vm` split moves into profiles only
when a profile needs it (§7); until then the existing direct-`CreateSession`
dev-VM path is simply unreachable from the profile picker.

**`include_user_tokens` is a capability grant, not a convenience toggle.** It
is the admin's explicit declaration that sessions started from this profile are
permitted to carry the user's Claude credentials. For an untrusted or
externally-facing image, an admin leaves it off and the token never enters the
sandbox. Today the only such token is the Claude OAuth token; the boolean is
named and modeled to generalize, but we do not enumerate token *types* yet
(YAGNI, §7).

**Edits don't reach running sessions.** A session is created from a *snapshot*
of the profile's values at create time; `task_session.profile_id` is a
historical record, not a live binding. Changing a profile's model or
environment later affects only sessions created after the change. This falls out of the
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

**The picker** replaces `NewSessionDialog`'s image and mode fields; the user's
task prompt stays. The new-session entry opens a picker of profiles (icon,
name, description) with a **search box** that filters the list by
name/description; profiles list in a stable default order (by `name`). The user
picks one, types the task, and creates the session. There is **no escape
hatch** — no free-form/custom image, mode, or environment that bypasses
profiles. Concretely:

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
- **Shell / `dev_vm` profiles.** Every profile launches an `agent` session
  today; curating a shell profile (mapping to the control plane's `dev_vm`
  mode) waits until there's a need. The `CreateSession` contract already
  accepts `mode`, so this is a `mode` column on `profiles` plus a mapping step
  in `CreateTask` when it lands — no contract change. (§1, §5)
- **Admin-curated ordering.** Replaced by picker search; if explicit ordering
  is wanted later it returns as a `sort_order` column plus a reorder affordance
  on the management list. (§1, §6)
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
- **Agent-only keeps one code path.** Fixing mode to `agent` means `CreateTask`
  keeps the single control-plane call it already makes; the agent/shell branch
  is added only when a curated shell profile actually exists (§7).
- **Search instead of curated order.** With a handful of profiles, a search box
  finds the right one faster than maintained ordering does, and it drops a whole
  reorder UI (drag handles, `sort_order` writes) the feature does not need yet.
- **No escape hatch by intent.** A free-form bypass would defeat the governance
  the feature exists to create; the empty state handles the only legitimate
  "there's nothing to pick" case.

## Implications

- **Proto / contract:** new `profile.proto` in the app package; `buf generate`
  emits the orchestrator handler stubs and the connect-es/connect-query client.
  `CreateTaskRequest` changes (drops `image_uri`, adds `profile_id`, keeps the
  now user-only `prompt`) — a breaking change to that message, caught by `buf`
  breaking-change CI and coordinated with the web cutover. The control-plane
  contract is **unchanged**.
- **Orchestrator:** a Drizzle migration adds the `profiles` table and
  `task_session.profile_id`; `CreateTask` is rewritten to resolve a profile
  (image resolution + `harness_env` assembly; mode stays the hardcoded
  `agent`); a `DisableImage` pre-flight guard is added to the passthrough; the
  five `ProfileService` methods are authorized with the same CASL machinery (as
  native handlers, the checks live in the handlers, not the passthrough policy
  map); `ListProfiles`/`GetProfile` handlers strip `env_vars` for non-admins
  and gate `include_archived` on admin.
- **Coordinator:** no changes. It keeps its own image-disable session guard;
  the profile guard sits in front of it.
- **Web:** `NewSessionDialog` becomes a profile picker (filterable list, empty
  state, no custom option); a new admin Profiles surface provides CRUD + the
  Archived section; the shared CASL ability file gates the admin affordances.
  The client sends `profile_id` plus the user's `prompt` to `CreateTask` and no
  longer assembles image or mode.
- **New failure modes to surface in the UI:** "no profiles configured" (empty
  picker), "image disabled out from under a profile" (create-time rejection,
  should be rare behind the §3 guard), and "cannot disable image — N profiles
  use it" (admin image-disable rejection with the blocking names).
- **Migration / rollout:** until at least one profile exists, no sessions can be
  created through the UI. The rollout step is therefore "create the initial
  profiles" before or with the web cutover — the old free-form path is removed,
  not left as a fallback.

## Impeccable shaped design for the UI

Produced with the `impeccable shape` flow against the committed "Mont Blanc
logbook" / Aston-racing design system (`web/PRODUCT.md`; no `DESIGN.md`). This
is the agreed UX/UI direction for the three web surfaces profiles touch; it
shapes presentation only — the data model, `ProfileService` contract, and
authorization above are fixed.

Five discovery decisions correct or sharpen the body of this ADR:

- **Icons are lucide-react, not Phosphor.** The web app uses lucide everywhere
  (sidebar, buttons, every component); a second icon family would break the
  product's consistent-icon-style rule. The profile `icon` column still stores
  an icon-name string — it just names a **lucide** icon (§1 reflects this).
- **The admin surface lives under Settings, not Operator.** Profiles are org
  setup more than fleet operations.
- **Profiles carry no prompt.** The prompt is removed from the profile entirely
  (2026-06-16); the task is the user's to write at create time. A profile
  configures the *environment* (image, model, env, token grant), not the
  *work*. This is why §1 has no `starting_prompt` column and §5 keeps the
  user-supplied `prompt` on `CreateTaskRequest`.
- **Profiles carry no mode — every profile is an `agent` session (2026-06-16).**
  The agent/shell toggle is dropped from profiles for now; `CreateTask` always
  sends `mode: "agent"`, keeping its single existing code path. The picker has
  no mode control and the task field is always shown. Curating a shell/`dev_vm`
  profile is deferred (§1, §7).
- **The picker filters by search, not curated order (2026-06-16).** `sort_order`
  is dropped: the new-session picker gets a search box that filters by
  name/description, and profiles list in a stable default order. This also
  removes the management-list reorder/drag affordance (§1, §6).

### 1. Feature summary

Profiles turn "fill in a new-session form" into "pick a curated starting
point." Three surfaces: the **new-session picker** (all users), the **admin
management surface** under Settings (list + create/edit), and a **profile
identity** that replaces the raw image string in the session detail, rail, and
list.

### 2. Primary user action

- **Developer (≈90% of sessions):** open new-session → search/scan a short list
  of named profiles → pick one → session starts. Zero technical input.
- **Admin:** curate that short list — create / edit / archive profiles.

### 3. Design direction

Reuses the existing system as-is. **Restrained** color strategy — Settings is
config, not a hero surface; lime (`--primary`) stays fill-only on the confirm
action, and profile identity reads in ink plus the lucide glyph. Scene
sentence: *an engineer at their desk, mid-task, picking how to launch a unit of
agent work — and an admin, occasionally, setting the few ways that's allowed to
happen.* Anchor references: **Linear** settings panels (dense list + side
editor), **Vercel** project-settings env-var editor (the key/value control),
**Raycast** command list (the picker's quiet, keyboard-first search + selection
feel).

### 4. Scope

Production-ready, shipped quality. Full breadth: picker + management list +
create/edit editor + the app-wide profile chip. Real `ProfileService` wiring
(connect-query / TanStack Query, matching the existing hook idioms).

### 5. Layout strategy

**A. New-session picker** (`NewSessionDialog`, all users) — stays a dialog (it
is invoked from the header, the ⌘-palette, the sessions rail, and a keyboard
shortcut). Its image and mode fields are replaced by a **vertical list of
selectable profile rows** (radio-card semantics): lucide glyph · name ·
description, with a quiet meta line (`model · carries your token`). A **search
box** above the list filters by name/description (Raycast-style, keyboard-first);
the list otherwise shows in a stable default order (by `name`). A **task field**
(the user's prompt) sits below the list and is always shown — every profile is
an agent session. Selecting a profile, then a primary lime **"Start session"**,
confirms. A list beats a card grid here — few profiles are expected, and it
avoids the identical-card-grid trap. There is no escape hatch for image / env:
those are the profile's to set, not the user's.

**B. Management surface** (`/settings/profiles`, admin) — added to the Settings
second sidebar as **"Session Profiles"**, deliberately disambiguated from the
account "Profile" page; non-admins never see it (the existing `requireAdmin`
guard, which already redirects members to `/settings/profile`). Layout is a
**list of profile rows**, not a data table: glyph + name + description ·
resolved image · model · token indicator · edit/archive actions. A list rather
than the tabular Members/Images panels because the rich icon+description
identity suits rows, and N is small — a deliberate, explained divergence. Below
the active list, a collapsible **"Archived"** section (read-only; restore is
deferred per §7).

**C. Create/Edit editor** (`/settings/profiles/new`, `/settings/profiles/$id`,
admin) — a **full sub-page**, not a modal: the form is too tall for a dialog
(icon picker + env key/value editor), and the product rule is to exhaust inline
before reaching for a modal. Single column, grouped with the existing `Field`
system: Identity (name, description, icon picker) → Launch (image select) →
Environment (`include_user_tokens` switch, `env_vars` editor). There is no
prompt field and no mode field — a profile configures the environment, not the
task, and every profile is an agent session.

### 6. Key states

- **Picker:** default (list) · empty ("No profiles configured — contact an
  admin"; the entry point stays visible, no mysteriously disabled button) ·
  loading (row skeletons) · single-profile (still the picker) · search with no
  matches · error.
- **Management list:** default · empty ("No session profiles yet" + Create) ·
  loading · error · archived section (empty / with rows).
- **Editor:** create vs edit · field validation (name/description required,
  image required, env-key format) · save success (toast + return to list) ·
  save failure · the rare "image no longer enabled" rejection.
- **Profile chip (detail / rail / list):** default (glyph + name) · **details
  on hover/focus + click** (resolved `image_uri`, model, token-grant;
  admins also get a link to the profile) · **archived** profile
  (muted + an "archived" badge, still resolvable) · **profile-less / legacy**
  session (falls back to the image string, as today) · loading.
- **Adjacent:** the Operator → Images disable action must surface the new
  `failed_precondition` from §3 — "Can't disable — N profiles use this image" +
  the blocking names. In scope, lightly.

### 7. Interaction model

- **Profile chip** is one reusable `<ProfileChip>` with a disclosure mode: a
  **Tooltip** in dense link rows (rail/list — the whole row is already a link,
  so the disclosure stays glanceable) and a **HoverCard/Popover** in the detail
  header (opens on hover, focus, and click/tap — keyboard-reachable and
  touch-tappable). Adds shadcn `hover-card` + `popover` (neither exists yet).
- **Icon picker:** a Popover with a `Command` search over a curated,
  dev-relevant lucide starter set (Terminal, Bot, Bug, Wrench, FlaskConical,
  GitBranch, Rocket, Cpu…) plus full search; selection writes the lucide name
  string into `icon`.
- **Env editor:** repeatable KEY / VALUE rows, add/remove, with a hint that the
  model is set here — no separate model field (one mechanism, per §1).
- **Picker search:** a search box above the profile list filters by
  name/description (Raycast-style, keyboard-first); there is no manual ordering
  to curate and no reorder affordance.
- **Create flow:** new session → search/pick → "Start session" → navigate to the
  new session's detail with the transcript streaming.

### 8. Content requirements

- Empty: "No profiles configured — contact an admin to set one up." / "No
  session profiles yet."
- `include_user_tokens` helper (capability-grant framing, not a convenience
  toggle): "Sessions started from this profile may carry the user's Claude
  credentials into the sandbox. Leave off for untrusted or externally-facing
  images."
- Picker task field placeholder: "Describe the task for this session…" — this
  is the user's prompt; profiles do not set it.
- Picker search placeholder: "Search profiles…"
- Buttons (verb + object): "Create profile," "Save changes," "Archive
  profile," "Start session."
- Realistic ranges: 0 profiles (empty), 2–6 typical, ~20 max; `env_vars` 0–~15
  rows; description ~1 line.

### 9. Backend contract assumption

Session list/detail responses (`ListTasks` / `GetSession`) embed a lightweight
resolved snapshot — `profile { id, name, icon, archived, image_uri }` — keyed
off `task_session.profile_id`, enough for the chip and its hover glance without
an N+1. Richer admin detail comes from `GetProfile` lazily on the management
page.

### 10. Resolved

1. **Does the user still provide a task at creation? — Yes** (decided
   2026-06-16). The prompt configuration is removed from profiles entirely;
   profiles configure the environment, and the task is the user's to write in
   the picker's task field at create time. `CreateTaskRequest` keeps the
   optional, user-supplied `prompt`, and `profiles` has no `starting_prompt`
   column.
2. **Do profiles carry a mode? — No** (decided 2026-06-16). Every profile
   launches an `agent` session; `CreateTask` hardcodes `mode: "agent"`. The
   shell/`dev_vm` split is deferred to a future profile that needs it (§7).
3. **Admin-curated ordering or search? — Search** (decided 2026-06-16). The
   picker filters by a search box and lists in a stable default order; there is
   no `sort_order` column and no reorder UI (§6).
