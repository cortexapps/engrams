# Runbook: the PR-review parallel window (ADR 0119 phase 4.4)

During the window, each enrolled repo reviews on one of two engines:

- **`legacy`** — the hand-written `PrReviewWorkflow` (ADR 0100). The default.
- **`automation`** — the seeded PR-review built-in automation (ADR 0119).

The two run on the same review record and the same `/reviews` dossier; only
the engine that drives a pass differs. Move one repo at a time, verify, widen.

## Flip a repo onto the built-in

Flip via the enrollment RPC (the `engine` field on `UpsertEnrollment`, or the
narrower path if you script it). Flipping to `automation`:

1. sets `review_enrollment.engine = 'automation'` for the repo, and
2. adds the repo to the built-in's `repos` input (mode from `trigger_mode`,
   autofix from `autofix`), and
3. enables the built-in if it was still disabled (it seeds disabled).

Flipping back to `legacy` removes the repo from the input; the built-in stays
enabled if it still owns other repos.

Start with **`engrams/engrams`**:

```
# via the engrams CLI / orchestrator-rpc.sh, ReviewService.UpsertEnrollment
{ "repo": "engrams/engrams", "trigger_mode": "auto", "autofix": "off",
  "engine": "automation" }
```

Confirm: `select repo, engine from review_enrollment;` shows `automation`, and
the PR-review automation on the Automations page is **enabled** with
`engrams/engrams` in its `repos` input.

## The two brakes

1. **Per-repo:** flip the repo's `engine` back to `legacy`. Immediate; the next
   delivery reviews on the legacy path.
2. **Fleet-wide kill switch:** set `ORCHESTRATOR_REVIEW_AUTOMATION_DISABLED=1`
   (or `true`) and restart the orchestrator pods. Every repo reviews on legacy
   regardless of its flag, without touching any row. The built-in's own
   `enabled=false` toggle is an independent third brake.

## What to watch on the first flagged repo

- The **run page** (`/settings/automations/<pr-review id>/runs`): each PR event
  should start a run that walks open-pass → finder → (verifier) → policy gate →
  post, and end `completed`.
- The **Reviews dossier** (`/reviews`): a pass appears with findings, same as
  legacy.
- On the PR itself: the **👀 acknowledging** comment, then the posted review,
  then **✅ posted N findings** — or, on a crash, **❌ review failed: …** (the
  4.3b finalize hook).
- A force-push (`synchronize`) supersedes the in-flight pass (one active pass
  per PR url), same as legacy.

## Parity checklist (walk before widening)

- [ ] 👀 acknowledgement comment posted, then edited to ✅/❌ in place.
- [ ] Findings appear in the dossier and inline on the PR (COMMENT review).
- [ ] A `@engrams review` comment from an OWNER/MEMBER/COLLABORATOR starts a
      pass; a bot or outside commenter does not.
- [ ] A draft PR does not auto-review; marking it ready does.
- [ ] Zero-candidate pass posts a clean "no findings" and skips the verifier.
- [ ] A force-push supersedes the running pass (no double review).
- [ ] The 422 inline-comment fallback still posts a summary-only review.
- [ ] Re-run from `/reviews` (Retry) starts a fresh built-in run.
- [ ] A crashed/timed-out pass posts **❌ review failed** and cleans up.

## Roll back

- One repo: flip `engine` → `legacy`.
- Everything: `ORCHESTRATOR_REVIEW_AUTOMATION_DISABLED=1` + restart, then
  disable the built-in on the Automations page. No data migration; the legacy
  workflow was never removed (that is phase 4.7, after this window passes in
  prod).
