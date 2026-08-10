/** Durable review-record and finder-session operations behind the workflow seam. */

import { Code, ConnectError } from "@connectrpc/connect";
import { createHash } from "node:crypto";

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
  type BeginReviewPassInput,
  type BeginReviewPassResult,
  type PriorReviewPass,
  type ReviewDetail,
  type ReviewStore,
  type UpdateReviewPassContextInput,
} from "../db/reviews.ts";
import {
  makeReviewSessionStore,
  type ReviewSessionStore,
} from "../db/review-sessions.ts";
import { type ProfileNetwork } from "../db/schema.ts";
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
  type ReviewStatusPhase,
} from "../reviews/github-review.ts";
import type { PrContext } from "../reviews/pr-context.ts";
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

export interface ResolveReviewTargetInput {
  provider: string;
  providerId: string;
  repo: string;
  number: number;
  title: string | null;
  author: string | null;
  state: string | null;
  url: string | null;
  providerUpdatedAt: Date | null;
}

export interface StartReviewPassInput {
  reviewId: string;
  taskId: string;
  repo: string;
  prNumber: number;
  trigger: string;
  idempotencyKey: string;
  headSha: string;
  baseSha: string;
  focus?: string;
}

/** Who asked for a review. Carried whole so the ingress workflow body never has
 *  to assemble a log payload of its own — see `abandonIngress`. */
export interface ReviewIngressSource {
  provider: string;
  repo: string;
  prNumber: number;
  trigger: string;
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
  /** Find or create the row for the change under review, and return its id.
   *  Refreshes the title/state, so a rename lands everywhere at once. */
  resolveReviewTarget(input: ResolveReviewTargetInput): Promise<{ targetId: string }>;
  /** Atomically decide whether this request deduplicates or creates a pass. The
   *  transaction and nothing else — see the implementation's warning. */
  createReviewPass(input: BeginReviewPassInput): Promise<BeginReviewPassResult>;
  /** Post the 👀 status comment for a freshly created pass. Split out of
   *  `createReviewPass` so a slow or failing GitHub ack can never re-run that
   *  method's non-idempotent transaction. */
  acknowledgeReviewPass(reviewId: string): Promise<void>;
  /** Fill an early retry row once GitHub resolves its current pass facts. */
  updateReviewPassContext(
    reviewId: string,
    input: UpdateReviewPassContextInput,
  ): Promise<boolean>;
  /** Hand a fully resolved request to the review workflow. */
  startReviewPass(input: StartReviewPassInput): Promise<void>;
  /** Tell a committed predecessor to tear down without changing its status. */
  signalSupersededPass(reviewId: string, idempotencyKey: string): Promise<void>;
  /**
   * The single give-up path for ingress: a request that will never become a
   * review.
   *
   * `reviewId` is present only when ingress had already created a pass row (the
   * retry entry point does). Then this fails that row, so the user who pressed
   * retry sees a failed review instead of a spinner. With no row there is
   * nothing to fail, so it only reports.
   *
   * Both arms — and their logging — live here rather than in the workflow so
   * the ingress body stays free of branches and log calls. DBOS hashes that
   * body to derive the application version, so editing a log message there
   * would rotate the version and strand in-flight executions (ADR 0104).
   */
  abandonIngress(
    source: ReviewIngressSource,
    reason: string,
    reviewId?: string,
  ): Promise<void>;
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
  /** Tear down a superseded worker without rewriting the transaction's status. */
  cleanupSupersededReview(
    reviewId: string,
    opts?: { sessionId?: string },
  ): Promise<void>;
}

interface ReviewControlPlaneStore extends Pick<
  ReviewStore,
  | "claimTargetId"
  | "upsertTarget"
  | "beginReviewPass"
  | "updateReviewPassContext"
  | "getReview"
  | "listPriorPasses"
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
  writeFile(input: AsyncIterable<{
    frame:
      | {
        case: "metadata";
        value: {
          sessionId: string;
          path: string;
          sizeBytes: bigint;
          sha256: string;
          mode?: number;
        };
      }
      | { case: "chunk"; value: Uint8Array };
  }>): Promise<{ path: string; sizeBytes: bigint; sha256: string }>;
  sendPrompt(req: { sessionId: string; promptId: string; text: string }): Promise<unknown>;
}

interface FileToStage {
  path: string;
  content: Uint8Array;
  mode: number;
}

async function stageFiles(
  sessions: ReviewSessionsClient,
  sessionId: string,
  files: readonly FileToStage[],
): Promise<Array<{ path: string; ok: boolean; error?: string }>> {
  const results: Array<{ path: string; ok: boolean; error?: string }> = [];
  for (const file of files) {
    const sha256 = createHash("sha256").update(file.content).digest("hex");
    async function* frames() {
      yield {
        frame: {
          case: "metadata" as const,
          value: {
            sessionId,
            path: file.path,
            sizeBytes: BigInt(file.content.byteLength),
            sha256,
            mode: file.mode,
          },
        },
      };
      if (file.content.byteLength > 0) {
        yield { frame: { case: "chunk" as const, value: file.content } };
      }
    }
    try {
      await sessions.writeFile(frames());
      results.push({ path: file.path, ok: true });
    } catch (error) {
      results.push({
        path: file.path,
        ok: false,
        error: error instanceof Error ? error.message : String(error),
      });
    }
  }
  return results;
}

type RenderReviewer = (opts: RenderReviewerOptions) => RenderedReviewerFile[];
type CreateExistingTaskSession = (
  params: CreateSessionForExistingTaskParams,
) => Promise<{ sessionId: string }>;

export interface ReviewControlPlaneDeps {
  reviews?: ReviewControlPlaneStore;
  db?: ReturnType<typeof getDb>;
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
  /** Hands a resolved request to the review workflow. Injected rather than
   *  imported: `dispatch-review` reaches `pr-review`, which reaches this module,
   *  so a direct import would close a cycle. */
  dispatchPass?: (input: StartReviewPassInput) => Promise<void>;
  signalSupersededPass?: (
    reviewId: string,
    idempotencyKey: string,
  ) => Promise<void>;
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

// Cross-round context (prior-findings.json): how many earlier passes ride
// along, and how much of each finding body / author reply survives the clip.
const PRIOR_PASS_LIMIT = 10;
const PRIOR_TEXT_MAX = 1500;
const PRIOR_FINDINGS_PATH = "/workspace/.review/prior-findings.json";

function clipPriorText(text: string): string {
  if (text.length <= PRIOR_TEXT_MAX) return text;
  return `${text.slice(0, PRIOR_TEXT_MAX)}…`;
}

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

export function makeReviewControlPlane(
  deps: ReviewControlPlaneDeps = {},
): ReviewControlPlane {
  let resolvedDb = deps.db;
  const db = () => (resolvedDb ??= getDb());
  let reviewStore = deps.reviews;
  const reviews = () => (reviewStore ??= makeReviewStore(db()));
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
  const logRefusedTransition = (
    reviewId: string,
    attemptedStatus: string,
  ): void => {
    log.warn(
      { reviewId, attemptedStatus },
      "refused a late review transition because the row is already terminal",
    );
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
  // Cross-round review context: everything earlier passes of this pull request
  // reported, with verdict reasons and the author's inline replies. Best-effort
  // by construction — a re-review must still run when the history read or
  // GitHub fails, so every failure degrades to "no context file", never a
  // throw. Returns null on a first review, which is the common case.
  const buildPriorFindingsFile = async (
    reviewId: string,
  ): Promise<{ path: string; content: string } | null> => {
    try {
      const detail = await reviews().getReview(reviewId);
      if (!detail) return null;
      const { targetId, repo, prNumber } = detail.review;
      const passes: PriorReviewPass[] = await reviews().listPriorPasses(targetId, {
        excludeReviewId: reviewId,
        limit: PRIOR_PASS_LIMIT,
      });
      if (passes.length === 0) return null;

      const priorTitles = new Set(
        passes.flatMap((pass) => pass.findings.map((finding) => finding.title)),
      );
      // Author replies live only on GitHub. A posted finding carries no thread
      // id, so a reply is matched to its finding through the parent comment's
      // first line, which quotes the finding title verbatim.
      const authorReplies: Array<{
        finding_title: string;
        author: string;
        body: string;
      }> = [];
      try {
        const comments = await githubPoster.listReviewComments(repo, prNumber);
        const roots = new Map(comments.map((comment) => [comment.id, comment]));
        for (const comment of comments) {
          if (comment.inReplyToId === null) continue;
          const parent = roots.get(comment.inReplyToId);
          if (!parent) continue;
          const firstLine = parent.body.split("\n", 1)[0] ?? "";
          const title = [...priorTitles].find((candidate) =>
            candidate !== "" && firstLine.includes(candidate)
          );
          if (title === undefined) continue;
          authorReplies.push({
            finding_title: title,
            author: comment.authorLogin,
            body: clipPriorText(comment.body),
          });
        }
      } catch (err) {
        log.warn(
          { reviewId, repo, prNumber, err },
          "prior-review reply fetch failed; staging DB context only (best-effort)",
        );
      }

      const payload = {
        pull_request: prNumber,
        current_review_id: reviewId,
        prior_passes: passes.map((pass) => ({
          review_id: pass.reviewId,
          head_sha: pass.headSha,
          trigger: pass.trigger,
          status: pass.status,
          created_at: pass.createdAt.toISOString(),
          findings: pass.findings.map((finding) => ({
            title: finding.title,
            path: finding.path,
            start_line: finding.startLine,
            end_line: finding.endLine,
            category: finding.category,
            severity: finding.severity,
            confidence: finding.confidence,
            state: finding.state,
            verdict_reason: finding.verdictReason,
            body_md: clipPriorText(finding.bodyMd),
          })),
        })),
        author_replies: authorReplies,
      };
      return { path: PRIOR_FINDINGS_PATH, content: JSON.stringify(payload) };
    } catch (err) {
      log.warn(
        { reviewId, err },
        "prior-findings context build failed; reviewing without it (best-effort)",
      );
      return null;
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
  const dispatchPass = deps.dispatchPass ?? (async (input: StartReviewPassInput) => {
    // Imported at call time, not at module load: `dispatch-review` reaches
    // `pr-review`, which reaches this module. A top-level import would close the
    // cycle and leave one of the three partially initialised.
    const { dispatchReviewPass } = await import("./dispatch-review.ts");
    await dispatchReviewPass(input);
  });
  const signalSupersededPass = deps.signalSupersededPass
    ?? (async (reviewId: string, idempotencyKey: string) => {
      const { dispatchReviewSupersede } = await import("./dispatch-review.ts");
      await dispatchReviewSupersede(reviewId, idempotencyKey);
    });
  // Hoisted out of the returned object so `abandonIngress` can reuse it without
  // reaching back through `this`, which a plain object literal cannot do safely.
  const failReview = async (
    reviewId: string,
    opts: { sessionId?: string; reason?: string } = {},
  ): Promise<void> => {
    await cleanupWorkerSession(opts.sessionId);
    if (!(await reviews().updateReviewStatus(reviewId, "failed"))) {
      logRefusedTransition(reviewId, "failed");
      return;
    }
    await recordEvent(reviewId, "failed", opts.reason);
    await ackStatus(reviewId, "failed");
  };

  return {
    async resolvePrHeads(repo, prNumber) {
      return githubPoster.fetchPrContext(repo, prNumber);
    },

    async resolveReviewTarget(input) {
      // Adopt a row that is still waiting for an id before inserting a new one.
      // Without this, a pull request whose row predates the id column would get a
      // SECOND row on its next review, splitting its history in two. Returns null
      // in the ordinary case, where every row already has an id.
      const claimed = await reviews().claimTargetId({
        provider: input.provider,
        providerId: input.providerId,
        repo: input.repo,
        number: input.number,
      });
      if (claimed) {
        log.info(
          { provider: input.provider, repo: input.repo, number: input.number },
          "adopted a review target that had no provider id",
        );
      }
      const target = await reviews().upsertTarget(input);
      return { targetId: target.id };
    },

    /**
     * NOTHING FALLIBLE MAY FOLLOW `beginReviewPass` IN THIS METHOD.
     *
     * `beginReviewPass` is one transaction and it is NOT idempotent: re-running it
     * either deduplicates onto the row it just created (automation, leaving the
     * pass unstarted) or supersedes that row and creates a second one (a human
     * trigger, leaving a spurious dossier). Ingress runs this as a step with
     * retries allowed, and DBOS re-invokes the whole callback on any throw. So a
     * fallible call placed after the commit would turn its first transient error
     * into a double-create — no crash required.
     *
     * The GitHub status ack used to sit here. It is now its own step
     * (`acknowledgeReviewPass`), which is why retries are safe: a throw can only
     * come from the transaction itself, and that means it rolled back and created
     * nothing. Logging below is a synchronous, infallible write, not an effect.
     */
    async createReviewPass(input) {
      const result = await reviews().beginReviewPass(input);
      const where = {
        repo: input.repo,
        prNumber: input.prNumber,
        trigger: input.trigger,
        targetId: input.targetId,
        headSha: input.headSha,
        reviewId: result.reviewId,
      };
      if (result.kind === "deduplicated") {
        log.info(where, "review request deduplicated onto the active pass");
        return result;
      }
      log.info(
        {
          ...where,
          ...(result.supersededReviewId !== undefined
            ? { supersededReviewId: result.supersededReviewId }
            : {}),
        },
        "review pass created",
      );
      return result;
    },

    async acknowledgeReviewPass(reviewId) {
      // Best-effort by construction: ackStatus swallows its own failures, so this
      // never throws. It is still a step so a replay does not re-post the comment.
      await ackStatus(reviewId, "acknowledged");
    },

    async updateReviewPassContext(reviewId, input) {
      const updated = await reviews().updateReviewPassContext(reviewId, input);
      if (!updated) {
        log.warn(
          { reviewId, attemptedStatus: "queued-context" },
          "refused a late review-pass context write to a terminal row",
        );
      }
      return updated;
    },

    async startReviewPass(input) {
      const where = {
        repo: input.repo,
        prNumber: input.prNumber,
        trigger: input.trigger,
        reviewId: input.reviewId,
        headSha: input.headSha,
      };
      log.info(where, "dispatching a review pass");
      await dispatchPass(input);
      log.info(where, "review pass dispatched");
    },

    async signalSupersededPass(reviewId, idempotencyKey) {
      log.info({ reviewId }, "signalling a superseded review pass to tear down");
      await signalSupersededPass(reviewId, idempotencyKey);
    },

    async abandonIngress(source, reason, reviewId) {
      if (reviewId === undefined) {
        // Nothing was created, so there is no row to carry this. The log is the
        // only record — which is why it is an error, not a warning.
        log.error(
          { ...source, reason },
          "review ingress gave up before it could identify a target",
        );
        return;
      }
      log.error({ ...source, reviewId, reason }, "review ingress gave up; failing the pass");
      await failReview(reviewId, { reason });
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
        taskType: "pr_review",
        profileId,
        integrationPrincipalId: "automation:pr-review",
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
      const priorFindings = await buildPriorFindingsFile(input.reviewId);
      const files = [
        ...renderReviewer({
          role: "finder",
          ...(input.enabledCategories ? { enabledCategories: input.enabledCategories } : {}),
          ...(input.orgInstructions !== undefined
            ? { orgInstructions: input.orgInstructions }
            : {}),
        }),
        ...(priorFindings ? [priorFindings] : []),
      ].map((file) => ({
        path: file.path,
        content: encoder.encode(file.content),
        mode: 0o644,
      }));
      const results = await stageFiles(sessions, sessionId, files);
      const failed = results.find((result) => !result.ok);
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

      // Re-review scoping: an automation pass after a push reviews the DELTA
      // since the last posted review, not the whole PR again. Full re-reviews
      // re-derive the same findings, re-post duplicates, and flag the rework
      // the previous round caused — the audited treadmill. Human triggers keep
      // the full range. Best-effort: a failed context read only widens the
      // scope back to the full PR.
      let lastReviewedHead = "";
      let hasPriorPasses = false;
      try {
        const detail = await reviews().getReview(input.reviewId);
        if (detail) {
          const passes = await reviews().listPriorPasses(detail.review.targetId, {
            excludeReviewId: input.reviewId,
            limit: PRIOR_PASS_LIMIT,
          });
          hasPriorPasses = passes.length > 0;
          if (detail.review.trigger === "synchronize") {
            const lastPosted = passes.find((pass) =>
              pass.status === "posted"
              && SHA_RE.test(pass.headSha)
              && pass.headSha !== input.headSha
            );
            lastReviewedHead = lastPosted?.headSha ?? "";
          }
        }
      } catch (err) {
        log.warn(
          { reviewId: input.reviewId, err },
          "re-review scoping read failed; reviewing the full range (best-effort)",
        );
      }

      const prompt = [
        `Review ${input.repo} pull request #${input.prNumber}.`,
        `Analyze ${range} in /workspace/${name}.`,
        ...(mergeBase !== "" ? [`base_sha is ${mergeBase} (the merge base).`] : []),
        ...(lastReviewedHead !== ""
          ? [
              `This is an automatic re-review after a push. The last posted review examined ${lastReviewedHead}.`,
              `Report new findings only from the changes since it: \`git diff ${lastReviewedHead}...${input.headSha}\`. The full range is context, not new review surface. If that commit is absent from the clone (force-push), fall back to the full range.`,
            ]
          : []),
        ...(hasPriorPasses
          ? [
              `Earlier review rounds for this pull request are recorded at ${PRIOR_FINDINGS_PATH} (when present) — read them before reviewing.`,
            ]
          : []),
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
      if (!(await reviews().updateReviewStatus(input.reviewId, "finding"))) {
        logRefusedTransition(input.reviewId, "finding");
        return;
      }
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
        taskType: "pr_review",
        profileId,
        integrationPrincipalId: "automation:pr-review",
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
      const priorFindings = await buildPriorFindingsFile(input.reviewId);
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
        ...(priorFindings ? [priorFindings] : []),
      ].map((file) => ({
        path: file.path,
        content: encoder.encode(file.content),
        mode: 0o644,
      }));
      const results = await stageFiles(sessions, sessionId, files);
      const failed = results.find((result) => !result.ok);
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
      if (!(await reviews().updateReviewStatus(input.reviewId, "verifying"))) {
        logRefusedTransition(input.reviewId, "verifying");
        return;
      }
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
        const updated = await reviews().finalizeReview(reviewId, {
          status: detail.review.status,
          summaryMd: detail.review.summaryMd ?? "",
          headSha,
          baseSha,
        });
        if (!updated) {
          logRefusedTransition(reviewId, detail.review.status);
          return;
        }
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
        const finalized = await reviews().finalizeReview(reviewId, {
          status: "posted",
          summaryMd: detail.review.summaryMd ?? "",
        });
        if (!finalized) {
          logRefusedTransition(reviewId, "posted");
          return;
        }
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
      const finalized = await reviews().finalizeReview(reviewId, {
        status: "posted",
        summaryMd: posted.summaryMd,
        ...(posted.githubReviewId !== undefined
          ? { githubReviewId: posted.githubReviewId }
          : {}),
      });
      if (!finalized) {
        logRefusedTransition(reviewId, "posted");
        return;
      }
      const surfaced = decision.toPost.length + decision.uiOnly.length;
      await recordEvent(reviewId, "posted", postedSummary(surfaced));
      await ackStatus(reviewId, "posted", surfaced);
    },

    failReview,

    async haltReview(reviewId, opts = {}) {
      await cleanupWorkerSession(opts.sessionId);
      if (!(await reviews().updateReviewStatus(reviewId, "halted"))) {
        logRefusedTransition(reviewId, "halted");
        return;
      }
      await recordEvent(reviewId, "halted");
      await ackStatus(reviewId, "halted");
    },

    async cleanupSupersededReview(_reviewId, opts = {}) {
      // The ingress transaction already committed `superseded`. This step owns
      // only teardown; writing status here would reintroduce the race B6 removes.
      await cleanupWorkerSession(opts.sessionId);
    },
  };
}
