/** The code-review built-in automation, one variant for each git forge
 * (ADR 0119 D7; a workstream per pull request, ADR 0120).
 *
 * `reviewDefinition` holds the graph every variant shares. A `ReviewVariant`
 * fills what a forge decides: the events, the facts step, the checkout, the
 * worker's reach, and the connector actions. Each variant is its own
 * built-in row with its own key, switch and inputs.
 *
 * The structure is locked. The `tunable` lists name the fields an org may
 * override through block overrides, and the inputs schema holds the per-org
 * settings (which repositories, the mention handle, the lenses, the reviewer
 * profile, org instructions).
 *
 * Product logic that is not a generic primitive is a `review_*` catalog
 * block: opening the pass, staging the briefs and prior context, the policy
 * gate, and the close-out. The session, the checkout, the prompts, the
 * comments and the review post are generic blocks.
 *
 * The facts step derives every value later blocks template from
 * (`steps.facts.value.*`), and returns null to reject the event. A variant
 * may use a Code block for it: the admission rule reads a map input by a
 * payload value, which a structured condition cannot express.
 *
 * `settings.onFinalize` reports a terminal state through
 * `review_close_pass`: failed or deadline marks the pass failed with the
 * run's error; halted marks it halted; superseded only tears the workers
 * down, because the new pass marks its predecessor. The engine ends the
 * worker sessions. A run that fails before `open` has no review id, so the
 * hook fails on its own step and the run's status stands.
 */

import { REVIEW_CATEGORIES } from "../../reviewers/render.ts";
import { FINDER_SYSTEM_PROMPT, VERIFIER_SYSTEM_PROMPT } from "../../reviews/control-plane.ts";
import { PR_REVIEW_CAPABILITY, REVIEW_PHASE_SIGNALS } from "../../tools/review.ts";
import {
  REVIEW_FACTS,
  REVIEW_HEAD_SHA,
  REVIEW_PR_NUMBER,
  REVIEW_REPO,
  type ReviewVariant,
} from "./review-variant.ts";
import type { AutomationDefinition, BlockDef } from "../engine/definition.ts";
import { DEFAULT_CONNECTION_PLACEHOLDER, type BuiltinAutomation } from "../engine/builtins.ts";
import {
  REVIEW_CLOSE_PASS_TYPE,
  REVIEW_OPEN_PASS_TYPE,
  REVIEW_RECORD_POST_TYPE,
  REVIEW_SETTLE_TYPE,
  REVIEW_STAGE_TYPE,
} from "../engine/blocks/review.ts";

/** The event key the dispatch edge admits a run under; no forge delivery
 * carries it. Its payload is the variant's `dispatchPayload`. */
export const REVIEW_DISPATCH_EVENT_KEY = "review.dispatch";

const F = REVIEW_FACTS;
const REPO = REVIEW_REPO;
const PR_NUMBER = REVIEW_PR_NUMBER;
const PHASE_DEADLINE_S = 7200;
const CLONE_DEADLINE_MS = 5 * 60_000;

function workerSession(
  v: ReviewVariant,
  id: string,
  role: "finder" | "verifier",
  systemPrompt: string,
): BlockDef {
  return {
    id,
    type: "create_session",
    // Model/effort ride the profile (the tunable profile reference).
    tunable: ["profileId", "networkOverride", "capabilityOverride"],
    config: {
      profileId: "${{ inputs.profile }}",
      // Empty initial prompt: the phase prompt arrives through send_prompt
      // after the workspace is staged, so nothing runs early.
      promptTemplate: "",
      titleTemplate: `Review ${REPO}#${PR_NUMBER} (${role})`,
      role,
      keepOnFinish: false,
      capabilityOverride: [PR_REVIEW_CAPABILITY, ...v.capabilities],
      networkOverride: v.network,
      dropProfileSecretsAndEnv: true,
      appendSystemPrompt: systemPrompt,
    },
  };
}

function checkout(v: ReviewVariant, id: string, sessionBlock: string): BlockDef {
  return {
    id,
    type: "run_command",
    tunable: ["deadlineMs"],
    config: {
      session: { blockId: sessionBlock },
      commandTemplate: v.checkoutCommand,
      deadlineMs: CLONE_DEADLINE_MS,
    },
  };
}

function stage(id: string, phase: "finder" | "verifier", sessionBlock: string): BlockDef {
  return {
    id,
    type: REVIEW_STAGE_TYPE,
    tunable: ["focus"],
    config: {
      phase,
      reviewId: "${{ steps.open.review_id }}",
      sessionId: `\${{ steps.${sessionBlock}.session_id }}`,
      repo: REPO,
      prNumber: PR_NUMBER,
      headSha: REVIEW_HEAD_SHA,
      baseSha: "${{ steps.open.base_sha }}",
      enabledCategories: { $ref: `${F}.categories` },
      orgInstructions: `\${{ ${F}.instructions }}`,
      focus: "",
    },
  };
}

function prompt(
  id: string,
  phase: "finder" | "verifier",
  sessionBlock: string,
  stageBlock: string,
): BlockDef {
  return {
    id,
    type: "send_prompt",
    tunable: ["deadlineSeconds"],
    config: {
      session: { blockId: sessionBlock },
      promptTemplate: `\${{ steps.${stageBlock}.prompt }}`,
      waitFor: { kind: "signal", name: REVIEW_PHASE_SIGNALS[phase] },
      deadlineSeconds: PHASE_DEADLINE_S,
    },
  };
}

/** The head, as a human reads a SHA. */
const HEAD_SHORT = "${{ steps.open.head_sha | truncate: 7, '' }}";
/** The status line while a pass runs. */
const REVIEWING_STATUS = `👀 engrams is reviewing ${HEAD_SHORT}.`;
/** The pinned status comment's key: `upsert_issue_comment` finds the
 * comment by it on every pass, and every edit goes through
 * `update_pinned_comment` with the same key so the marker survives the
 * rewrite (an edit through the plain update action would drop it, and the
 * next pass would post a second comment). */
const STATUS_KEY = "review-status";

function reviewBlocks(v: ReviewVariant): BlockDef[] {
  return [
    { id: "facts", ...v.facts },
    {
      id: "admit",
      type: "filter",
      tunable: [],
      config: {
        conditions: {
          mode: "all",
          conditions: [{ path: `${F}.admit`, op: "is_true" }],
        },
      },
    },
    {
      id: "open",
      type: REVIEW_OPEN_PASS_TYPE,
      tunable: [],
      config: {
        provider: v.provider,
        repo: REPO,
        prNumber: PR_NUMBER,
        trigger: `\${{ ${F}.trigger }}`,
        headSha: `\${{ ${F}.head_sha }}`,
        baseSha: `\${{ ${F}.base_sha }}`,
        pr: { $ref: `${F}.pr_context` },
      },
    },
    // ONE status comment per pull request, edited across passes: the pinned
    // comment is found by its key on every later pass (the connector's
    // marker scan), so a PR carries a single engrams status line that always
    // names the latest head, not one new comment per push.
    {
      id: "ack",
      type: "integration_action",
      tunable: ["params"],
      config: {
        provider: v.provider,
        actionId: v.actions.upsertStatus,
        params: {
          repo: REPO,
          number: PR_NUMBER,
          key: STATUS_KEY,
          body: REVIEWING_STATUS,
        },
      },
    },
    // A found comment still says what the previous pass said: rewrite it.
    {
      id: "ack_refresh",
      type: "integration_action",
      tunable: ["params"],
      config: {
        provider: v.provider,
        actionId: v.actions.updateStatus,
        params: {
          repo: REPO,
          commentId: { $ref: "steps.ack.commentId" },
          key: STATUS_KEY,
          body: REVIEWING_STATUS,
        },
      },
    },
    workerSession(v, "finder", "finder", FINDER_SYSTEM_PROMPT),
    checkout(v, "clone_finder", "finder"),
    stage("stage_finder", "finder", "finder"),
    prompt("find", "finder", "finder", "stage_finder"),
    {
      id: "has_candidates",
      type: "branch",
      tunable: [],
      config: {
        conditions: {
          mode: "all",
          conditions: [{ path: "steps.find.signal.candidate_count", op: "gt", value: 0 }],
        },
      },
      then: [
        workerSession(v, "verifier", "verifier", VERIFIER_SYSTEM_PROMPT),
        checkout(v, "clone_verifier", "verifier"),
        stage("stage_verifier", "verifier", "verifier"),
        prompt("verify", "verifier", "verifier", "stage_verifier"),
        {
          id: "end_verifier",
          type: "end_session",
          tunable: [],
          config: { session: { blockId: "verifier" } },
        },
      ],
      else: [],
    },
    {
      id: "end_finder",
      type: "end_session",
      tunable: [],
      config: { session: { blockId: "finder" } },
    },
    {
      id: "gate",
      type: REVIEW_SETTLE_TYPE,
      tunable: [],
      config: { reviewId: "${{ steps.open.review_id }}" },
    },
    // A review object on the PR only when there is something to review: a
    // pass with findings posts the forge review (inline comments + summary)
    // and points the status line at it; a clean pass says so on the status
    // line alone — an empty "No findings" review under every push was noise.
    {
      id: "has_findings",
      type: "branch",
      tunable: [],
      config: {
        conditions: {
          mode: "any",
          conditions: [
            { path: "steps.gate.to_post_count", op: "gt", value: 0 },
            { path: "steps.gate.ui_only_count", op: "gt", value: 0 },
          ],
        },
      },
      then: [
        {
          id: "post",
          type: "integration_action",
          tunable: [],
          config: {
            provider: v.provider,
            actionId: v.actions.postReview,
            params: {
              repo: "${{ steps.gate.repo }}",
              prNumber: { $ref: "steps.gate.pr_number" },
              commitId: "${{ steps.gate.commit_id }}",
              summary: "${{ steps.gate.summary_md }}",
              // GitHub refuses the whole batch when one anchor is outside the
              // diff (422); the action then posts this body instead, which
              // re-quotes every finding so the PR still shows them.
              fallbackSummary: "${{ steps.gate.fallback_summary_md }}",
              comments: { $ref: "steps.gate.comments" },
            },
          },
        },
        {
          // The posted review's id onto the pass, so the dossier links to it.
          // The settle step marked the pass posted before the post ran, so
          // this is the one write left after the action.
          id: "record_post",
          type: REVIEW_RECORD_POST_TYPE,
          tunable: [],
          config: {
            reviewId: { $ref: "steps.open.review_id" },
            // Missing on a replayed post (marker already there): `default`
            // makes the absent output render empty instead of failing.
            githubReviewId: "${{ steps.post.github_review_id | default: '' }}",
            // "false" when the action fell back to the summary-only review:
            // the findings settle as ui_only and the pass summary follows.
            inlinePosted: "${{ steps.post.inline_posted | default: true }}",
          },
        },
        {
          id: "status",
          type: "integration_action",
          tunable: ["params"],
          config: {
            provider: v.provider,
            actionId: v.actions.updateStatus,
            params: {
              repo: REPO,
              commentId: { $ref: "steps.ack.commentId" },
              key: STATUS_KEY,
              body: `✅ engrams reviewed ${HEAD_SHORT} — see the review below.`,
            },
          },
        },
      ],
      else: [
        {
          id: "status_clean",
          type: "integration_action",
          tunable: ["params"],
          config: {
            provider: v.provider,
            actionId: v.actions.updateStatus,
            params: {
              repo: REPO,
              commentId: { $ref: "steps.ack.commentId" },
              key: STATUS_KEY,
              body: `✅ engrams reviewed ${HEAD_SHORT}: no findings.`,
            },
          },
        },
      ],
    },
  ];
}

export function reviewDefinition(v: ReviewVariant): AutomationDefinition {
  return {
    engine: 1,
    trigger: {
      kind: "integration",
      provider: v.provider,
      connectionId: DEFAULT_CONNECTION_PLACEHOLDER,
      eventKeys: v.reviewEvents,
      scope: { fromInput: "repos" },
    },
    blocks: reviewBlocks(v),
    // ADR 0120: a pull request is a WORKSTREAM. The main entrypoint's runs
    // open it (or join it) by the key below; `pull_request.closed` ends it, so
    // later events for that PR drop, audited, instead of opening review runs.
    entrypoints: [
      {
        id: "closed",
        trigger: {
          kind: "integration",
          provider: v.provider,
          connectionId: DEFAULT_CONNECTION_PLACEHOLDER,
          eventKeys: v.closedEvents,
          scope: { fromInput: "repos" },
        },
        blocks: [
          {
            id: "close",
            type: "instance_close",
            tunable: [],
            config: { reason: "pull request closed" },
          },
        ],
      },
    ],
    inputsSchema: [
      {
        key: "repos",
        label: "Repositories",
        type: "map",
        keyNoun: "repository",
        help: "Which repositories engrams reviews, and whether each gets automatic reviews or only on request (a review comment).",
        default: {},
        valueShape: {
          mode: { type: "enum", values: ["auto", "on_request"], default: "on_request" },
          autofix: { type: "boolean", default: false },
        },
      },
      {
        key: "profile",
        label: "Reviewer profile",
        type: "string",
        help: "The id of the session profile the finder and verifier workers run under. Empty = reviews are off.",
        default: "",
      },
      {
        key: "mention",
        label: "Mention handle",
        type: "string",
        help: 'The handle a comment must mention to request a review, e.g. "@engrams-agent review". Defaults to the review App\'s login.',
        default: "@engrams",
      },
      {
        key: "categories",
        label: "Review lenses",
        type: "list",
        help: "Which lenses the reviewer applies.",
        default: [...REVIEW_CATEGORIES],
        values: [...REVIEW_CATEGORIES],
      },
      {
        key: "instructions",
        label: "Org instructions",
        type: "string",
        multiline: true,
        help: "Free-text guidance rendered into the reviewer brief.",
        default: "",
      },
    ],
    settings: {
      instance: {
        // One workstream per pull request.
        keyTemplate: v.instanceKeyTemplate,
        // A close only ends a workstream that exists: a PR engrams never
        // reviewed closing is a drop (audited), not an open-and-close.
        entrypoints: { closed: { admit: "require" } },
      },
      concurrency: {
        // Rendered at admission, before any block runs, so it reads the raw
        // event. It is instance-scoped: the supersede is per pull request,
        // and a close run supersedes a review still running on it.
        keyTemplate: v.concurrencyKeyTemplate,
        policy: "supersede",
      },
      runDeadlineSeconds: 4 * PHASE_DEADLINE_S,
      endSessionsOnFinish: true,
      onFinalize: [
        {
          when: ["failed", "deadline"],
          block: {
            id: "report_failure",
            type: REVIEW_CLOSE_PASS_TYPE,
            config: {
              reviewId: { $ref: "steps.open.review_id" },
              outcome: "failed",
              reason: "${{ run.error }}",
            },
          },
        },
        {
          when: ["halted"],
          block: {
            id: "report_halt",
            type: REVIEW_CLOSE_PASS_TYPE,
            config: { reviewId: { $ref: "steps.open.review_id" }, outcome: "halted" },
          },
        },
        {
          when: ["superseded"],
          block: {
            id: "cleanup_superseded",
            type: REVIEW_CLOSE_PASS_TYPE,
            config: { reviewId: { $ref: "steps.open.review_id" }, outcome: "superseded" },
          },
        },
      ],
    },
  };
}

export function reviewBuiltin(v: ReviewVariant): BuiltinAutomation {
  return {
    key: v.key,
    name: v.name,
    description: v.description,
    definitionVersion: v.definitionVersion,
    definition: reviewDefinition(v),
    async defaultInputs() {
      return {
        repos: {},
        profile: "",
        mention: v.defaultMention(),
        categories: [...REVIEW_CATEGORIES],
        instructions: "",
      };
    },
  };
}
