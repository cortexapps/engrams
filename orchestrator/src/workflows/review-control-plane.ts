/** Durable review-record and finder-session operations behind the workflow seam. */

import { Code, ConnectError } from "@connectrpc/connect";

import { getDb } from "../db/client.ts";
import {
  defaultRunExecRuntime,
  runExec,
  RunExecError,
  type DurableExecClient,
  type RunExecResult,
  type RunExecRuntime,
} from "../exec/durable-exec.ts";
import { makeConnectorStore } from "../db/connectors.ts";
import { makeEnrollmentStore, type EnrollmentStore } from "../db/enrollments.ts";
import { makeProfileStore, type ProfileStore } from "../db/profiles.ts";
import {
  makeReviewStore,
  type ReviewDetail,
  type ReviewStore,
} from "../db/reviews.ts";
import {
  makeReviewSessionStore,
  type ReviewSessionStore,
} from "../db/review-sessions.ts";
import {
  task as taskTable,
  type ProfileNetwork,
} from "../db/schema.ts";
import { config } from "../config.ts";
import { log as rootLog } from "../log.ts";
import {
  harnessCatalog as defaultHarnessCatalog,
  images as defaultImages,
  sessions as defaultSessions,
} from "../control-plane/client.ts";
import {
  createSessionForExistingTask,
  registerSessionListener as registerExistingSessionListener,
  type CreateSessionForExistingTaskParams,
  type HarnessCatalogClient,
  type TaskSessionsClient,
} from "../rpc/task-create.ts";
import type { ImagesClient } from "../rpc/profiles.ts";
import {
  buildInlineCommentBody,
  buildReviewSummary,
  buildStatusComment,
  makeGithubReviewPoster,
  type GithubReviewPoster,
  type PrContext,
  type ReviewStatusPhase,
} from "../reviews/github-review.ts";
import {
  runPolicyGate,
  type FindingDecision,
  type PolicyDecision,
} from "../reviews/policy-gate.ts";
import {
  renderReviewer as defaultRenderReviewer,
  type RenderReviewerOptions,
  type RenderedReviewerFile,
  type ReviewCategory,
} from "../reviewers/render.ts";

const log = rootLog.child({ component: "review-control-plane" });

export interface EnsureReviewRecordInput {
  repo: string;
  prNumber: number;
  headSha: string;
  baseSha: string;
  trigger: string;
  /** ADR 0100 decision 9. Absent when head resolution failed before it could be
   *  captured — the record is still created, just unnamed. */
  pr?: PrContext;
}

export interface ReviewControlPlane {
  /** The name stays `resolvePrHeads` even though it now also returns the PR
   *  context: it appears as a `step(...)` name inside `prReviewWorkflowImpl`,
   *  and DBOS derives the application version from that function's source, so
   *  renaming it would rotate the version and strand in-flight reviews for no
   *  benefit. */
  resolvePrHeads(repo: string, prNumber: number): Promise<{
    headSha: string;
    baseSha: string;
    pr: PrContext;
  }>;
  ensureReviewRecord(
    input: EnsureReviewRecordInput,
  ): Promise<{ reviewId: string; taskId: string }>;
  createFinderSession(input: {
    reviewId: string;
    taskId: string;
    repo: string;
    prNumber: number;
    workflowId: string;
  }): Promise<{ sessionId: string }>;
  bootstrapFinderSession(sessionId: string, input: {
    reviewId: string;
    repo: string;
    headSha: string;
    enabledCategories?: readonly ReviewCategory[];
    orgInstructions?: string;
  }): Promise<void>;
  sendFinderPrompt(sessionId: string, input: {
    reviewId: string;
    repo: string;
    prNumber: number;
    headSha: string;
    baseSha: string;
    focus?: string;
  }): Promise<void>;
  /** Retire the finder worker (best-effort) and report how many candidate
   *  findings it left, as one durable step. */
  concludeFinderPhase(
    reviewId: string,
    opts?: { sessionId?: string },
  ): Promise<{ candidateCount: number }>;
  createVerifierSession(input: {
    reviewId: string;
    taskId: string;
    repo: string;
    prNumber: number;
    workflowId: string;
  }): Promise<{ sessionId: string }>;
  bootstrapVerifierSession(sessionId: string, input: {
    repo: string;
    headSha: string;
    reviewId: string;
    enabledCategories?: readonly ReviewCategory[];
    orgInstructions?: string;
  }): Promise<void>;
  sendVerifierPrompt(sessionId: string, input: {
    reviewId: string;
    repo: string;
    prNumber: number;
  }): Promise<void>;
  /** Post the review results. Retires the verifier worker first (best-effort)
   *  when a session id is given, so a stray worker never blocks the post. */
  postReviewResults(
    reviewId: string,
    opts?: { sessionId?: string },
  ): Promise<void>;
  /** Tear down the given worker (best-effort) and mark the review failed, as one
   *  durable step. `reason` is recorded on the activity log for the UI. */
  failReview(
    reviewId: string,
    opts?: { sessionId?: string; reason?: string },
  ): Promise<void>;
  /** Tear down the given worker (best-effort) and mark the review halted. */
  haltReview(reviewId: string, opts?: { sessionId?: string }): Promise<void>;
}

interface ReviewControlPlaneStore extends Pick<
  ReviewStore,
  | "createReview"
  | "getReview"
  | "getActiveReviewForPr"
  | "updateReviewStatus"
  | "updateFindingState"
  | "finalizeReview"
  | "setStatusCommentId"
  | "setReviewSessionId"
  | "recordEvent"
> {}

// The review control plane knows nothing about exec transport mechanics
// (severance, replay offsets, backoff) — that is `DurableExecClient` /
// `runExec`'s job, in ../exec/durable-exec.ts. Reviews only decide WHAT to
// run (clone, merge-base), the ticket name, and the deadline.
export interface ReviewSessionsClient extends TaskSessionsClient, DurableExecClient {
  writeFiles(req: {
    sessionId: string;
    files: Array<{ path: string; content: Uint8Array; mode: number }>;
  }): Promise<{ results: Array<{ path: string; ok: boolean; error?: string }> }>;
  sendPrompt(req: { sessionId: string; promptId: string; text: string }): Promise<unknown>;
}

type RenderReviewer = (opts: RenderReviewerOptions) => RenderedReviewerFile[];
type CreateExistingTaskSession = (
  params: CreateSessionForExistingTaskParams,
) => Promise<{ sessionId: string }>;

export interface ReviewControlPlaneDeps {
  reviews?: ReviewControlPlaneStore;
  db?: ReturnType<typeof getDb>;
  insertTask?: (input: { repo: string; prNumber: number }) => Promise<string>;
  sessions?: ReviewSessionsClient;
  profiles?: Pick<ProfileStore, "getActive" | "getByDesignation">;
  enrollments?: Pick<EnrollmentStore, "get">;
  reviewSessions?: ReviewSessionStore;
  githubPoster?: GithubReviewPoster;
  renderReviewer?: RenderReviewer;
  /** Focused test seam; production delegates to the shared task-create helper. */
  createSessionForExistingTask?: CreateExistingTaskSession;
  /** Focused seam for asserting binding-before-listener publication. */
  registerSessionListener?: (sessionId: string) => Promise<void>;
  /** Deterministic retry/deadline scheduler for durable-exec tests. */
  execRuntime?: RunExecRuntime;
}

/** Human detail for a `posted` activity-log entry. */
function postedSummary(count: number): string {
  if (count === 0) return "No findings";
  return `${count} finding${count === 1 ? "" : "s"} posted`;
}

/**
 * The v1 policy gate plus the anchor split: a confirmed finding with no inline
 * anchor is demoted to ui_only. Shared by the live post and the crash-recovery
 * marker path so both settle findings into the same terminal states.
 */
function buildDecision(detail: ReviewDetail): PolicyDecision {
  const policy = runPolicyGate(detail);
  const missingAnchors: FindingDecision[] = [];
  const anchored = policy.toPost.filter((item) => {
    const hasAnchor = item.finding.endLine != null || item.finding.startLine != null;
    if (!hasAnchor) missingAnchors.push({ ...item, state: "ui_only" });
    return hasAnchor;
  });
  return {
    ...policy,
    toPost: anchored,
    uiOnly: [...policy.uiOnly, ...missingAnchors],
  };
}

export class ReviewSetupError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "ReviewSetupError";
  }
}

const FINDER_SYSTEM_PROMPT = [
  "You are the finder for an automated pull-request review.",
  "Read /workspace/.review/finder.md and follow it. Never edit files, never push, and report findings only through the provided tools.",
].join("\n");

const VERIFIER_SYSTEM_PROMPT = [
  "You are the verifier for an automated pull-request review.",
  "Read /workspace/.review/verifier.md and follow it. Refute each finding; confirm only what you can reproduce from code you read, and report every judgment through submit_verdict.",
].join("\n");

// GitHub's own charset limits (owner + repo are [A-Za-z0-9._-]); enforced here
// so a repo/SHA can never carry shell metacharacters into the bootstrap `sh -c`
// — defense in depth on top of the signature check + enrollment gate.
const REPO_RE = /^[A-Za-z0-9._-]+\/[A-Za-z0-9._-]+$/;
const SHA_RE = /^[0-9a-fA-F]{7,40}$/;
const BOOTSTRAP_CLONE_DEADLINE_MS = 5 * 60_000;
const MERGE_BASE_DEADLINE_MS = 60_000;

// Security clamp: reviewer workers may read only the reviewed repo, while the
// orchestrator remains the sole GitHub writer. Direct clone/codeload hosts are
// explicit because the GitHub connector itself declares only api.github.com.
const REVIEW_CAPABILITIES = (repo: string): readonly string[] => [
  "engram:pr_review",
  `github:contents:read@${repo}`,
];
const REVIEW_NETWORK: ProfileNetwork = {
  default: "deny",
  allowHosts: ["github.com", "codeload.github.com", "api.github.com"],
  allowHostPatterns: [],
};

function assertSafeRepo(repo: string): void {
  if (!REPO_RE.test(repo)) throw new ReviewSetupError(`invalid repository: ${repo}`);
}

function repoName(repo: string): string {
  assertSafeRepo(repo);
  const name = repo.split("/").at(-1);
  if (!name) throw new ReviewSetupError(`invalid repository: ${repo}`);
  return name;
}

/** The one coarse clone step both phases share: `rm -rf` the target (so a
 *  retried step is safe) then clone (+ checkout when a head SHA is known). repo
 *  and headSha are validated before reaching the `sh -c` — injection defense in
 *  depth. Throws ReviewSetupError on a non-zero/absent exit, carrying stderr. */
async function cloneRepo(
  sessions: ReviewSessionsClient,
  sessionId: string,
  repo: string,
  headSha: string,
  phase: string,
  execRuntime: RunExecRuntime,
): Promise<void> {
  const name = repoName(repo);
  if (headSha !== "" && !SHA_RE.test(headSha)) {
    throw new ReviewSetupError(`invalid head SHA: ${headSha}`);
  }
  const workspace = `/workspace/${name}`;
  let command = `rm -rf ${workspace} && git clone https://github.com/${repo}.git ${workspace}`;
  if (headSha !== "") {
    command += ` && git -C ${workspace} checkout ${headSha}`;
  }

  let result: RunExecResult;
  try {
    result = await runExec(sessions, sessionId, command, {
      execId: `exec:${sessionId}:bootstrap-clone`,
      deadlineMs: BOOTSTRAP_CLONE_DEADLINE_MS,
    }, execRuntime);
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    const stderr = error instanceof RunExecError ? error.stderr.trim() : "";
    throw new ReviewSetupError(
      `${phase} clone failed: ${message}${stderr === "" ? "" : `: ${stderr}`}`,
    );
  }
  const { exitStatus, stderr } = result;
  if (exitStatus !== 0) {
    const detail = stderr.trim() || "exec stream ended without a successful exit status";
    throw new ReviewSetupError(`${phase} clone failed: ${detail}`);
  }
}

/** Resolve the TRUE merge base (fork point) of the base and head commits in
 *  the already-cloned checkout. GitHub's `pull.base.sha` is the base BRANCH's
 *  current head, not the fork point — anchoring a diff there renders every
 *  commit the base branch gained since the fork as phantom DELETIONS in the PR
 *  (live: engrams#820 was reported as deleting `HarnessDescriptor.egress`, a
 *  field main gained after the branch forked). Resolving here, in setup, hands
 *  the finder the real anchor so it cannot mis-scope even with a two-dot
 *  `git diff`. The full clone always contains the fork point, so this is a
 *  local computation — no network, no API. Throws rather than fall back to the
 *  wrong anchor. Both SHAs are validated before reaching `sh -c` (injection
 *  defense in depth, as in cloneRepo). */
async function resolveMergeBase(
  sessions: ReviewSessionsClient,
  sessionId: string,
  repoDir: string,
  baseSha: string,
  headSha: string,
  execRuntime: RunExecRuntime,
): Promise<string> {
  if (!SHA_RE.test(baseSha)) throw new ReviewSetupError(`invalid base SHA: ${baseSha}`);
  if (!SHA_RE.test(headSha)) throw new ReviewSetupError(`invalid head SHA: ${headSha}`);
  const { exitStatus, stdout, stderr } = await runExec(
    sessions,
    sessionId,
    `git -C ${repoDir} merge-base ${baseSha} ${headSha}`,
    {
      execId: `exec:${sessionId}:merge-base:${baseSha}:${headSha}`,
      deadlineMs: MERGE_BASE_DEADLINE_MS,
    },
    execRuntime,
  );
  const mergeBase = stdout.trim();
  if (exitStatus !== 0 || !SHA_RE.test(mergeBase)) {
    const detail = stderr.trim() || `merge-base returned ${JSON.stringify(mergeBase)}`;
    throw new ReviewSetupError(
      `failed to resolve merge base of ${baseSha}..${headSha}: ${detail}`,
    );
  }
  return mergeBase;
}

function productionImagesClient(): ImagesClient {
  return {
    async listEnabledImages(req) {
      const response = await defaultImages.listEnabledImages(req);
      return {
        images: response.images.map((image) => ({
          id: image.id,
          imageUri: image.imageUri,
        })),
      };
    },
  };
}

function productionHarnessCatalogClient(): HarnessCatalogClient {
  return {
    async listHarnesses(req) {
      const response = await defaultHarnessCatalog.listHarnesses(req);
      return {
        harnesses: response.harnesses.map((harness) => ({
          name: harness.name,
          ...(harness.descriptor ? { descriptor: harness.descriptor } : {}),
        })),
      };
    },
  };
}

/** Insert only the automation-owned task row. Review phase sessions attach
 * task_session rows later in the ADR 0100 execution PR. */
export async function insertReviewTask(
  db: ReturnType<typeof getDb>,
  input: { repo: string; prNumber: number },
): Promise<string> {
  const taskId = crypto.randomUUID();
  await db.insert(taskTable).values({
    id: taskId,
    type: "pr_review",
    title: `Review ${input.repo}#${input.prNumber}`,
    status: "working",
    createdByUserId: null,
    source: {
      provider: "github",
      repo: input.repo,
      prNumber: input.prNumber,
    },
  });
  return taskId;
}

export function makeReviewControlPlane(
  deps: ReviewControlPlaneDeps = {},
): ReviewControlPlane {
  let resolvedDb = deps.db;
  const db = () => (resolvedDb ??= getDb());
  let reviewStore = deps.reviews;
  const reviews = () => (reviewStore ??= makeReviewStore(db()));
  const insertTask = deps.insertTask ?? ((input) =>
    insertReviewTask(db(), input));
  const sessions = deps.sessions ?? defaultSessions;
  const execRuntime = deps.execRuntime ?? defaultRunExecRuntime;
  let profileStore = deps.profiles;
  const profiles = () => (profileStore ??= makeProfileStore(db()));
  let enrollmentStore = deps.enrollments;
  const enrollments = () => (enrollmentStore ??= makeEnrollmentStore(db()));
  let reviewSessionStore = deps.reviewSessions;
  const reviewSessions = () => (
    reviewSessionStore ??= makeReviewSessionStore(db())
  );
  const renderReviewer = deps.renderReviewer ?? defaultRenderReviewer;
  const githubPoster = deps.githubPoster ?? makeGithubReviewPoster();

  // The sticky GitHub status comment (👀 → ⏳ → ✅). Best-effort: an ack that
  // fails must never wedge the review, so every failure is logged and
  // swallowed. The comment id is persisted on first post so later phases edit
  // in place rather than stacking new comments.
  const reviewsPageUrl = `${config.baseUrl.replace(/\/$/, "")}/reviews`;
  const ackStatus = async (
    reviewId: string,
    phase: ReviewStatusPhase,
    count?: number,
  ): Promise<void> => {
    try {
      const detail = await reviews().getReview(reviewId);
      if (!detail) return;
      const { repo, prNumber, statusCommentId } = detail.review;
      const body = buildStatusComment({
        reviewId,
        phase,
        ...(count !== undefined ? { count } : {}),
        ...(phase === "posted" ? { reviewUrl: reviewsPageUrl } : {}),
      });
      const { commentId } = await githubPoster.upsertStatusComment({
        repo,
        prNumber,
        ...(statusCommentId ? { commentId: statusCommentId } : {}),
        body,
      });
      if (commentId !== statusCommentId) {
        await reviews().setStatusCommentId(reviewId, commentId);
      }
    } catch (err) {
      log.error({ reviewId, phase, err }, "review status ack failed (best-effort)");
    }
  };
  // Append one milestone to the review's activity log. Best-effort like
  // ackStatus: an activity-log write must never wedge a review, so a failure is
  // logged and swallowed. Recorded inside the existing control-plane steps, so
  // DBOS memoization keeps a replayed workflow from duplicating entries.
  const recordEvent = async (
    reviewId: string,
    kind: string,
    detail?: string,
  ): Promise<void> => {
    try {
      await reviews().recordEvent(reviewId, kind, detail);
    } catch (err) {
      log.error({ reviewId, kind, err }, "review event record failed (best-effort)");
    }
  };
  // Delete a worker's coordinator session and forget its binding. Tolerates an
  // already-absent session (a prior partial teardown) so it is safe to retry.
  const removeWorkerSession = async (sessionId: string): Promise<void> => {
    try {
      await sessions.deleteSession({ sessionId });
    } catch (err) {
      if (!(err instanceof ConnectError) || err.code !== Code.NotFound) {
        throw err;
      }
      log.info(
        { sessionId },
        "review worker session was already absent during cleanup",
      );
    }
    await reviewSessions().remove(sessionId);
  };
  // Best-effort cleanup for the terminal paths: a worker that will not tear down
  // must never block the review from settling into failed/halted.
  const cleanupWorkerSession = async (
    sessionId: string | undefined,
  ): Promise<void> => {
    if (sessionId === undefined) return;
    try {
      await removeWorkerSession(sessionId);
    } catch (err) {
      log.error({ sessionId, err }, "review worker cleanup failed");
    }
  };
  const createExistingSession = deps.createSessionForExistingTask ?? ((params) => {
    const database = db();
    return createSessionForExistingTask(
      {
        profiles: profiles(),
        images: productionImagesClient(),
        connectors: { list: () => makeConnectorStore(database).list() },
        harnessCatalog: productionHarnessCatalogClient(),
        sessions,
        secrets: { get: async () => null, getAll: async () => ({}) },
        db: database,
      },
      params,
    );
  });
  const registerSessionListener = deps.registerSessionListener
    ?? ((sessionId: string) => registerExistingSessionListener(db(), sessionId));

  return {
    async resolvePrHeads(repo, prNumber) {
      return githubPoster.fetchPrContext(repo, prNumber);
    },

    async ensureReviewRecord(input) {
      const active = await reviews().getActiveReviewForPr(
        input.repo,
        input.prNumber,
      );
      if (active) return { reviewId: active.id, taskId: active.taskId };

      const taskId = await insertTask({
        repo: input.repo,
        prNumber: input.prNumber,
      });
      // Flattened field by field rather than spread: `input` carries a nested
      // `pr` object, and this value goes straight into a drizzle insert where an
      // unknown key is a runtime error rather than a type error.
      const reviewId = await reviews().createReview({
        repo: input.repo,
        prNumber: input.prNumber,
        headSha: input.headSha,
        baseSha: input.baseSha,
        trigger: input.trigger,
        taskId,
        status: "queued",
        prTitle: input.pr?.title ?? null,
        prAuthor: input.pr?.author ?? null,
        headBranch: input.pr?.headBranch ?? null,
        baseBranch: input.pr?.baseBranch ?? null,
        prState: input.pr?.state ?? null,
        additions: input.pr?.additions ?? null,
        deletions: input.pr?.deletions ?? null,
        changedFiles: input.pr?.changedFiles ?? null,
      });
      await recordEvent(reviewId, "queued");
      await ackStatus(reviewId, "acknowledged");
      return { reviewId, taskId };
    },

    async createFinderSession(input) {
      assertSafeRepo(input.repo);
      const enrollment = await enrollments().get(input.repo);
      const profileId = enrollment?.profileId
        ?? (await profiles().getByDesignation("pr_reviewer"))?.id;
      if (!profileId) {
        throw new ReviewSetupError("no pr_reviewer profile configured");
      }

      const created = await createExistingSession({
        taskId: input.taskId,
        profileId,
        role: "finder",
        capabilityOverride: REVIEW_CAPABILITIES(input.repo),
        networkOverride: REVIEW_NETWORK,
        dropProfileSecretsAndEnv: true,
        appendSystemPrompt: FINDER_SYSTEM_PROMPT,
        source: {
          reviewId: input.reviewId,
          repo: input.repo,
          prNumber: input.prNumber,
        },
      });
      await reviewSessions().record(
        created.sessionId,
        input.workflowId,
        "finder",
      );
      // Stamp the session on the review at kickoff so the UI can offer a live
      // "watch" link the moment the finding phase starts.
      await reviews().setReviewSessionId(input.reviewId, "finder", created.sessionId);
      await recordEvent(input.reviewId, "finder_started");
      await registerSessionListener(created.sessionId);
      return created;
    },

    async bootstrapFinderSession(sessionId, input) {
      await recordEvent(input.reviewId, "cloning", "finder");
      await cloneRepo(
        sessions,
        sessionId,
        input.repo,
        input.headSha,
        "finder",
        execRuntime,
      );

      const encoder = new TextEncoder();
      const files = renderReviewer({
        role: "finder",
        ...(input.enabledCategories ? { enabledCategories: input.enabledCategories } : {}),
        ...(input.orgInstructions !== undefined
          ? { orgInstructions: input.orgInstructions }
          : {}),
      }).map((file) => ({
        path: file.path,
        content: encoder.encode(file.content),
        mode: 0o644,
      }));
      const response = await sessions.writeFiles({ sessionId, files });
      const failed = response.results.find((result) => !result.ok);
      if (failed) {
        throw new ReviewSetupError(
          `failed to stage reviewer instructions at ${failed.path}: ${failed.error ?? "unknown error"}`,
        );
      }
    },

    async sendFinderPrompt(sessionId, input) {
      const name = repoName(input.repo);
      // Resolve the TRUE merge base here and hand it to the finder as the diff
      // anchor. input.baseSha is GitHub's `pull.base.sha` — the base BRANCH's
      // head, not the fork point — so it is the WRONG anchor (see
      // resolveMergeBase for the engrams#820 phantom-deletion failure). By
      // computing `git merge-base` in setup we give the finder the real fork
      // point as base_sha, so its diff is correctly scoped even if it runs a
      // two-dot `git diff base head`. The prompt still uses three-dot as a
      // belt-and-suspenders (with a true merge base the two are equivalent).
      const mergeBase = input.baseSha !== "" && input.headSha !== ""
        ? await resolveMergeBase(
          sessions,
          sessionId,
          `/workspace/${name}`,
          input.baseSha,
          input.headSha,
          execRuntime,
        )
        : "";
      const range = mergeBase !== ""
        ? `${mergeBase}...${input.headSha}`
        : "the PR diff";
      const prompt = [
        `Review ${input.repo} pull request #${input.prNumber}.`,
        `Analyze ${range} in /workspace/${name}.`,
        ...(mergeBase !== "" ? [`base_sha is ${mergeBase} (the merge base).`] : []),
        "Read /workspace/.review/finder.md and follow its instructions before reviewing.",
        ...(input.focus?.trim() ? [`Focus directive: ${input.focus.trim()}`] : []),
        "Report findings only through the provided review tools; do not edit files or push changes.",
      ].join("\n");

      // The prompt id MUST be scoped to the session: the coordinator's
      // outbox is keyed globally by prompt_id (ON CONFLICT DO NOTHING), so
      // a retry finder session re-sending `review:<id>:finder` deduped
      // against the FAILED attempt's consumed row and silently never got
      // its prompt — the session idled forever and the review wedged in
      // `finding` (first observed live: review f33ad531).
      await sessions.sendPrompt({
        sessionId,
        promptId: `review:${input.reviewId}:finder:${sessionId}`,
        text: prompt,
      });
      await reviews().updateReviewStatus(input.reviewId, "finding");
      await recordEvent(input.reviewId, "reviewing");
      await ackStatus(input.reviewId, "finding");
    },

    async concludeFinderPhase(reviewId, opts = {}) {
      await cleanupWorkerSession(opts.sessionId);
      const detail = await reviews().getReview(reviewId);
      if (!detail) throw new Error(`review not found: ${reviewId}`);
      const candidateCount = detail.findings.filter(
        (finding) => finding.state === "candidate",
      ).length;
      return { candidateCount };
    },

    async createVerifierSession(input) {
      assertSafeRepo(input.repo);
      const enrollment = await enrollments().get(input.repo);
      const profileId = enrollment?.profileId
        ?? (await profiles().getByDesignation("pr_reviewer"))?.id;
      if (!profileId) {
        throw new ReviewSetupError("no pr_reviewer profile configured");
      }

      const created = await createExistingSession({
        taskId: input.taskId,
        profileId,
        role: "verifier",
        capabilityOverride: REVIEW_CAPABILITIES(input.repo),
        networkOverride: REVIEW_NETWORK,
        dropProfileSecretsAndEnv: true,
        appendSystemPrompt: VERIFIER_SYSTEM_PROMPT,
        source: {
          reviewId: input.reviewId,
          repo: input.repo,
          prNumber: input.prNumber,
        },
      });
      await reviewSessions().record(
        created.sessionId,
        input.workflowId,
        "verifier",
      );
      await reviews().setReviewSessionId(input.reviewId, "verifier", created.sessionId);
      await recordEvent(input.reviewId, "verifier_started");
      await registerSessionListener(created.sessionId);
      return created;
    },

    async bootstrapVerifierSession(sessionId, input) {
      await recordEvent(input.reviewId, "cloning", "verifier");
      await cloneRepo(
        sessions,
        sessionId,
        input.repo,
        input.headSha,
        "verifier",
        execRuntime,
      );

      const review = await reviews().getReview(input.reviewId);
      if (!review) {
        throw new ReviewSetupError(`review not found: ${input.reviewId}`);
      }
      const candidates = review.findings
        .filter((finding) => finding.state === "candidate")
        .map((finding) => ({
          id: finding.id,
          path: finding.path,
          start_line: finding.startLine,
          end_line: finding.endLine,
          side: finding.side,
          category: finding.category,
          severity: finding.severity,
          confidence: finding.confidence,
          title: finding.title,
          body_md: finding.bodyMd,
          evidence: finding.evidence,
        }));

      const encoder = new TextEncoder();
      const files = [
        ...renderReviewer({
          role: "verifier",
          ...(input.enabledCategories ? { enabledCategories: input.enabledCategories } : {}),
          ...(input.orgInstructions !== undefined
            ? { orgInstructions: input.orgInstructions }
            : {}),
        }),
        {
          path: "/workspace/.review/candidates.json",
          content: JSON.stringify(candidates),
        },
      ].map((file) => ({
        path: file.path,
        content: encoder.encode(file.content),
        mode: 0o644,
      }));
      const response = await sessions.writeFiles({ sessionId, files });
      const failed = response.results.find((result) => !result.ok);
      if (failed) {
        throw new ReviewSetupError(
          `failed to stage verifier inputs at ${failed.path}: ${failed.error ?? "unknown error"}`,
        );
      }
    },

    async sendVerifierPrompt(sessionId, input) {
      repoName(input.repo);
      const prompt = [
        `Judge the candidate findings for ${input.repo} pull request #${input.prNumber}.`,
        "Judge each candidate in /workspace/.review/candidates.json per /workspace/.review/verifier.md.",
        "Submit submit_verdict for every candidate; confirm only findings you can reproduce from code you read.",
      ].join("\n");

      // Session-scoped for the same reason as the finder prompt id: a
      // verifier retry must not dedupe against a dead attempt's row.
      await sessions.sendPrompt({
        sessionId,
        promptId: `review:${input.reviewId}:verifier:${sessionId}`,
        text: prompt,
      });
      await reviews().updateReviewStatus(input.reviewId, "verifying");
      const detail = await reviews().getReview(input.reviewId);
      const candidateCount = detail?.findings.filter(
        (finding) => finding.state === "candidate",
      ).length ?? 0;
      await recordEvent(
        input.reviewId,
        "verifying",
        `${candidateCount} candidate finding${candidateCount === 1 ? "" : "s"}`,
      );
      await ackStatus(input.reviewId, "verifying", candidateCount);
    },

    async postReviewResults(reviewId, opts = {}) {
      await cleanupWorkerSession(opts.sessionId);
      const detail = await reviews().getReview(reviewId);
      if (!detail) throw new Error(`review not found: ${reviewId}`);

      const { repo, prNumber } = detail.review;
      let { headSha, baseSha } = detail.review;
      if (headSha === "" || baseSha === "") {
        const live = await githubPoster.fetchPrContext(repo, prNumber);
        if (headSha === "") headSha = live.headSha;
        if (baseSha === "") baseSha = live.baseSha;
        await reviews().finalizeReview(reviewId, {
          status: detail.review.status,
          summaryMd: detail.review.summaryMd ?? "",
          headSha,
          baseSha,
        });
      }

      // Settle every finding into its decided terminal state. Idempotent, so it
      // is safe on both the live post and the crash-recovery marker path.
      const applyFindingStates = async (
        settled: PolicyDecision,
        inlinePosted: boolean,
      ): Promise<void> => {
        for (const item of settled.toPost) {
          await reviews().updateFindingState(
            item.finding.id,
            inlinePosted ? "posted" : "ui_only",
            item.verdictReason == null ? undefined : { verdictReason: item.verdictReason },
          );
        }
        for (const item of settled.uiOnly) {
          await reviews().updateFindingState(
            item.finding.id,
            "ui_only",
            item.verdictReason == null ? undefined : { verdictReason: item.verdictReason },
          );
        }
        for (const item of settled.suppressed) {
          await reviews().updateFindingState(
            item.finding.id,
            item.state,
            item.verdictReason == null ? undefined : { verdictReason: item.verdictReason },
          );
        }
      };

      // The marker check closes the crash window between GitHub accepting the
      // review and the local transaction recording it. On recovery the review is
      // already on GitHub, so we cannot know whether it posted inline or fell
      // back to a summary — assume inline (the common case) and re-run the
      // idempotent state updates the crashed transaction never committed, so
      // findings don't stay stuck at `candidate`.
      if (await githubPoster.alreadyPosted(repo, prNumber, reviewId)) {
        const decision = buildDecision(detail);
        await applyFindingStates(decision, true);
        await reviews().finalizeReview(reviewId, {
          status: "posted",
          summaryMd: detail.review.summaryMd ?? "",
        });
        const surfaced = decision.toPost.length + decision.uiOnly.length;
        await recordEvent(reviewId, "posted", postedSummary(surfaced));
        await ackStatus(reviewId, "posted", surfaced);
        return;
      }

      const decision = buildDecision(detail);
      const comments = decision.toPost.map((item) => {
        const line = item.finding.endLine ?? item.finding.startLine;
        if (line == null) {
          throw new Error(`finding ${item.finding.id} has no inline anchor`);
        }
        const startLine = item.finding.startLine;
        return {
          findingId: item.finding.id,
          path: item.finding.path,
          line,
          side: item.finding.side ?? "RIGHT",
          ...(startLine != null && startLine !== line ? { startLine } : {}),
          body: buildInlineCommentBody(item.finding),
        };
      });

      // The summary is rendered for the actual outcome: concise when the inline
      // comments land (they carry their own detail), fuller on the 422 fallback
      // (re-quotes every surviving finding so none is lost). The poster returns
      // the body it actually posted, which we persist below.
      const reviewUrl = `${config.baseUrl.replace(/\/$/, "")}/reviews`;
      const posted = await githubPoster.postReview({
        repo,
        prNumber,
        commitId: headSha,
        buildSummary: (inlinePosted) =>
          buildReviewSummary({ reviewId, reviewUrl, decision, inlinePosted }),
        comments,
      });
      if (!posted.posted) throw new Error("GitHub review was not posted");

      await applyFindingStates(decision, posted.inlinePosted);
      await reviews().finalizeReview(reviewId, {
        status: "posted",
        summaryMd: posted.summaryMd,
        ...(posted.githubReviewId !== undefined
          ? { githubReviewId: posted.githubReviewId }
          : {}),
      });
      const surfaced = decision.toPost.length + decision.uiOnly.length;
      await recordEvent(reviewId, "posted", postedSummary(surfaced));
      await ackStatus(reviewId, "posted", surfaced);
    },

    async failReview(reviewId, opts = {}) {
      await cleanupWorkerSession(opts.sessionId);
      await reviews().updateReviewStatus(reviewId, "failed");
      await recordEvent(reviewId, "failed", opts.reason);
      await ackStatus(reviewId, "failed");
    },

    async haltReview(reviewId, opts = {}) {
      await cleanupWorkerSession(opts.sessionId);
      await reviews().updateReviewStatus(reviewId, "halted");
      await recordEvent(reviewId, "halted");
      await ackStatus(reviewId, "halted");
    },
  };
}
