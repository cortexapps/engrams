/** Durable review-record and finder-session operations behind the workflow seam. */

import { Code, ConnectError } from "@connectrpc/connect";

import { getDb } from "../db/client.ts";
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
  type ReviewStatusPhase,
} from "../reviews/github-review.ts";
import {
  runPolicyGate,
  type FindingDecision,
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
}

export interface ReviewControlPlane {
  resolvePrHeads(repo: string, prNumber: number): Promise<{
    headSha: string;
    baseSha: string;
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
  getReview(reviewId: string): Promise<ReviewDetail | null>;
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
    headSha: string;
    baseSha: string;
  }): Promise<void>;
  deleteReviewSession(sessionId: string): Promise<void>;
  postReviewResults(reviewId: string): Promise<void>;
  markReviewFailed(reviewId: string): Promise<void>;
  markReviewHalted(repo: string, prNumber: number): Promise<void>;
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

interface ReviewExecOutput {
  event:
    | { case: "started"; value: unknown }
    | { case: "stdout"; value: Uint8Array }
    | { case: "stderr"; value: Uint8Array }
    | { case: "exit"; value: { exitStatus?: number } }
    | { case: undefined; value?: undefined };
}

export interface ReviewSessionsClient extends TaskSessionsClient {
  exec(req: { sessionId: string; command: string }): AsyncIterable<ReviewExecOutput>;
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
}

/** Human detail for a `posted` activity-log entry. */
function postedSummary(count: number): string {
  if (count === 0) return "No findings";
  return `${count} finding${count === 1 ? "" : "s"} posted`;
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

  const stderrDecoder = new TextDecoder();
  let stderr = "";
  let exitStatus: number | undefined;
  for await (const message of sessions.exec({ sessionId, command })) {
    if (message.event.case === "stderr") {
      stderr += stderrDecoder.decode(message.event.value, { stream: true });
    } else if (message.event.case === "exit") {
      exitStatus = message.event.value.exitStatus;
    }
  }
  stderr += stderrDecoder.decode();
  if (exitStatus !== 0) {
    const detail = stderr.trim() || "exec stream ended without a successful exit status";
    throw new ReviewSetupError(`${phase} clone failed: ${detail}`);
  }
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
      return githubPoster.fetchPrHeads(repo, prNumber);
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
      const reviewId = await reviews().createReview({
        ...input,
        taskId,
        status: "queued",
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
      await cloneRepo(sessions, sessionId, input.repo, input.headSha, "finder");

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
      const range = input.baseSha !== "" && input.headSha !== ""
        ? `${input.baseSha}..${input.headSha}`
        : "the PR diff";
      const prompt = [
        `Review ${input.repo} pull request #${input.prNumber}.`,
        `Analyze ${range} in /workspace/${name}.`,
        "Read /workspace/.review/finder.md and follow its instructions before reviewing.",
        ...(input.focus?.trim() ? [`Focus directive: ${input.focus.trim()}`] : []),
        "Report findings only through the provided review tools; do not edit files or push changes.",
      ].join("\n");

      await sessions.sendPrompt({
        sessionId,
        promptId: `review:${input.reviewId}:finder`,
        text: prompt,
      });
      await reviews().updateReviewStatus(input.reviewId, "finding");
      await recordEvent(input.reviewId, "reviewing");
      await ackStatus(input.reviewId, "finding");
    },

    async getReview(reviewId) {
      return reviews().getReview(reviewId);
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
      await cloneRepo(sessions, sessionId, input.repo, input.headSha, "verifier");

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

      await sessions.sendPrompt({
        sessionId,
        promptId: `review:${input.reviewId}:verifier`,
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

    async deleteReviewSession(sessionId) {
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
    },

    async postReviewResults(reviewId) {
      const detail = await reviews().getReview(reviewId);
      if (!detail) throw new Error(`review not found: ${reviewId}`);

      const { repo, prNumber } = detail.review;
      let { headSha, baseSha } = detail.review;
      if (headSha === "" || baseSha === "") {
        const live = await githubPoster.fetchPrHeads(repo, prNumber);
        if (headSha === "") headSha = live.headSha;
        if (baseSha === "") baseSha = live.baseSha;
        await reviews().finalizeReview(reviewId, {
          status: detail.review.status,
          summaryMd: detail.review.summaryMd ?? "",
          headSha,
          baseSha,
        });
      }

      // The marker check closes the crash window between GitHub accepting the
      // review and the local transaction recording it.
      if (await githubPoster.alreadyPosted(repo, prNumber, reviewId)) {
        await reviews().finalizeReview(reviewId, {
          status: "posted",
          summaryMd: detail.review.summaryMd ?? "",
        });
        const postedCount = detail.findings.filter(
          (finding) => finding.state === "posted" || finding.state === "ui_only",
        ).length;
        await recordEvent(reviewId, "posted", postedSummary(postedCount));
        await ackStatus(reviewId, "posted", postedCount);
        return;
      }

      const policy = runPolicyGate(detail);
      const missingAnchors: FindingDecision[] = [];
      const anchored = policy.toPost.filter((item) => {
        const hasAnchor = item.finding.endLine != null || item.finding.startLine != null;
        if (!hasAnchor) missingAnchors.push({ ...item, state: "ui_only" });
        return hasAnchor;
      });
      const decision = {
        ...policy,
        toPost: anchored,
        uiOnly: [...policy.uiOnly, ...missingAnchors],
      };
      const comments = anchored.map((item) => {
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

      for (const item of decision.toPost) {
        await reviews().updateFindingState(
          item.finding.id,
          posted.inlinePosted ? "posted" : "ui_only",
          item.verdictReason == null ? undefined : { verdictReason: item.verdictReason },
        );
      }
      for (const item of decision.uiOnly) {
        await reviews().updateFindingState(
          item.finding.id,
          "ui_only",
          item.verdictReason == null ? undefined : { verdictReason: item.verdictReason },
        );
      }
      for (const item of decision.suppressed) {
        await reviews().updateFindingState(
          item.finding.id,
          item.state,
          item.verdictReason == null ? undefined : { verdictReason: item.verdictReason },
        );
      }
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

    async markReviewFailed(reviewId) {
      await reviews().updateReviewStatus(reviewId, "failed");
      await recordEvent(reviewId, "failed");
      await ackStatus(reviewId, "failed");
    },

    async markReviewHalted(repo, prNumber) {
      const active = await reviews().getActiveReviewForPr(repo, prNumber);
      if (active) {
        await reviews().updateReviewStatus(active.id, "halted");
        await recordEvent(active.id, "halted");
        await ackStatus(active.id, "halted");
      }
    },
  };
}
