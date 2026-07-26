# 0100 — PR code review: engrams as a code reviewer on GitHub pull requests

Status: Proposed (2026-07-15)

engrams reviews pull requests in enrolled repos. A review runs as one or more
sandboxed sessions that investigate the change with real tools (clone, grep,
read, git history), report findings through orchestrator-registered tools
(ADR 0089), and never hold GitHub write credentials — the orchestrator is the
only thing that posts to GitHub. Humans converse with the reviewer directly in
PR threads. When a PR was authored by an engrams session, an opt-in autofix
mode routes findings back to that session so it can fix them or discuss them
on the PR.

This replaces an earlier unmerged design (the `adr-0067-0068-pr-review-design`
branch, 2026-07-02). The system has changed substantially since then — the
generic tool protocol shipped as ADR 0089, profiles and the task model matured
— so this ADR stands alone and supersedes that draft.

## What we learned from the field

We studied CodeRabbit, Greptile, Devin Review, Cursor Bugbot, and kodus
(open-source reviewer, full pipeline read) before designing. The lessons that
shaped this ADR:

1. **Precision comes from architecture, not prompting.** Every vendor that
   tried "prompt the model to be less noisy" or "have an LLM rate its own
   comments" failed — Greptile measured LLM-as-judge scores on its own output
   as near-random. What works everywhere: a high-recall *finder* pass followed
   by an independent *verifier* pass that tries to refute each finding and
   drops what it can't confirm (kodus's "refute-to-drop" verifier, CodeRabbit's
   separate judge model). kodus adds an *evidence gate*: a finding is suspect
   unless the agent actually read the file it's about.
2. **Confidence and severity are separate axes** (Devin Review). A finding can
   be severe but uncertain. Devin only posts high-confidence findings to the
   PR; uncertain ones become "flags" visible in the product UI but never on
   the PR. The PR surface stays high-signal; nothing is lost.
3. **Agent-with-tools beats prompt-stuffing.** CodeRabbit, Greptile v3, and
   kodus all converged on: give the model a full checkout plus grep/read/git
   tools in a sandbox and let it investigate, instead of cramming context into
   one giant prompt. CodeRabbit runs every review in its own microVM. An
   engram session *is* this architecture — we get their sandbox story for
   free, with stronger isolation.
4. **Findings only on changed lines.** kodus's single biggest false-positive
   fix was a hard rule: a finding that doesn't land on a line changed in this
   PR gets dropped, never re-anchored. Comments about unchanged code are the
   fastest way to lose trust.
5. **The metric that matters is the address rate** — did the author actually
   change the code — not comment volume. Greptile's whole learning loop and
   published quality numbers key off it (their embedding filter over
   team-downvoted comments took address rate from 19% to 55%+).
6. **Incremental reviews need force-push handling.** Track the last-reviewed
   commit, review only the new commits on push, but fall back to a full
   re-review when that commit is no longer reachable (rebase/force-push) —
   kodus learned this the hard way.
7. **Read everyone's instruction files.** Devin Review deliberately reads
   AGENTS.md, CLAUDE.md, .cursorrules, .coderabbit.yaml and similar files as
   free repo context, scoping files in agent directories to their parent
   directory.
8. **Review methodology is tested, versioned engineering.** kodus renders its
   entire system prompt from code — category "lens" blocks with
   mission / focus / do-not-report / reasoning-policy / writing-policy lists
   that read like distilled postmortems — and covers the prompt builder with
   snapshot tests. Prompt text is where the product quality lives; treat it
   like code.
9. **Cap the noise mechanically.** Comment caps, severity thresholds, path
   excludes, and draft/author/branch filters are table stakes in every
   product. A 40-comment review is a failed review even if every comment is
   right.

## Decisions

1. **Scope: any PR in enrolled repos.** A general review bot, not just
   engrams-authored PRs. When the PR did come from an engrams task, that link
   unlocks autofix (below).
2. **The orchestrator is the sole GitHub writer.** Review sessions get a
   read-only clone credential and tight egress. They propose findings through
   ADR 0089 tools; the orchestrator validates and posts. A review session
   executes attacker-controlled input (the diff), so prompt injection must not
   be able to post, approve, or spend. Policy is enforced in code, not
   prompts.
3. **Sessions are ephemeral workers; the review record is the durable
   memory.** Every unit of LLM work is a session with a self-contained,
   orchestrator-rendered prompt. No orchestrator-side LLM inference, ever —
   all model judgment happens inside sessions; the orchestrator only runs
   deterministic code.
4. **The review runs as phases coordinated by a durable workflow**: a finder
   phase (high recall), a verifier phase (independent session, refute-to-drop),
   then a deterministic policy gate, then one batched GitHub review. Findings
   flow between phases through the database, written by tool calls as they
   happen.
5. **Two-axis finding model.** Category (six, below) + severity
   (`critical|high|medium|low`) + confidence (`high|medium|low`). Only
   confident findings post to the PR; the rest stay on the engrams review
   page.
6. **Batched review, COMMENT verdict.** One formal GitHub review per pass —
   inline comments plus a summary body — submitted as `COMMENT`. Advisory,
   never blocks merge. `APPROVE`/`REQUEST_CHANGES` are future work.
7. **Autofix is opt-in and budget-capped.** When enabled, posted findings are
   routed to the session that authored the PR (or a fresh fix session), which
   can push fixes and reply on finding threads. Agent-to-agent conversation
   happens *through the PR itself*, mediated and metered by the workflow.
8. **Setup is deterministic, not agentic.** The workflow prepares each review
   session itself — `Exec` runs the git clone, `WriteFiles` stages the
   instruction files — before the first prompt is sent. The agent wakes up to
   a ready workspace and spends zero tokens on mechanical setup; and because
   the clone already happened, the review session needs no network access at
   all in v1.
9. **The review record carries the PR's identity, not just its coordinates**
   (added 2026-07-25). `repo` + `pr_number` + two SHAs are enough to *run* a
   review and not enough to *read* one: every surface that reports a review has
   to say "cortexapps/engrams #881" where a human thinks "the quinn-proto bump".
   So the record also stores the PR's title, author, head/base branch, state,
   and diff size. It must be *stored* rather than read on demand, because the
   reviewer workers are network-clamped to a read-only clone credential and
   cannot fetch PR metadata themselves. Every field is nullable: reviews
   recorded before this decision keep only their coordinates, forever, so every
   consumer degrades to `repo #number` rather than assuming a title exists.

   **Capture happens at head resolution, and only there.** Every trigger already
   resolves the PR heads unconditionally — the trigger contract carries no
   `baseSha`, so `opened`, `synchronize`, `command`, `dispatch`, and `retry` all
   pass through the same `GET /repos/{repo}/pulls/{n}`, whose response already
   contains every field above and today discards all but two SHAs. That makes it
   the one choke point covering all five triggers with one code path. The
   `pull_request` webhook payload also carries the title, and an earlier revision
   of this decision proposed reading it there as well; that was dropped as
   redundant, and because widening the trigger message would have rotated the
   DBOS application version for no gain. For the same reason the control-plane
   method keeps the now-slightly-narrow name `resolvePrHeads` — it appears as a
   `step(...)` name inside the version-hashed workflow body — while the GitHub
   client method it delegates to was renamed `fetchPrContext`.
10. **Reviewer-session transcripts inherit the review's visibility** (added
   2026-07-25). Reviews are org-visible by decision; the sessions that produce
   them were not, because a `pr_review` task is inserted with a null owner and
   session reads are owner-scoped — so every non-admin got a 404 on the very
   transcript that would justify a finding. A session named as a review's
   `finder_session_id` / `verifier_session_id` is therefore readable by anyone
   who can read that review. This is a *derived* permission, resolved in the
   session guard from a review the caller can already see; it is deliberately
   not a rule in the ability set, because owner-scoped-plus-sharing is the
   OpenFGA threshold and this decision does not cross it. Nothing becomes
   writable: prompting, shell, and delete stay owner-scoped and therefore
   admin-only on reviewer sessions.

   Two properties worth stating plainly rather than implying. It widens `read`
   on the *whole* reviewer session — transcript, metadata, logs, artifacts — not
   only the transcript; that is acceptable because it is the same class of data
   the review page already shows, and it should not be described as narrower
   than it is. And the derived check is gated on the session resolving to *no*
   owner, which every `pr_review` worker does by construction, so an ordinary
   cross-owner denial short-circuits before the lookup and no session-id probe
   can turn a denial into a database query.

## Finding categories

Findings are labeled with exactly one category (this is CodeRabbit's public
taxonomy, adopted deliberately — it covers the field cleanly):

- 🔒 **Security & Privacy** — vulnerabilities, authentication and
  authorization flaws, secret handling, data exposure
- 🩺 **Stability & Availability** — crashes, unhandled errors, resource
  leaks, reliability risks
- 🗄️ **Data Integrity & Integration** — data correctness, persistence,
  schema, integration-boundary issues
- 🎯 **Functional Correctness** — logic errors, incorrect behavior, unhandled
  edge cases
- 🚀 **Performance & Scalability** — inefficiencies, bottlenecks, scalability
  concerns
- 📐 **Maintainability & Code Quality** — readability, structure, naming,
  best practices

Each category has a "lens" file in the methodology (kodus's shape): a mission
statement, a focus list of concrete defect patterns, an explicit
do-not-report list, a reasoning policy ("trace execution, don't pattern
match"), and a writing policy (every finding body is WHAT / WHEN — what the
problem is in one sentence, and when the issue can be hit, explained clearly;
a concrete fix goes in the separate `suggested_fix` field only when it is
clear from code actually read).

## Architecture

```
GitHub App ──webhooks──▶ POST /api/v1/integrations/github/events   (verify signature, ack fast)
  pull_request.{opened,synchronize,ready_for_review,closed}
  pull_request_review_comment.created · issue_comment.created
        │  idempotent startWorkflow + DBOS.send (delivery-id dedupe)
        ▼
  PrReviewWorkflow  (one per PR; workflowID = review:<repo>#<number>)
        │ phases: finder session(s) → verifier session → policy gate → post
        │ conversation: dirty set + sweep sessions
        │ autofix: fix prompts to the authoring task
        │
        │ CreateTask / Exec (clone) /                  tool calls (ADR 0089)
        │ WriteFiles / SendPrompt                            │
        ▼                                                   │
  ephemeral review sessions (reviewer profile:              │ submit_finding
  pre-cloned, no write creds, no egress)  ──────────────────┘ submit_verdict, …
        │
        ▼
  review record (Postgres) ──▶ GitHub posting (orchestrator octokit)
        │
        ▼
  /reviews page in the web UI
```

The skeleton is the Slack integration's (ADR 0060): an idempotent webhook
handler, one durable DBOS workflow per conversation draining a single mailbox
that multiplexes session events and trigger events, and a per-source
communication policy. The tool plumbing is ADR 0089: the reviewer's tools are
registered orchestrator tools (`handled` + `sync`, the papercut shape), so
each tool call is persisted and answered without any new protocol work.

### The GitHub edge

- **One GitHub App** serves both planes: credential minting (the existing
  `github_app` mint kind gives sessions their scoped tokens) and webhooks
  (new). The orchestrator gets its own octokit client — App private key and
  webhook secret stored as KEK-sealed org secrets, installation tokens minted
  orchestrator-side. The webhook handler verifies the signature, dedupes by
  delivery id, and acks without calling the GitHub API.
- **Enrollment** lives in engrams (org settings → integrations → GitHub):
  per-repo rows `{repo, trigger_mode: auto|manual, autofix: auto|manual|off,
  profile_id?}`. Draft PRs are skipped until marked ready. Events for
  un-enrolled repos are dropped at the edge.
- **Dispatch API**: `POST /api/v1/reviews/dispatch {repo, pr_number}`
  (bearer-authed) starts or pokes the same workflow, for teams that want
  CI-conditional triggering. A small reference GitHub Action wrapping it
  ships in-repo.
- **Commands on the PR** (deterministic string match against our mention —
  never LLM-classified):
  - `@engrams review` — run/re-run a review.
  - `@engrams review <free text>` — same, with the text injected as a focus
    directive (raises depth on the named area; never suppresses findings
    elsewhere — priority, not a filter; sanitized and length-capped).
  - `@engrams fix <text>` / `@engrams fix this` on a thread — route to fix
    handling.
  - `@engrams stop` — halt autofix and auto-review for this PR.

### The review record

Three readers (the web UI, autofix routing, prompt building) means this state
earns durable tables:

- `pr_ref` — `{repo, pr_number, authoring_task_id?}`. Written when an
  authoring session's PR creation is observed; the link that makes autofix
  possible.
- `review` — one row per review pass: `{id, repo, pr_number, task_id,
  head_sha, base_sha, trigger, status:
  queued|finding|verifying|posted|failed|superseded, github_review_id?,
  summary_md?}`.
- `review_finding` — one row per finding: `{id, review_id, path, start_line?,
  end_line?, side?, category, severity, confidence, title, body_md,
  suggested_fix?, state, verdict_reason?, github_thread_id?, resolution?}`.
  `state` walks: `candidate → confirmed | suppressed_refuted`, then
  `posted | ui_only | suppressed_by_config | superseded`. Nothing is ever
  deleted — suppressed findings stay visible in the UI with the reason.
- `review_verdict` — the verifier's judgment per finding: `{finding_id,
  verdict: confirmed|refuted, confidence, reasoning}`.

Review work is a task (`task.type = "pr_review"`) that spans the PR: each
phase session attaches to it via `task_session`, and it closes when the PR
closes. Per-pass state lives on the `review` row; the UI reads reviews, not
task status.

### The tools

All `handled` + `sync` (ADR 0089): the handler validates against a zod
schema, writes the row, and returns — replay-safe on
`(session_id, tool_call_id)` like the papercut tool.

- `submit_finding` — finder sessions report candidates:
  `{path, start_line?, end_line?, category, severity, confidence, title,
  body_md, suggested_fix?, evidence: [files actually read]}`. Persists a
  `candidate` row immediately, so the UI shows findings streaming in while
  the review runs.
- `finder_done` — `{summary_md}`. Marks the finder phase finished from the
  session's side.
- `submit_verdict` — verifier sessions judge candidates:
  `{finding_id, verdict: confirmed|refuted, confidence, reasoning}`.
- `update_finding_status` — incremental passes report resolutions:
  `{finding_id, resolution: addressed|still_open|no_longer_applies,
  note?}`.
- `reply_to_review_thread` — `{thread_ref, body_md}`. Available to sweep
  sessions (answering humans) and, in autofix mode, to the authoring session.
  The orchestrator posts the reply; free text from a session never reaches
  GitHub.

### WriteFiles (new primitive, small)

`SessionService.Exec` exists today but has no stdin field, so pushing content
into a guest means embedding it in a shell command string — quoting hazards
and size limits. The as-built API adds one RPC, plumbed through the same
coordinator → host → sandbox path as Exec:

- `SessionService.WriteFiles {session_id, files: [{path, content, mode?}]}` —
  a batch in one round trip; a batch of one is the single-file case. The
  response reports per-file success so a partial failure is visible and
  retryable.

The host-side backend opens one guest connection per file and reuses agentd's
existing `Upload {path, bytes, mode}` verb. No new append-only agentd wire
variant is added, so already-baked guest images support `WriteFiles` and no
image rebake is required.

The review workflow uses them, as a durable step, to stage a session before
its first prompt:

- the reviewer instruction files (`/workspace/.review/…`, next section) —
  rendered fresh per review, with org instructions already merged in;
- the verifier's `candidates.json`;
- prior-findings context for incremental passes.

Why files instead of a baked skill bundle: bundle content is frozen at
bake/enable time, while these files are composed per run (config changes take
effect on the next review, and instruction iteration ships with an
orchestrator deploy, not a bundle republish). Files in `/workspace` also
survive evict/resume and are readable by any subagent the harness spawns.

### The reviewers folder: instructions as files in the orchestrator

The review instructions are not string literals in TypeScript — they live as
markdown files in the orchestrator source tree and are uploaded into each
session verbatim (plus merge slots):

```
orchestrator/reviewers/
  finder.md            # the finder's full instructions (mindset, workflow, hard rules)
  verifier.md          # the verifier's refute-to-drop instructions
  sweep.md             # answering humans on finding threads
  lenses/
    security-privacy.md
    stability-availability.md
    data-integrity-integration.md
    functional-correctness.md
    performance-scalability.md
    maintainability-quality.md
```

Each lens file has the five-part shape (mission / focus list / do-not-report /
reasoning policy / writing policy). The renderer is deliberately thin: it
loads the role file and the enabled lens files, fills the merge slots (org
instructions, enabled categories, the tool contract), and hands the result to
`WriteFiles` as `/workspace/.review/…`. Snapshot tests cover the rendered
output; wording changes show up as reviewable markdown diffs, because this
text is where the product's precision/recall actually lives (the kodus
discipline — their category lenses read like distilled postmortems, and every
production false negative should become a new focus-list bullet here).

The per-session system prompt (`ENGRAM_APPEND_SYSTEM_PROMPT`) is then tiny
and stable: the role line, the binding pointer — *"Before anything else, read
`/workspace/.review/finder.md` and follow it exactly; its rules are binding
for this session"* — and the handful of hard rules that deserve system-prompt
authority (never edit files, never push, findings only via the tools).

### Instructions and configuration, layered

Two kinds of customization, delivered differently:

**Settings** (machine-enforced knobs, applied by the orchestrator in the
policy gate): severity posting threshold, comment cap, category toggles, path
excludes, autofix defaults, trigger mode. Org-level rows with per-repo
overrides.

**Instructions** (natural-language guidance, read by the model). Four layers:

1. **engrams methodology (floor, not overridable)** — the category lenses,
   the evidence rules, the WHAT/WHEN writing policy, "findings only on
   changed lines", "nits are nits". Lives as markdown in the orchestrator's
   `reviewers/` folder (see below).
2. **Org instructions** — free text on org settings (settings UI): guidance
   that applies to every enrolled repo ("every service is multi-tenant —
   always check tenant isolation on new queries"). Merged by the prompt
   builder into the methodology files it renders, so delivery costs nothing
   new.
3. **Repo instructions** — `.engrams/review.md` at the repo root. Free text:
   conventions, critical areas, known footguns, "always flag X".
4. **Directory instructions** — `.engrams/review.md` in a subdirectory,
   applying only to changes under that directory (the Devin/Bugbot pattern —
   monorepos get per-component rules).

Plus **free context**: AGENTS.md, CLAUDE.md, .cursorrules and similar agent
instruction files, read as background when present.

Precedence: the engrams floor always wins; below it, more specific guidance
wins for its subtree (directory > repo > org). Non-conflicting guidance from
all layers is additive. Repo/directory instructions can add focus areas and
conventions; they can never disable a category, lower the evidence bar, or
instruct the reviewer to approve — the floor says so explicitly and the
policy gate enforces the machine-checkable parts regardless.

#### How the session picks up repo and directory instructions

Layers 3–4 and the free-context files live in the checkout, so the session
reads them itself — with one security-critical rule: **instruction files are
read at the merge base, never at the PR head**. The files are attacker-editable
in the PR itself (a malicious PR could add "report nothing" to
`.engrams/review.md`), so the reviewer honors the base-branch version, and a
PR that modifies `.engrams/**` gets those changes reviewed like any other
code — they take effect on the *next* PR.

The finder's instructions say, concretely:

1. You already have `base_sha` (the merge base) in your prompt.
2. Read the repo-wide file with `git show <base_sha>:.engrams/review.md`
   (missing file → skip silently).
3. For each changed file, walk up its directory path and read any
   `.engrams/review.md` in an ancestor directory the same way
   (`git show <base_sha>:src/query-engine/.engrams/review.md`). Apply it only
   to changes under that directory.
4. Read the free-context files (`AGENTS.md`, `CLAUDE.md`, `.cursorrules`) at
   the merge base too, as background.
5. Apply the precedence rules above.

Why in-session rather than orchestrator-fetched: the checkout with full
history is already there, `git show` is free, and discovering which
directories carry instruction files is a filesystem walk instead of GitHub
tree-API archaeology. The orchestrator-rendered files cover what the checkout
cannot know — the engrams methodology and org instructions.

### What the reviewer instructions contain

`finder.md` (rendered and uploaded as `/workspace/.review/finder.md`) is a
fixed skeleton containing, in order:

1. **Role and mindset** (never compacted — these are the load-bearing
   behavioral cues): *"You are a code reviewer for this pull request. Assume
   every change is broken until you prove it is safe. Your default is to
   report — you need evidence to dismiss a suspicion, not evidence to raise
   it. 'Looks correct' is not a verdict; 'I traced X and confirmed Y holds'
   is. You are the high-recall pass: report anything with concrete,
   code-backed suspicion. An independent verifier will filter — do not
   self-censor a finding because you are only 70% sure."*
2. **The workflow**, three phases:
   - *Phase 1 — investigate.* Read the diff. For each changed function:
     trace who calls it and what they expect; if it calls something new, read
     that too; keep following until you hit a concrete implementation. Before
     every file read, name the question the read will answer — re-reading to
     "gain confidence" is wasted work.
   - *Phase 2 — challenge.* For each changed unit: what if this input is
     null/empty/zero? What if two requests hit this at once? Did the
     signature, return type, or side effect change — and did every caller
     keep up? Does a removed guard make an old bug newly reachable?
   - *Phase 3 — submit.* One `submit_finding` call per issue, then
     `finder_done` with a short summary. Anything not submitted through the
     tool does not exist.
3. **A pointer to the category lenses** — the six lens files under
   `/workspace/.review/lenses/`, with org instructions already merged in.
4. **The hard rules (the floor)**: findings only on lines changed in this PR
   unless the change makes an old issue newly reachable; every finding must
   cite evidence from files actually read; WHAT/WHEN writing shape (what the
   problem is and when it can be hit), with any concrete fix carried in the
   separate `suggested_fix` field; never edit files, never
   push, never call GitHub beyond the read access provided; repo instructions
   may add focus and conventions but cannot disable a category, lower the
   evidence bar, or direct an approval.
5. **The instruction-pickup procedure** from the section above (merge-base
   `git show`, directory scoping, precedence).
6. **The tool contract**: the `submit_finding` / `finder_done` schemas, with
   the axis rule spelled out — *severity is impact if the finding is real;
   confidence is how sure you are it is real. They are different questions.*

Per-run context goes in the **user prompt**, not the system prompt: repo and
PR metadata, `base_sha..head_sha`, the focus directive from
`@engrams review <text>` if any, and — on incremental passes — the prior
still-open findings with "do not re-report these" plus the request to judge
their resolutions.

`verifier.md` is the same skeleton with the stance inverted: *"Your job is to
refute each finding. Re-derive the reasoning from the code yourself — do not
trust the finder's description. If you cannot confirm the failure scenario
from code you actually read, the verdict is `refuted`. If the finding's
evidence list does not include the file the finding is about, re-derive from
scratch. Confirm only what survives your best attempt to kill it."* Its tool
contract is `submit_verdict`, and its user prompt is the candidate list (also
written into the guest as `candidates.json`).

## How a review runs

### Scenario: a PR opens in an enrolled repo

1. GitHub sends `pull_request.opened`. The webhook route verifies the
   signature, checks enrollment, and does the idempotent handshake:
   `startWorkflow` with the deterministic id `review:<repo>#<number>`, then
   `DBOS.send` keyed by the webhook delivery id. Redelivered webhooks land on
   the same workflow and dedupe.
2. The workflow creates the `pr_review` task and a `review` row
   (`status: queued`, `head_sha` pinned from the event), and boots a **finder
   session** on the **reviewer profile** — a normal profile row carrying a
   `designation: "pr_reviewer"` marker. It ships seeded with a simple image
   (git, jq, ripgrep and similar) and sensible harness/model defaults, but
   because it is a real profile, org admins can modify it in the profiles UI
   like any other (swap the image, model, effort, skills); the designation is
   how the workflow finds it and why it can't be deleted. Per-repo enrollment
   can point at a different profile. The tool manifest is
   `{submit_finding, finder_done}`.
3. The workflow prepares the workspace itself, as durable steps — the agent
   never does mechanical setup:
   - `Exec`: `git clone` the repo at `head_sha` (full history, so merge-base
     `git show` works) into `/workspace/<repo>`, using the session's scoped
     read-only credential. A deterministic step that retries cleanly; a
     failed clone is an infra error on the review row, never an agent
     giving up.
   - `WriteFiles`: the rendered reviewer instructions into
     `/workspace/.review/`.
   Because the clone is already done, the finder session needs no network
   access at all.
4. The workflow sends the prompt: repo, PR title/description,
   `base_sha..head_sha`, where the instructions live, and the marching
   orders. The finder wakes to a ready workspace, reads the diff against the
   merge base, reads `.engrams/review.md` and the free-context files (at the
   merge base — see the instruction-pickup rules), and investigates —
   tracing callers, reading surrounding code, checking git history. Its
   stance is high recall: report anything with concrete, code-backed
   suspicion; a verifier will filter. Every suspicion becomes a
   `submit_finding` call; every call lands as a `candidate` row visible in
   the UI in real time. It finishes with `finder_done`.
5. The workflow sees the finder session end. If there are zero candidates, it
   skips ahead to posting a short "no findings" summary. Otherwise it boots a
   **verifier session** — a fresh session with no memory of the finder's
   reasoning, which is the point: genuine skepticism needs an independent
   look.
6. The verifier gets the same deterministic setup (`Exec` clone,
   `WriteFiles` with `verifier.md` and `candidates.json`) and a
   refute-to-drop prompt: for each candidate, actively try to prove it
   wrong; confirm only what survives; check the evidence (did the finder
   actually read the file it cites — an unread citation means re-verify from
   scratch). Each judgment is a `submit_verdict` call.
7. The verifier session ends. The workflow runs the **policy gate** —
   deterministic code, no LLM (details below) — which decides, per finding:
   post inline, keep UI-only, or suppress, and computes the summary counts.
8. The workflow posts **one GitHub review** (`COMMENT`): the summary body
   (verdict line, per-category counts, a link to the engrams review page, and
   a hidden marker `<!-- engrams-review:<id> -->`) plus the inline comments —
   each with its category emoji, severity, WHAT/WHEN body, and a
   ` ```suggestion ` block when a committable fix was provided.
9. Thread ids from GitHub's response are stamped back onto the finding rows —
   they're the correlation anchors for conversation later. `review.status =
   posted`. The review page shows everything: posted findings, UI-only
   findings, and refuted candidates with the verifier's reasoning.

Two sizing notes. First, phase count is configuration, not architecture: the
workflow runs a phase list, so a `thorough` profile can fan the finder out
into parallel per-category sessions (running many microVMs at once is
literally our product), while a docs-only PR can run a single finder with
inline self-verification. Second, the whole pass targets the same "a few
minutes" window every vendor operates in; phases serialize but each is
short-lived.

### The policy gate (v1: deliberately minimal)

Deterministic code between verdicts and posting. In v1 it does five things:

1. **Fold verdicts**: refuted → `suppressed_refuted` (reasoning kept);
   confirmed → carry the verifier's confidence (the finder's was advisory);
   no verdict (verifier died mid-list) → treat as low confidence, never post
   unverified.
2. **Normalize**: canonicalize paths (strip the guest workspace prefix),
   clamp line ranges, validate enums.
3. **Config filters**: category toggles, severity threshold, path excludes →
   `suppressed_by_config`; confidence below the posting bar → `ui_only`.
4. **Comment cap**: rank by severity then confidence; overflow past the cap
   (default 10) → `ui_only`, with one summary line: "N more findings on the
   review page".
5. **Post, with GitHub as the anchor validator**: submit the review; if
   GitHub rejects a comment's anchor (422 — the line isn't in the diff),
   demote that finding one rung and resubmit: inline → file-level comment →
   quoted block in the summary. Findings are demoted, never dropped, and the
   `disposition` is recorded. The hidden summary marker makes posting
   idempotent — on workflow replay, an existing review with the marker means
   "already posted".

Proactive anchor validation (parsing diff hunks ourselves, snapping
near-miss line numbers, mapping lines across pushes) is described under
Future follow-ups; v1 leans on GitHub's own validation plus the demotion
ladder, which is ~20 lines and fails soft.

### Scenario: new commits are pushed

1. `pull_request.synchronize` arrives; the workflow sets a new-commits flag
   in its dirty state. If a review pass is mid-flight it finishes against its
   own pinned `head_sha` first (its comments simply show as "outdated" where
   the push moved things); the flag then triggers the next pass.
2. The workflow compares: is the previous review's `head_sha` still reachable
   from the new head? If yes → **incremental pass**: the finder is prompted
   with the commit range `old_head..new_head`, plus the prior still-open
   findings ("do not re-report these"). If no (rebase or force-push) → full
   re-review, which is safer than diffing against a commit that no longer
   exists.
3. The incremental pass also judges the prior findings: the session reports
   `update_finding_status` per prior finding (`addressed`, `still_open`, or
   `no_longer_applies`) — resolution is an in-session judgment, recorded by
   the orchestrator.
4. The prior review row is marked `superseded`; the new pass posts as usual.

### Scenario: a review session dies

1. The finder (or verifier) session ends without its finish tool call, or its
   harness run errors (idle + `run_failed`).
2. The workflow marks the review `failed`, posts a single "review failed" PR
   comment, and surfaces the failure on the review page. Never silently green.
3. Re-running is an explicit action, not an in-workflow retry (see the
   divergence note below): the `/reviews` **Retry** button (or the dispatch
   endpoint) mints a fresh review record + workflow epoch over the PR's current
   head. The failed row stays as history.

## Conversation on the PR

Humans reply to finding threads or mention `@engrams`; the reviewer answers
in-thread. The mechanism is a **dirty set + sweep** (prompts are rendered
fresh, never forwarded webhook payloads):

1. Comment webhooks mark state in the workflow: which threads have new
   activity, whether there's a new general PR comment. Our own posts echo
   back as webhooks and are filtered out by their hidden markers.
2. Command-shaped comments (`@engrams fix …`, `@engrams review`,
   `@engrams stop`) peel off first via exact string matching and route to
   their handlers — they never enter the sweep.
3. When the dirty set is non-empty, no review session is mid-turn, and a
   short debounce (~20–30s) has passed, the workflow fires one **sweep**: it
   re-fetches every dirty thread from GitHub (source of truth at
   prompt-build time) and renders a self-contained prompt — per thread: the
   finding, the code context, the full conversation, and a `thread_ref`.
4. The sweep goes to a review session — resumed if one is still warm, booted
   fresh otherwise. Because the prompt is self-contained, a fresh session
   picks up mid-conversation with no loss. Replies come back as
   `reply_to_review_thread {thread_ref, body_md}` calls; the workflow maps
   each to its GitHub thread and posts. An unknown `thread_ref` is rejected
   at the schema edge.
5. The dirty flags clear only after the sweep prompt is successfully
   delivered; a delivery failure leaves them dirty for the next sweep. Bursts
   batch naturally — five quick replies become one sweep turn.

## Autofix: closing the loop with the authoring session

The field converged on closing the loop (Devin's autofix responds to review
comments; Bugbot spawns fix agents). Our version routes findings to the
session that *wrote* the PR, because it still has the context of why the code
looks the way it does.

The key architectural choice: **the PR is the bus.** The authoring task and
the review workflow never exchange session events directly. Both act on
GitHub (commits, thread replies); the review workflow watches the webhooks
and does all routing, metering, and prodding. Loose coupling — the authoring
session may be a chat task, a Slack-thread task, or long dead; the workflow
only needs `pr_ref.authoring_task_id` and the existing prompt-delivery path
(idle-evicted sessions resume on prompt delivery as usual).

### Scenario: autofix round

1. A review pass posts findings. The repo's enrollment has
   `autofix: auto` (or someone presses "Send to authoring task" on the
   review page — the same path, manually triggered).
2. The workflow looks up `pr_ref.authoring_task_id`. Present → it renders a
   **fix prompt**: every posted finding with its body, severity, and
   `thread_ref`, plus the instruction — *for each finding, either fix it and
   push, or reply on its thread explaining why you disagree or what you need
   clarified.* Delivered via the existing prompt path, idempotency key
   `review:<review_id>:autofix`. The authoring session also gets
   `reply_to_review_thread` added to its tool manifest.
3. The authoring session wakes with its original context, makes the changes,
   pushes to the PR branch (it already has branch write credentials — that's
   how the PR exists; autofix grants nothing new), and/or replies on threads
   it disputes.
4. The push arrives as a normal `synchronize` webhook → incremental re-review
   → resolutions recorded → remaining findings posted.
5. The loop repeats until no posted findings remain above threshold — or the
   **round budget** (default 2 autofix rounds per PR) is spent, at which
   point the workflow stops prodding and the summary states what's still
   open. Budgets are the guard against two agents ping-ponging forever.
6. No authoring task (a human PR), or its session can't be resumed → autofix
   instead creates a fresh fix task on the PR branch with the same prompt
   (configured profile or org default), and the loop is identical from
   there.

### Scenario: the authoring agent asks the reviewer a question

1. During an autofix round, the authoring session disagrees with a finding:
   it calls `reply_to_review_thread` — "this null case is unreachable
   because X; what input do you think triggers it?"
2. The orchestrator posts the reply with a role marker
   (`<!-- engrams:author-agent:<task_id> -->` plus a visible badge line —
   both agents post through the same GitHub App, so markers are how anyone,
   including us, tells them apart).
3. The comment webhook comes back. The workflow's dirty-set filter is
   role-aware: reviewer-authored comments never dirty anything (self);
   author-agent comments dirty the sweep (they're addressed to the
   reviewer); human comments dirty the sweep and never count against agent
   budgets.
4. The next sweep answers on the thread — concretely, with the input that
   triggers the bug, or by conceding ("you're right, `no_longer_applies`" via
   `update_finding_status`).
5. The reviewer's answer is forwarded back to the authoring task as a prompt
   only if autofix is active and the per-thread exchange budget (default 2
   turns per side per push cycle) isn't spent. Otherwise the answer just sits
   on the PR for humans.
6. A human comment or `@engrams stop` halts autofix for the PR at any time.

### Security posture, stated honestly

The review session is an untrusted-input sandbox — the diff, PR description,
and comments are attacker-controlled. Containment, enforced in code:

- No write credentials; the clone credential is read-only, repo-scoped, and
  only exercised during the workflow-driven clone step — after setup the
  review session has no network access at all.
- Mediated output only: the tool manifest is the session's sole effect
  channel. The orchestrator enforces comment-verdict-only, this-PR-only, and
  schema validation regardless of what the session asks for.
- Instruction files are read at the merge base, so a PR cannot rewrite the
  reviewer's instructions for its own review; edits to `.engrams/**` are
  themselves reviewed and take effect on the next PR.
- Autofix deliberately relaxes the old draft's rule that reviewer output may
  never reach a write-capable session. What makes that acceptable: it's
  opt-in per repo and off by default; the fix prompt is *rendered by the
  orchestrator from structured finding rows* that survived verification and
  the policy gate — reviewer free text never flows raw into a write-capable
  prompt; the blast radius is exactly what the authoring session could
  already do (push to that one PR branch, still behind human merge review);
  and budgets plus `@engrams stop` bound the autonomy.

## Web UI

`/reviews` is served by the `ReviewService` proto (List/Get/Retry) through the
standard chain — proto → native Connect service → generated connectquery client
→ hook → page. Reviews are org-visible: a team dashboard, not a personal list.

The first implementation shipped the list-plus-expanding-row shape sketched in
the original revision of this section: one table row per review, expanding to
reveal every finding sorted by severity. Field use showed three problems, and
this section was rewritten on 2026-07-25 to fix them.

- One PR occupies several rows, because a retry and every `synchronize` push
  mint a new review record. The table repeats the PR and buries which pass is
  current.
- A review had no name. `cortexapps/engrams #881` is a coordinate, not a
  subject — hence decision 9 above.
- The finding list flattened the two things a reader most needs separated:
  what the finder claimed, and what the verifier did about it. `ui_only`
  rendered as one label, "shown here only", while actually meaning four
  different things.

**A PR is the record; a pass is an entry in it.** `/reviews` becomes a section
shell with a persistent rail, matching the `/sessions` convention. Rail rows are
PRs — reviews grouped by repo + PR number, showing the latest pass's state — so
opening one moves a highlight rather than swapping the layout. The section index
is the fuller ledger: the same PR-grouped list, filterable by repo, status, and
severity, for the scan-many job a rail cannot do.

**The dossier at `/reviews/$id` is addressed by pass, not by PR.** The id stays
a review record's id so the hidden marker in the posted summary
(`<!-- engrams-review:<id> -->`) can deep-link the exact pass that produced a
comment; the dossier shows the PR's context around it and lists sibling passes
as history, with a forward link when a newer pass exists. It composes as: PR
context header → verdict band → progress → findings → pass history → actions.

**The verdict band states the kept-vs-killed ratio.** "5 of 7 kept · verifier
refuted 2" is the number that tells a reader whether to trust the output, and
the first implementation did not show it anywhere. It is the dossier's headline
on a finished pass.

**Progress has two weights, not two homes.** While a pass is queued, finding, or
verifying, there are no stable findings yet and progress *is* the content: the
activity log takes the verdict band's slot, full width, current step live. Once
the pass is terminal, findings are the content and the log collapses to a single
line — step count, total duration, per-phase split — expanding in place. Step
durations continue to derive from adjacent `review_event` rows rather than a
wall clock, and a failure surfaces the event's `detail` string ("finder phase
deadline expired") rather than a bare status.

**Findings group by outcome, and the not-posted reasons are derived, not
stored.** Posted-to-the-PR leads; then not-posted, split by reason; then refuted,
collapsed behind a disclosure that names the count. The split is computed from
rows the client already has, so no column is added:

| Reason | Derivation |
| --- | --- |
| Unverified | no verdict row for the finding |
| No line anchor | verdict `confirmed`, `start_line` and `end_line` both null |
| Over the comment cap | verdict `confirmed`, anchored, state not `posted` |
| Refuted | verdict `refuted` (its own collapsed group) |

The 422 batch-fallback deliberately folds into "over the cap" rather than
earning a persisted disposition — it is rare, it demotes the whole batch at
once, and separating it would buy a schema field for a distinction no reader
acts on differently.

**Each finding card carries two attributed voices and never blends them.** The
finder's claim is the body — its WHAT/WHEN prose, file anchor, category, the
evidence file list that is its receipts, and its own confidence, which is a
different axis from severity. The verifier's ruling sits below it as a visibly
separate annotation. `suggested_fix` renders as code, because GitHub receives it
as a committable suggestion block, so prose there is a bug in the finder's
output rather than something the UI should smooth over.

Two states the UI must not get wrong: a failed or mid-flight pass leaves every
finding at `candidate`, since only `postReviewResults` advances them — so
`candidate` is a common terminal state, not a transient one. And `confirmed`,
`suppressed_by_config`, and `superseded` are never written today; the UI does not
invent labels for them.

**The activity log is the way into the transcripts.** Decision 10 makes reviewer
sessions readable, and the milestones that name a session — `finder_started`,
`cloning`, `reviewing` for the finder; `verifier_started`, `verifying` for the
verifier — are the click targets that open that session's thread in a side pane,
with an escape hatch to the full session page. Control-plane milestones
(`queued`, `posted`, `failed`, `halted`) have no session behind them and stay
inert. The pane reuses the existing session-transcript component and its event
subscription unchanged, and shows the transcript only: the reviewer VM is
destroyed at phase end, so a shell or browser tab would be dead by
construction. The transcript itself survives, because tearing down a session
flips its status and destroys its sandbox without deleting its event log.

Known limitation, accepted rather than fixed here: the event subscription
replays a bounded first page and then tails live, so a finished session with a
very long transcript is truncated at that bound. This is pre-existing behavior
shared with the session detail page and is tracked separately; the review pane
inherits it rather than forking a second event-reading path.

The autofix surfaces this section originally listed — round history and the
"send to authoring task" control — remain P2 work and land in the dossier's
actions and pass history when that phase ships; the composition above leaves
room for them rather than specifying them now.

## Failure modes

| Failure | Handling |
| --- | --- |
| Webhook redelivery / duplicates | delivery-id keys on `DBOS.send`; deterministic workflow id per PR |
| Finder/verifier dies mid-phase | one fresh-session retry, resuming from rows already written; second failure → `failed` status + one PR comment |
| GitHub rejects a comment anchor (422) | demote one rung (inline → file-level → summary quote), resubmit; disposition recorded |
| Crash between GitHub post and checkpoint | hidden summary marker makes posting idempotent on replay |
| Push mid-pass | pass completes against its pinned `head_sha`; the synchronize flag triggers the next pass |
| Force-push / rebase | last-reviewed commit unreachable → full re-review |
| Reply burst during a sweep | dirty set accumulates; next sweep covers all of it |
| Agent ping-pong under autofix | round budget + per-thread exchange budget + `@engrams stop` |
| PR closed mid-review | workflow cancels outstanding work; merged-vs-closed recorded |

## Testing

- Workflow logic (phase sequencing, dirty set, budgets, peel-off, role
  routing): orchestrator unit tests with injected fakes — the ADR 0060
  pattern, no live DBOS engine.
- The instruction renderer: snapshot tests; wording edits show up as reviewable
  diffs.
- Tool handlers: schema + replay-idempotency tests (the papercut suite as
  template).
- Webhook handler: signature, dedupe, enrollment routing.
- One e2e-stack assertion: dispatch API → review posted against a recorded
  GitHub fixture, proving the loop end to end.

## Phasing

- **P0 — the review pass**: GitHub App + webhook route + enrollment,
  `WriteFiles`, the seeded reviewer profile (designation marker),
  the `reviewers/` folder + renderer, the deterministic setup steps (Exec
  clone + WriteFiles), finder/verifier phases with `submit_finding` /
  `submit_verdict` / `finder_done`, the v1 policy gate, batched posting,
  tables, minimal `/reviews` list. Manual trigger (`@engrams review` +
  dispatch API) first; auto-on-open once the loop is proven.
- **P1 — conversation + incremental**: dirty set + sweeps,
  `reply_to_review_thread`, re-review on push with force-push fallback,
  `update_finding_status` resolutions, focus directives.
- **P2 — autofix**: `pr_ref` capture, fix prompts to the authoring task,
  fresh-fix-session fallback, role-aware routing + budgets, the UI button.
- **P3 — depth**: per-directory config, review detail page polish, the
  follow-ups below as they earn priority.

## Future follow-ups (described, deliberately deferred)

1. **Proactive deterministic anchor validation.** Fetch the PR diff
   ourselves, parse patch hunks into per-file sets of commentable
   (side, line) positions, and classify every finding *before* posting:
   exact hit → inline; within a few lines of a hunk edge → snap into the
   hunk (off-by-a-few anchors are the most common agent error, and snapping
   rescues them deterministically); file changed but line not → file-level;
   file not in diff → summary quote. This upgrades the 422-reactive ladder
   into a pre-validation gate, doubles as a stronger injection filter
   (anchors validated against *our* fetch, not the session's claims), and
   enables cross-pass dedup by mapping prior findings' line ranges forward
   through the hunks of `old_head..new_head` before matching on
   (file, overlapping range, category). The fiddly part is the hunk parser
   (renames, deletions, truncated patches on huge PRs) — it needs a recorded
   fixture suite, which is exactly why it's deferred rather than rushed.
2. **Learnings from feedback.** Capture 👍/👎 reactions and thread outcomes
   per finding; suppress finding-patterns a team consistently rejects.
   Greptile's proven shape: embed past findings with their outcomes, block a
   new finding only when it's similar to several distinctly-downvoted ones,
   and never suppress security/correctness categories. Requires the
   address-rate data below.
3. **Address-rate analytics.** Per repo and per category: how many posted
   findings were addressed before merge. The one metric the field agrees
   predicts reviewer quality; it also tells us whether config is too loose or
   too tight (suppression stats come free from the state machine).
4. **`APPROVE` / `REQUEST_CHANGES` verdicts** as per-repo opt-in, once
   precision is proven (kodus refuses to auto-approve a degraded run — same
   rule would apply).
5. **Execution validation** — for findings that claim runtime behavior, let
   the verifier actually run the code path in its sandbox and attach the
   evidence (Greptile's TREX, Devin's Security Swarm both landed here; we
   already own the sandbox).
6. **Multi-forge (GitLab)** via the same communication-policy seam.
7. **A responder tier** — a slim no-clone profile for answering simple thread
   questions, if sweep volume makes full sessions measurably wasteful.

## Implementation divergences

- **The operation graph stays inline in the registered function.** DBOS derives
  the application version from registered workflow function source
  (`computeAppVersion` → `origFunction.toString()`, which does **not** recurse
  into module-level helpers) and replays an in-flight workflow only against code
  of its own version. So the recv loop and the order + names of every `step(...)`
  call live inline in `prReviewWorkflowImpl`: a change to the graph rotates the
  version, and DBOS version-gates replay rather than running a recovered review
  through a changed graph (which would raise `DBOSUnexpectedStepError` or take a
  wrong branch). The heavy work stays behind the injected `ReviewControlPlane` —
  the `ToolExecWorkflow` shape (graph in the body, logic in the functions the
  steps call; a completed step is memoized on replay, so its internals evolve
  freely). An earlier revision hoisted the graph into plain module functions for
  a "hash-stable" thin shell; that was reverted after review — it moved
  replay-sensitive control flow *out* from under versioning, so a later
  helper-only edit would ship under an unchanged version and be replayed against
  a changed graph. (This leaves the standing computed-hash trade-off: a graph
  change rotates the version and strands in-flight reviews until drained — the
  broader fix is explicit `applicationVersion` / DBOS patching, tracked
  separately.) The step vocabulary is kept deliberately coarse so the body reads
  as a short sequence of high-level steps: each terminal or phase-boundary action
  is one control-plane step that folds in the worker teardown — `failReview` /
  `haltReview` (best-effort teardown + status change, with the reason recorded on
  the activity log), `concludeFinderPhase` (retire the finder + report its
  candidate count), and `postReviewResults` (retire the verifier + post). Phase
  *setup* stays three granular steps (create/bootstrap/prompt) on purpose — those
  are distinct, expensive, non-idempotent checkpoints, and collapsing them would
  re-create sessions or re-clone on crash recovery.
- **No in-workflow retry (supersedes the "session dies" retry-once above).** A
  dead/errored phase marks the review `failed` immediately. Re-running is an
  explicit `ReviewService.RetryReview` RPC (the `/reviews` **Retry** button) or
  the dispatch endpoint, both of which re-enter `dispatchReview` → a fresh
  review record (a terminal review is not "active") + successor workflow epoch.
  This removed the retry counters, the per-phase deadline-window counting (now
  one `recv` window is the phase deadline), and `deleteFindingsForSession` (the
  retry-only finding-dedup step), which is dropped from the control-plane seam.
- **`RetryReview` authz.** Gated as `create` on the `Review` subject — any
  authenticated member, mirroring "any member can trigger a review by command";
  enrollment mutations stay admin-only.

## References

- ADR 0089 (generic tool protocol) — the tool channel every review tool rides.
- ADR 0060 (external triggers & durable workflows) — the webhook/workflow
  skeleton, idempotency norms, and the drain-loop error-isolation lesson.
- ADR 0034 (idle eviction/resume) — why prodding a parked authoring session
  just works. ADR 0051 (orchestration tier) — why all of this lives in the
  orchestrator.
- Prior unmerged draft: `adr-0067-0068-pr-review-design` branch (2026-07-02),
  superseded by this ADR.
- Field research (2026-07-15): CodeRabbit docs + their published pipeline
  (context engineering → sandboxed agentic investigation → judge model;
  category taxonomy adopted here), Greptile docs + "How to Make LLMs Shut Up"
  + v3/v4 blogs (address rate, embedding feedback filter, agentic loop
  precision), Devin Review docs + SWE-check blog (confidence/severity split,
  precision as an engineered target), Cursor Bugbot docs (BUGBOT.md
  directory-scoped rules, budgets), kodus source (finder/verifier
  refute-to-drop, evidence gate, diff-boundary rule, deterministic dedup,
  snapshot-tested prompt builder).
