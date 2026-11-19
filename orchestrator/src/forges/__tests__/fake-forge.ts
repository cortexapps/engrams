/** A git forge for tests of the forge-generic code. It is not GitHub: its
 * repository key drops a trailing `.git` and the case, and the event a
 * payload means is the payload's `event` field. */

import { EMPTY_PR_CONTEXT } from "../pr-context.ts";
import {
  ForgeRequestError,
  type GitForge,
  type PostReviewInput,
  type PostReviewResult,
  type ReviewCommentEvent,
  type ReviewEvent,
  type ReviewEventPullRequest,
} from "../forge.ts";

export const FAKE_PROVIDER = "fakeforge";

export function fakePullRequest(overrides: Partial<ReviewEventPullRequest> = {}): ReviewEventPullRequest {
  return {
    repo: "Acme/App.git",
    number: 7,
    url: "https://forge.test/acme/app/7",
    headSha: "h".repeat(40),
    baseSha: "b".repeat(40),
    context: { ...EMPTY_PR_CONTEXT, providerId: "pr-7", title: "Fix", state: "open" },
    ...overrides,
  };
}

export function fakeComment(body: string, overrides: Partial<ReviewCommentEvent> = {}): ReviewCommentEvent {
  return {
    kind: "comment",
    body,
    authorId: "dev",
    fromBot: false,
    authorAccess: null,
    pr: fakePullRequest(),
    ...overrides,
  };
}

/** A payload whose meaning is `event`. */
export function eventPayload(event: ReviewEvent): Record<string, unknown> {
  return { event };
}

export interface FakeForgeState {
  /** The authors `mayRequestReview` accepts. */
  maintainers: Set<string>;
  /** Every `mayRequestReview` call, by author. */
  asked: string[];
  /** Comments by id; the id is `c<n>`. */
  comments: Map<string, { prNumber: number; body: string; ours: boolean }>;
  reviews: Array<{ prNumber: number; body: string }>;
  postReview: (input: PostReviewInput) => Promise<PostReviewResult>;
  calls: string[];
}

export function makeFakeForge(state: Partial<FakeForgeState> = {}): { forge: GitForge; state: FakeForgeState } {
  const s: FakeForgeState = {
    maintainers: state.maintainers ?? new Set(["maintainer"]),
    asked: [],
    comments: state.comments ?? new Map(),
    reviews: state.reviews ?? [],
    postReview:
      state.postReview ??
      (async (input) => ({
        providerReviewId: "r-1",
        inlinePosted: true,
        inlineFindingIds: input.comments.map((comment) => comment.findingId),
        summaryMd: input.buildSummary(true),
      })),
    calls: [],
  };
  const unused = (name: string) => () => {
    throw new Error(`unexpected forge call: ${name}`);
  };
  const forge: GitForge = {
    provider: FAKE_PROVIDER,
    links: {
      pullRequest: (repo, number) => `https://forge.test/${repo}/${number}`,
      review: (prUrl, id) => `${prUrl}#${id}`,
      file: (prUrl) => prUrl,
    },
    repoKey: (repo) => repo.toLowerCase().replace(/\.git$/, ""),
    parsePullRequestUrl: unused("parsePullRequestUrl"),
    isPermanentError: (error) => error instanceof ForgeRequestError && error.isClientError,
    reviewEvent: (_eventKey, payload) => (payload["event"] as ReviewEvent | undefined) ?? null,
    async mayRequestReview(comment) {
      s.asked.push(comment.authorId);
      return s.maintainers.has(comment.authorId);
    },
    async fetchPullRequest(_repo, prNumber) {
      s.calls.push(`fetchPullRequest:${prNumber}`);
      return { headSha: "f".repeat(40), baseSha: "b".repeat(40), pr: EMPTY_PR_CONTEXT };
    },
    listReviewComments: unused("listReviewComments"),
    async postReview(input) {
      s.calls.push(`postReview:${input.prNumber}`);
      const result = await s.postReview(input);
      s.reviews.push({ prNumber: input.prNumber, body: result.summaryMd });
      return result;
    },
    async hasReviewWithMarker(_repo, prNumber, marker) {
      s.calls.push(`hasReviewWithMarker:${prNumber}`);
      return s.reviews.some((review) => review.prNumber === prNumber && review.body.includes(marker));
    },
    async createComment(_repo, prNumber, body) {
      const id = `c${s.comments.size + 1}`;
      s.calls.push(`createComment:${prNumber}`);
      s.comments.set(id, { prNumber, body, ours: true });
      return id;
    },
    async updateComment(_repo, commentId, body) {
      s.calls.push(`updateComment:${commentId}`);
      const comment = s.comments.get(commentId);
      if (!comment) throw new Error(`no comment ${commentId}`);
      comment.body = body;
    },
    async findCommentWithMarker(_repo, prNumber, marker) {
      s.calls.push(`findCommentWithMarker:${prNumber}`);
      const found = [...s.comments].reverse().find(
        ([, comment]) => comment.ours && comment.prNumber === prNumber && comment.body.includes(marker),
      );
      return found?.[0] ?? null;
    },
  };
  return { forge, state: s };
}
