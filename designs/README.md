# Handoff: Engrams — auth & users redesign (ADR 0031)

## Overview
ADR 0031 added authentication, a signed-in **principal**, **roles** (admin / member),
and an **admin users API** to Engrams — but the dashboard UI never caught up. This
handoff is a complete design for those surfaces: an **identity vocabulary**, the
missing **Members** (admin users) surface, a regrouped **Settings** information
architecture, a **Tokens** ledger, **owner-attributed sessions**, the **member
experience**, and the pre-login **auth states** — all in the existing "Lab
Notebook" design system.

It also fixes two shipped defects: the owner-row wrap bug in the admin "all
sessions" view, and the rounded avatar in the profile (the system is square-corner).

## About the design files
The files in `kit/` are a **design reference created in HTML/CSS + React-over-Babel**
— a runnable, clickable prototype showing intended look and behavior. **They are not
production code to copy verbatim.** The task is to **recreate these designs in the
real `cortexapps/engrams` web app** (`web/`, which is React + TypeScript + Vite +
TanStack Query + react-router + framer-motion), using its existing components, the
`theme.css` token layer, and established patterns. Lift exact values (hex, spacing,
type, copy) from this spec; implement with the app's real primitives.

Open `kit/index.html` in a browser to explore. Use the **Tweaks** panel (top
toolbar) to drive:
- **View as** → `admin` / `member` (the whole app re-renders for each role)
- **Auth state** → `app` / `boot` / `error` / `not a member`

## Fidelity
**High-fidelity.** Final colors, typography, spacing, copy, interactions, and
responsive behavior. Recreate pixel-faithfully using the codebase's libraries.

---

## Design tokens
These mirror the real `web/src/theme.css`. The kit defines them in
`kit/colors_and_type.css`. Do **not** introduce new hues or rounded corners.

**Color (warm-paper "Lab Notebook" palette)**
| token | hex | role |
|---|---|---|
| `--bg` / `--color-paper` | `#f4eedf` | page paper |
| `--bg-raised` | `#efe7d3` | raised paper (badges, hover) |
| `--fg` / `--color-ink` | `#1b1612` | primary ink |
| `--fg-muted` / `--color-ink-faded` | `#5c544a` | secondary ink |
| `--fg-quiet` / `--color-ink-quiet` | `#8a8275` | tertiary ink / labels |
| `--border` / `--color-rule` | `#d9cfb8` | hairline rule |
| `--border-faint` | `#e7decb` | faintest rule |
| `--accent-now` / `--color-amber` | `#b85c0a` | "now" / live / primary action |
| `--accent-archived` / `--color-verd` | `#3a6b5c` | archived / affirmative (✓) |

**Type**
- Display / serif: **Newsreader** (`--font-display`) — headings, names, prose, italics.
- Mono: **JetBrains Mono** (`--font-mono`) — ids, emails, labels, small-caps eyebrows.
- Small-caps label idiom: `font-variant-caps: all-small-caps; letter-spacing: 0.12–0.18em`.

**Shape & motion**
- **Border-radius: 0 everywhere.** Square corners are load-bearing in this system.
- Hairline borders only (`1px solid var(--border)`). No shadows except the documented popover drop.
- `--dur-fast` for hover/color transitions; the engram trace mark animates on boot/loop.

---

## The identity vocabulary (new brand layer)
The system had glyphs for *session status* but nothing for *people*. These are the
new primitives — typography & rules only, no new hue. Implemented in the kit in
`kit/components.jsx`; cards in `preview/brand-person-mark.html` and
`preview/brand-identity-tags.html`.

- **Person mark** — an embossed typesetter's initial (the person's first letter,
  Newsreader italic, `translateY(-1px)`) on `--bg-raised` inside a 1px `--border`
  **square** frame. Sizes: `xs` 1.6rem (rows), `sm` 2.1rem (chip / ledger), `md`
  3rem (profile). Disabled variant uses `--fg-quiet` ink + `--border-faint` frame.
  *Fixes the shipped split where the chip was square but the profile avatar was
  `border-radius: 0.4rem`.*
- **Owner badge** — same square frame, but for sessions launched by the platform
  itself it holds the **engram trace mark** instead of an initial (see Sessions).
- **Role tag** — a square hairline box, mono all-small-caps, `letter-spacing:0.1em`.
  `admin` reads heavier (ink text + ink border); `member` is quiet (quiet ink +
  `--border`). **Differentiation is by weight, never a new color.**
- **Provenance** — a faint Newsreader-italic note for `role_source`:
  `claim → "via okta claim"`, `scim → "via scim sync"`, `manual → "set by an admin"`.
- **Member status** — ink only. `active` is plain small-caps; `disabled` is quieted
  and prefixed with the system's `✕` glyph.

---

## Screens / Views

### 1 · Nav spine (role-aware)
`kit/nav.jsx`; maps to `web/src/components/NavSpine.tsx`.
- Wordmark + engram mark (left), primary tabs (center), identity chip (right), hairline rule beneath.
- Tabs: **admin** → Sessions · Fleet · Storage · Settings. **member** → Sessions · Settings only.
- **IA change vs. shipped:** Settings is visible to **everyone** (it holds the user's
  Profile + Tokens); the admin-only Fleet/Storage are filtered out for members via an
  `adminOnly` flag, and the admin-only *config* is filtered **inside** Settings — not
  by hiding the whole tab. Active tab: `--fg` text + `--accent-now` bottom-border (1px).
- **Identity chip** (replaces the shipped `§` placeholder): a `sm` person mark; click
  opens a popover with name + `RoleTag`, email, "Settings", and "Sign out" (disabled if
  `principal.can_sign_out` is false).

### 2 · Sessions — owner attribution (+ bug fix)
`kit/surface-sessions.jsx`, `SessionRow` in `kit/components.jsx`; maps to
`web/src/pages/Sessions.tsx` + `SessionManifest.tsx`. Card: `preview/comp-session-rows.html`.
- **Every session is owner-attributed.** Owner is a first-class column on a **fixed
  track** (the badge leads so badges align row-to-row; age never wraps).
  - **Person-owned** → `xs` person mark + email (mono, `--fg-quiet`, ellipsis).
  - **Platform-owned** (warm-pool boots, scheduled/automated runs) → the **engram
    trace mark** in the badge frame + the word **"engrams"** (Newsreader italic). The
    specific trigger (e.g. "warm-pool boot") is a `title` tooltip, not inline text.
- **Row grid** (the fix): the shipped row was a 5-column grid and the admin view added
  a 6th child (owner) that overflowed and wrapped the age onto a second line. Use an
  explicit owner column. Kit values: desktop
  `grid-template-columns: 1ch 13ch 1fr 5rem 17rem 4ch` (glyph · id · image · status ·
  owner · age) with `align-items: baseline`. Size the id track to the id width so it
  can't push the flexible image column out of alignment.
- **Admin** sees a **My sessions · All sessions** scope toggle (Newsreader, active =
  `--fg` + 1px `--fg` underline); "All" includes platform-owned sessions. **Members**
  see only their own sessions (no toggle, no owner column).
- Status glyphs (unchanged): `● active` (amber, pulsing), `◐ created/guest_ready`,
  `○ pending`, `◌ idle` (verd), `⚠ host_lost`, `✓ completed`, `! failed`, `✕ dead`.

### 3 · Members (NEW admin surface)
`MembersPanel` in `kit/screens.jsx`; maps to a new page backed by `GET /admin/users`
+ `PATCH /admin/users/:id` (both already in `web/src/api.ts` as `fetchUsers` /
`updateUser`). Lives at **Settings → Members** (Deployment group); gate with the
existing `RequireAdmin`.
- A roll-up line: `<b>N</b> people · <b>N</b> admins · <b>N</b> disabled` above a 1px rule.
- A **ledger** (reuses the Storage/durability ledger idiom). Columns:
  `person | role | source | status · actions` —
  grid `2.4fr 0.8fr 1.3fr 1.7fr`, rows separated by `1px dotted --border`, header row
  is small-caps labels.
  - **person** = `sm` person mark + Newsreader name (with an italic "you" tag on the
    signed-in admin) over mono email.
  - **role** = `RoleTag`; **source** = `Provenance`; **status** = `MemberStatus`.
  - **actions** (Newsreader italic links, right-aligned): `make admin` / `revoke
    admin` (primary = `--accent-now` when promoting, quiet when revoking) and
    `deactivate` / `reactivate`. The signed-in admin shows "you" and **no actions**
    (no self-demote / self-deactivate).
- On a manual role change, set `role_source = manual` (UI stamps it "set by an admin").
- Deactivated rows: `opacity: ~0.8`, quieted name, `✕ disabled` status.
- Footnote (Newsreader italic) explains JIT/SCIM provisioning.

### 4 · Settings — grouped IA
`Settings` in `kit/screens.jsx`; maps to `web/src/pages/Settings.tsx`. Card: `preview/comp-tabs.html`.
- Replaces the flat `Images · Registries · Profile` row with **two labeled groups**
  separated by a vertical hairline:
  - **You** → Profile · Tokens
  - **Deployment** (admin only) → Members · Images · Registries
- Each group has a mono all-small-caps label (`--fg-quiet`, `letter-spacing:0.18em`)
  above its tabs. Active tab: `--fg` text + 1px `--fg` underline. Members see only **You**.

### 5 · Profile
`ProfilePanel` in `kit/screens.jsx`; maps to `web/src/components/settings/ProfilePanel.tsx`.
- Header: `md` **square** person mark + Newsreader name (1.2rem) over mono email.
- A definition list: **role** (`RoleTag` + `Provenance`) and **tokens** (summary +
  link to the Tokens tab).
- **Access legend** ("what your role can do") — a bordered-top block listing
  abilities with verd `✓` glyphs; for members, gated items are listed quiet with `✕ …
  admin only`. This makes the role model legible. (admin list vs. member list differ —
  see `AccessLegend` in `kit/screens.jsx`.)

### 6 · Tokens (ledger)
`TokensPanel` in `kit/screens.jsx`; maps to / replaces
`web/src/components/settings/TokensPanel.tsx`.
- **One tab, a ledger of services** (not a tab-per-service, not folded into Profile).
  Each row: service name (Newsreader 1.05rem) + a "what it's for" italic line on the
  left; **status** (`saved · sealed` in `--fg-muted`, or `not connected` in
  `--fg-quiet`); **actions** on the right.
- Saved rows → `replace` / `remove`. Unsaved → `add →` (primary `--accent-now`),
  which reveals an inline `password` field + hint + `save` / `cancel`. Saving flips
  status to saved.
- Seeded services: **Claude Code** ("built-in Claude sessions authenticate with
  this", from `claude setup-token`) and **GitHub** ("clone private repositories into a
  session"). New services slot in as additional rows.
- Footnote: tokens are sealed under the deployment key on save; plaintext never
  touches Postgres; used automatically (never prompted per session).

### 7 · Member experience
Driven by `View as → member`.
- Nav collapses to Sessions · Settings; identity chip shows the member's initial.
- Sessions: own-only, no owner column, no scope toggle, plus a hairline **token
  nudge** above the manifest when no Claude token is saved ("no Claude Code token
  saved yet — built-in Claude sessions need one. add token →" → routes to
  Settings → Tokens). NOT a colored card — prose + an `--accent-now` action.
- Settings: only the **You** group; Profile's access legend shows the member's
  (smaller) ability set with the gated items quieted.

### 8 · Auth states (pre-login)
`AuthScreen` in `kit/screens.jsx`; maps to the boot/error gates in
`web/src/auth/AuthProvider.tsx` (replace the inline-styled `BootScreen` /
`AuthErrorScreen`). Full-viewport, centered on `--bg`, 32rem card, engram mark, the
lowercase em-dash voice:
- **boot** — looping engram mark + "authenticating…".
- **error** — static mark + "could not reach the coordinator — retrying…" + the
  failing request (mono) + a "retry now" action.
- **not a member** — static mark + "you're signed in — but not yet a member of this
  deployment." + the email + "ask an admin to add you, then reload." + "sign out".

---

## Interactions & behavior
- **Role / status mutations** (Members) optimistically update the row, then PATCH
  `/admin/users/:id`; on role change set `role_source = manual`.
- **Token add/replace/remove** posts to the user-token endpoint; never render the
  stored value back (show a sealed placeholder).
- **Scope toggle** (admin Sessions) filters the manifest client-side or via query param.
- **Identity chip popover** opens on click, closes on outside-click / Esc (framer-motion fade is fine).
- **Guards:** keep `RequireAdmin` on the Deployment sub-routes (Members / Images /
  Registries) and admin-only pages — the UI filtering is UX only; the coordinator's
  `require_admin` is the real gate.

## State management
- `principal` from `GET /me` (already via `AuthProvider`): `{ name, email, role,
  is_admin, has_claude_token, can_sign_out, role_source }`.
- `useQuery(['admin','users'])` → `AdminUser[]` for Members; `useMutation` → `updateUser`.
- Local UI state: Settings active tab, Sessions scope, Tokens editing-row.

## Responsive behavior
All new surfaces reflow under `@media (max-width: 760px)` (see end of `kit/dashboard.css`):
- Grouped Settings tabs → stacked column (divider hidden, each group hairline-topped).
- Members ledger → stacked records (header hidden; role/source/status/actions wrap under the person).
- Tokens ledger → stacked (status + actions drop under the service).
- Profile rows → label-over-value.
- Session rows already fold owner/age (existing `@media (max-width: 600px)` idiom).

## Assets
- **Engram mark** — `kit/engram-mark.jsx` (React) and `assets/`/`preview/` SVG forms.
  Already in the app; reuse the real component. Used in the identity chip, the
  platform-owner badge, and the auth screens.
- No raster assets. Person marks are typeset initials, not images.

## Files
- `kit/index.html` — open this; the full clickable redesign.
- `kit/colors_and_type.css`, `kit/dashboard.css` — tokens + all component CSS (new
  sections are clearly commented: identity vocabulary, Members ledger, grouped
  settings, profile + access legend, tokens ledger, auth states, responsive).
- `kit/components.jsx` — identity primitives (`PersonMark`, `RoleTag`, `Provenance`,
  `MemberStatus`, `OwnerBadge`, `OwnerCell`), `SessionRow`, `NavSpine` chip, seed data
  (`PRINCIPAL`, `PEOPLE`).
- `kit/screens.jsx` — `Settings`, `ProfilePanel`, `AccessLegend`, `TokensPanel`,
  `MembersPanel`, `AuthScreen`.
- `kit/surface-sessions.jsx` — Sessions surface + scope toggle + token nudge.
- `kit/nav.jsx` — role-aware nav spine.
- `kit/app-redesign.jsx` — app shell + the View-as / Auth-state review Tweaks.
- Design-system specimen cards (in the project `preview/` folder, not copied here):
  `brand-person-mark.html`, `brand-identity-tags.html`, `comp-session-rows.html`.

## Real source files this maps onto (in `cortexapps/engrams`, `web/src/`)
`App.tsx` (routes) · `auth/AuthProvider.tsx` (principal, boot/error gates) ·
`auth/RequireAdmin.tsx` (guard) · `api.ts` (`fetchUsers`, `updateUser`, token endpoints) ·
`types.ts` (`Principal`, `AdminUser`) · `components/NavSpine.tsx` · `components/UserChip.tsx` ·
`pages/Sessions.tsx` + `components/SessionManifest.tsx` · `pages/Settings.tsx` ·
`components/settings/ProfilePanel.tsx` + `TokensPanel.tsx` + `_form.tsx` · **new** `pages/Members.tsx`.
