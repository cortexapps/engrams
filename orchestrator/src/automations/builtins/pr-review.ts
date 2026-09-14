/** The PR-review built-in automation (ADR 0119 D7, phase 4.3).
 *
 * The review pipeline ADR 0100 hand-wrote as a DBOS graph, expressed as
 * data on the automation engine. Structure is locked (built-ins are
 * structure-locked, property-editable); the `tunable` lists name the fields
 * an org may override through block overrides, and the inputs schema holds
 * the coarse per-org knobs (which repos, the mention handle, the lenses, the
 * reviewer profile, org instructions).
 *
 * Where product logic is not a generic primitive it stays in code as a
 * `system.*` block: opening the pass (targets, dedupe, the review row),
 * staging the reviewer briefs + prior context + candidates and composing the
 * phase prompts, the policy gate, and the supersede cleanup. Everything else
 * — the session, the clone, the prompt delivery, the GitHub comments and
 * review post, the teardown — is a visible generic block.
 *
 * The first block is a Code block rather than a structured filter: the
 * admission rule reads the repo's entry in a MAP input keyed by a dynamic
 * payload value (`inputs.repos[event.repository.full_name]`), which
 * structured condition paths cannot express, and the same block derives the
 * review facts every later block templates from (`steps.facts.value.*`) —
 * the hardened Liquid sandbox has no conditional tags, so deriving
 * "command vs opened" or "repo short name" belongs in code, once.
 *
 * Terminal parity with the legacy graph rides `settings.onFinalize`
 * (ENGINE_STEP_CONTRACT 2) through system.review_finalize:
 *   - failed | deadline → the pass is marked failed, the activity log gets
 *     the run's error as the reason, and the sticky status comment flips to
 *     the legacy "failed" text (failReview);
 *   - halted → the same via haltReview;
 *   - superseded → worker teardown only (cleanupSupersededReview); the review
 *     row's `superseded` status is written by the NEW pass's open_review_pass
 *     (beginReviewPass marks the predecessor), which also posts its own ack.
 * Worker sessions are ended by the engine's finalize (endSessionsOnFinish),
 * so the hooks pass no sessionId. A run that fails BEFORE open_review_pass
 * has no review id: the hook's `$ref` cannot resolve, it fails on its own
 * step row, and the run's status stands — there is no pass to report on.
 */

import { config } from "../../config.ts";
import { normalizeMentionHandle } from "../../integrations/github-webhook.ts";
import { PR_REVIEWER_DESIGNATION } from "../../reviewers/seed-profile.ts";
import { REVIEW_CATEGORIES } from "../../reviewers/render.ts";
import {
  FINDER_SYSTEM_PROMPT,
  REVIEW_NETWORK,
  VERIFIER_SYSTEM_PROMPT,
} from "../../reviews/control-plane.ts";
import { REVIEW_PHASE_SIGNALS } from "../../tools/review.ts";
import type { AutomationDefinition, BlockDef } from "../engine/definition.ts";
import type { BuiltinAutomation } from "../engine/builtins.ts";
import {
  OPEN_REVIEW_PASS_TYPE,
  REVIEW_POLICY_GATE_TYPE,
  REVIEW_STAGE_TYPE,
} from "../engine/blocks/system/review.ts";

export const PR_REVIEW_BUILTIN_KEY = "pr_review";

/** Bump on any graph or inputs-schema change (the seeder inserts a new
 * version when the stored content hash differs). */
export const PR_REVIEW_DEFINITION_VERSION = 3;

/** Synthetic event key the CI dispatch edge admits a run under (no GitHub
 * delivery carries it). The admission arm accepts it for any mapped repo. */
export const REVIEW_DISPATCH_EVENT_KEY = "review.dispatch";

/** Placeholder the seeder replaces with the org's default GitHub connection. */
export const DEFAULT_CONNECTION_PLACEHOLDER = "__default__";

const F = "steps.facts.value";
const REPO = `\${{ ${F}.repo }}`;
const PR_NUMBER = `\${{ ${F}.pr_number }}`;
const PHASE_DEADLINE_S = 7200;
const CLONE_DEADLINE_MS = 5 * 60_000;

/** Admission + fact derivation. Returns null to reject (the filter block
 * after it ends the run as `filtered`), else the facts object. Pure, no I/O,
 * runs in the QuickJS cage. */
export const REVIEW_FACTS_SOURCE = `
export default ({ event, inputs, trigger }) => {
  const raw = event.raw ?? {};
  const fullName = raw.repository?.full_name ?? "";
  const repoKey = fullName.toLowerCase();
  const entry = Object.entries(inputs.repos ?? {}).find(([k]) => k.toLowerCase() === repoKey)?.[1];
  if (!entry) return null;

  const key = trigger.event ?? "";
  let mode = null;
  if (key.startsWith("pull_request.")) {
    if (raw.pull_request?.draft === true) return null;
    if (entry.mode !== "auto") return null;
    mode = key.slice("pull_request.".length);
  } else if (key === "issue_comment.created") {
    if (!raw.issue?.pull_request) return null;
    if (raw.sender?.type === "Bot") return null;
    if (!["OWNER", "MEMBER", "COLLABORATOR"].includes(raw.comment?.author_association)) return null;
    const handle = String(inputs.mention ?? "@engrams").replace(/[.*+?^$()|[\\]\\\\{}]/g, "\\\\$&");
    const re = new RegExp("(^|\\\\s)" + handle + "(\\\\[bot\\\\])?\\\\s+review(\\\\s|$)", "im");
    if (!re.test(raw.comment?.body ?? "")) return null;
    mode = "command";
  } else if (key === "review.dispatch") {
    // An explicit dispatch (CI, an operator) is a request, like a comment
    // command: it does not need mode "auto", and it carries no SHAs, so
    // open_review_pass resolves the heads from GitHub.
    mode = "dispatch";
  } else {
    return null;
  }

  const pr = raw.pull_request ?? {};
  const number = pr.number ?? raw.issue?.number ?? 0;
  const str = (v) => (v === undefined || v === null ? "" : String(v));
  return {
    admit: true,
    repo: fullName,
    repo_name: fullName.split("/")[1] ?? "",
    pr_number: number,
    pr_url: str(pr.html_url || raw.issue?.html_url),
    trigger: mode,
    head_sha: str(pr.head?.sha),
    base_sha: str(pr.base?.sha),
    pr_context: {
      providerId: str(pr.id),
      title: str(pr.title),
      author: str(pr.user?.login),
      state: str(pr.state),
      url: str(pr.html_url),
      providerUpdatedAt: str(pr.updated_at),
      headBranch: str(pr.head?.ref),
      baseBranch: str(pr.base?.ref),
      additions: typeof pr.additions === "number" ? pr.additions : null,
      deletions: typeof pr.deletions === "number" ? pr.deletions : null,
      changedFiles: typeof pr.changed_files === "number" ? pr.changed_files : null,
    },
    categories: Array.isArray(inputs.categories) ? inputs.categories : [],
    instructions: str(inputs.instructions),
  };
};
`.trim();

/** The same deny-default clamp as the legacy workers; tunable so an org can
 * widen the allowlist (a private package index, say) without forking. */
const WORKER_NETWORK = {
  default: REVIEW_NETWORK.default,
  allowHosts: [...REVIEW_NETWORK.allowHosts],
  allowHostPatterns: [...REVIEW_NETWORK.allowHostPatterns],
};
const WORKER_CAPABILITIES = ["engram:pr_review", `github:contents:read@${REPO}`];

function workerSession(id: string, role: "finder" | "verifier", systemPrompt: string): BlockDef {
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
      capabilityOverride: WORKER_CAPABILITIES,
      networkOverride: WORKER_NETWORK,
      dropProfileSecretsAndEnv: true,
      appendSystemPrompt: systemPrompt,
    },
  };
}

function clone(id: string, sessionBlock: string): BlockDef {
  return {
    id,
    type: "run_command",
    tunable: ["deadlineMs"],
    config: {
      session: { blockId: sessionBlock },
      // head_sha is always present for PR events; a comment command on a PR
      // whose head open_review_pass resolved lands in steps.open.head_sha.
      // The checkout falls back to the PR's immutable ref: a merged PR's
      // branch is deleted, but refs/pull/<n>/head survives — the review must
      // not fail because its PR merged mid-flight.
      commandTemplate:
        `rm -rf /workspace/\${{ ${F}.repo_name }} && ` +
        `git clone https://github.com/${REPO}.git /workspace/\${{ ${F}.repo_name }} && ` +
        `(git -C /workspace/\${{ ${F}.repo_name }} checkout \${{ steps.open.head_sha }} || ` +
        `(git -C /workspace/\${{ ${F}.repo_name }} fetch origin +refs/pull/${PR_NUMBER}/head && ` +
        `git -C /workspace/\${{ ${F}.repo_name }} checkout \${{ steps.open.head_sha }}))`,
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
      headSha: "${{ steps.open.head_sha }}",
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

const blocks: BlockDef[] = [
  {
    id: "facts",
    type: "code",
    tunable: [],
    config: { source: REVIEW_FACTS_SOURCE, mode: "value" },
  },
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
    type: OPEN_REVIEW_PASS_TYPE,
    tunable: [],
    config: {
      provider: "github",
      repo: REPO,
      prNumber: PR_NUMBER,
      trigger: `\${{ ${F}.trigger }}`,
      headSha: `\${{ ${F}.head_sha }}`,
      baseSha: `\${{ ${F}.base_sha }}`,
      pr: { $ref: `${F}.pr_context` },
    },
  },
  {
    id: "ack",
    type: "integration_action",
    tunable: ["params"],
    config: {
      provider: "github",
      actionId: "create_issue_comment",
      params: {
        repo: REPO,
        number: PR_NUMBER,
        body: "👀 engrams is reviewing ${{ steps.open.head_sha | truncate: 7, '' }}.",
      },
    },
  },
  workerSession("finder", "finder", FINDER_SYSTEM_PROMPT),
  clone("clone_finder", "finder"),
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
      workerSession("verifier", "verifier", VERIFIER_SYSTEM_PROMPT),
      clone("clone_verifier", "verifier"),
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
    type: REVIEW_POLICY_GATE_TYPE,
    tunable: [],
    config: { reviewId: "${{ steps.open.review_id }}" },
  },
  {
    id: "post",
    type: "integration_action",
    tunable: [],
    config: {
      provider: "github",
      actionId: "post_pr_review",
      params: {
        repo: "${{ steps.gate.repo }}",
        prNumber: { $ref: "steps.gate.pr_number" },
        commitId: "${{ steps.gate.commit_id }}",
        summary: "${{ steps.gate.summary_md }}",
        comments: { $ref: "steps.gate.comments" },
      },
    },
  },
  {
    id: "status",
    type: "integration_action",
    tunable: ["params"],
    config: {
      provider: "github",
      actionId: "update_issue_comment",
      params: {
        repo: REPO,
        commentId: { $ref: "steps.ack.commentId" },
        body:
          "✅ engrams posted ${{ steps.gate.to_post_count }} finding(s) for ${{ steps.open.head_sha | truncate: 7, '' }}.",
      },
    },
  },
];

export const PR_REVIEW_DEFINITION: AutomationDefinition = {
  engine: 1,
  trigger: {
    kind: "integration",
    provider: "github",
    connectionId: DEFAULT_CONNECTION_PLACEHOLDER,
    eventKeys: [
      "pull_request.opened",
      "pull_request.synchronize",
      "pull_request.ready_for_review",
      "issue_comment.created",
    ],
    scope: { fromInput: "repos" },
  },
  blocks,
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
      help: "The session profile the finder and verifier workers run under.",
      default: PR_REVIEWER_DESIGNATION,
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
    concurrency: {
      // Rendered at ADMISSION, before any block runs, so it reads the raw
      // event (not steps.*): the PR url for PR events, the issue url for a
      // review-command comment — both are the pull request's html_url.
      keyTemplate:
        "${{ event.raw.pull_request.html_url | default: event.raw.issue.html_url }}",
      policy: "supersede",
    },
    runDeadlineSeconds: 4 * PHASE_DEADLINE_S,
    endSessionsOnFinish: true,
    onFinalize: [
      {
        when: ["failed", "deadline"],
        block: {
          id: "report_failure",
          type: "system.review_finalize",
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
          type: "system.review_finalize",
          config: { reviewId: { $ref: "steps.open.review_id" }, outcome: "halted" },
        },
      },
      {
        when: ["superseded"],
        block: {
          id: "cleanup_superseded",
          type: "system.review_finalize",
          config: { reviewId: { $ref: "steps.open.review_id" }, outcome: "superseded" },
        },
      },
    ],
  },
};

/** The seeded `mention` default: the review App's own login
 * (`GITHUB_APP_LOGIN`, the same handle the legacy route matches), so a
 * fresh deployment answers `@<app> review` out of the box. A deployment
 * without an App login gets the historical placeholder. The org can still
 * edit the input afterwards; the seeder never overwrites an org value. */
export function defaultMentionHandle(appLogin: string = config.githubAppLogin): string {
  const slug = normalizeMentionHandle(appLogin);
  return slug ? `@${slug}` : "@engrams";
}

export const PR_REVIEW_BUILTIN: BuiltinAutomation = {
  key: PR_REVIEW_BUILTIN_KEY,
  name: "PR review",
  description:
    "Reviews pull requests with a finder/verifier pair and posts the confirmed findings as a GitHub review.",
  definitionVersion: PR_REVIEW_DEFINITION_VERSION,
  definition: PR_REVIEW_DEFINITION,
  async defaultInputs() {
    return {
      repos: {},
      profile: PR_REVIEWER_DESIGNATION,
      mention: defaultMentionHandle(),
      categories: [...REVIEW_CATEGORIES],
      instructions: "",
    };
  },
};
