/** The built-in actions every git forge has, against a forge that is not
 * GitHub. */

import { describe, expect, test } from "bun:test";

import { IntegrationActionError } from "../../automations/actions/errors.ts";
import { forgeActions } from "../actions.ts";
import { ForgeRequestError } from "../forge.ts";
import { FAKE_PROVIDER, makeFakeForge, type FakeForgeState } from "./fake-forge.ts";

const CTX = { runId: "autorun:a:fakeforge:d1", stepPath: "post", marker: null };
const MARKED = { ...CTX, marker: "<!-- engrams-pinned:status -->" };

function run(
  name: string,
  params: Record<string, unknown>,
  ctx: { runId: string; stepPath: string; marker: string | null } = CTX,
  state: Partial<FakeForgeState> = {},
) {
  const fake = makeFakeForge(state);
  const result = forgeActions.run(FAKE_PROVIDER, name, params, ctx, { clients: { [FAKE_PROVIDER]: fake.forge } });
  return { result, state: fake.state };
}

describe("post_pr_review", () => {
  const params = {
    repo: "acme/app",
    prNumber: 7,
    commitId: "c".repeat(40),
    summary: "Two findings.",
    fallbackSummary: "Two findings, quoted.",
    comments: [
      { finding_id: "f-1", path: "a.ts", line: 3, body: "one" },
      { finding_id: "f-2", path: "b.ts", line: 9, start_line: 4, side: "LEFT", body: "two" },
    ],
  };

  test("posts the review and names its id and the findings on the diff", async () => {
    const { result, state } = run("post_pr_review", params);
    expect(await result).toEqual({
      posted: true,
      inline_posted: true,
      inline_finding_ids: ["f-1", "f-2"],
      already_posted: false,
      review_id: "r-1",
      github_review_id: "r-1",
    });
    expect(state.calls).toEqual(["hasReviewWithMarker:7", "postReview:7"]);
    // The default marker pins the post to this step of this run.
    expect(state.reviews[0]!.body).toBe(`Two findings.\n\n<!-- engrams-automation:${CTX.runId}:post -->`);
  });

  test("a summary-only post names the findings that are still on the diff", async () => {
    const { result, state } = run("post_pr_review", params, CTX, {
      postReview: async (input) => {
        expect(input.comments.map((comment) => comment.findingId)).toEqual(["f-1", "f-2"]);
        expect(input.comments[1]).toMatchObject({ side: "LEFT", startLine: 4 });
        return {
          inlinePosted: false,
          inlineFindingIds: ["f-1"],
          summaryMd: input.buildSummary(false),
        };
      },
    });
    expect(await result).toEqual({
      posted: true,
      inline_posted: false,
      inline_finding_ids: ["f-1"],
      already_posted: false,
    });
    expect(state.reviews[0]!.body).toStartWith("Two findings, quoted.");
  });

  test("a review that already carries the marker is not posted again", async () => {
    const marker = `<!-- engrams-automation:${CTX.runId}:post -->`;
    const { result, state } = run("post_pr_review", params, CTX, {
      reviews: [{ prNumber: 7, body: `earlier\n\n${marker}` }],
    });
    expect(await result).toEqual({ posted: false, already_posted: true });
    expect(state.calls).toEqual(["hasReviewWithMarker:7"]);
  });

  test("with no commit the head of the pull request is reviewed", async () => {
    const { commitId: _commitId, ...withoutCommit } = params;
    const { result, state } = run("post_pr_review", withoutCommit);
    await result;
    expect(state.calls).toEqual(["hasReviewWithMarker:7", "fetchPullRequest:7", "postReview:7"]);
  });

  test("a malformed comment and a forge 4xx are permanent; a 429 is not", async () => {
    await expect(
      run("post_pr_review", { ...params, comments: [{ path: "a.ts", body: "no line" }] }).result,
    ).rejects.toMatchObject({ permanent: true });
    for (const [status, permanent] of [[404, true], [429, false], [502, false]] as const) {
      const { result } = run("post_pr_review", params, CTX, {
        postReview: async () => {
          throw new ForgeRequestError(status, `status ${status}`, "");
        },
      });
      const error = await result.catch((e: unknown) => e);
      expect(error).toBeInstanceOf(IntegrationActionError);
      expect(error).toMatchObject({ permanent, status });
    }
  });
});

describe("upsert_pr_comment and update_pr_comment", () => {
  test("upsert posts the body with the marker once, then returns that comment", async () => {
    const comments: FakeForgeState["comments"] = new Map();
    const first = run("upsert_pr_comment", { repo: "acme/app", number: 7, body: "Reviewing." }, MARKED, { comments });
    expect(await first.result).toEqual({ commentId: "c1" });
    expect(comments.get("c1")!.body).toBe(`Reviewing.\n\n${MARKED.marker}`);

    const again = run("upsert_pr_comment", { repo: "acme/app", number: 7, body: "Reviewing again." }, MARKED, {
      comments,
    });
    expect(await again.result).toEqual({ commentId: "c1" });
    expect(again.state.calls).toEqual(["findCommentWithMarker:7"]);
    expect(comments.size).toBe(1);
  });

  test("upsert does not take a comment that another author posted with the marker", async () => {
    const comments: FakeForgeState["comments"] = new Map([
      ["c1", { prNumber: 7, body: `quoted ${MARKED.marker}`, ours: false }],
    ]);
    const { result } = run("upsert_pr_comment", { repo: "acme/app", number: 7, body: "Reviewing." }, MARKED, {
      comments,
    });
    expect(await result).toEqual({ commentId: "c2" });
  });

  test("a forge error while it looks for the comment fails the upsert; it posts nothing", async () => {
    const { forge, state } = makeFakeForge();
    forge.findCommentWithMarker = async () => {
      throw new ForgeRequestError(502, "bad gateway", "");
    };
    const result = forgeActions.run(
      FAKE_PROVIDER,
      "upsert_pr_comment",
      { repo: "acme/app", number: 7, body: "x" },
      MARKED,
      { clients: { [FAKE_PROVIDER]: forge } },
    );
    await expect(result).rejects.toMatchObject({ permanent: false, status: 502 });
    expect(state.comments.size).toBe(0);
  });

  test("update rewrites the comment and keeps the marker", async () => {
    const comments: FakeForgeState["comments"] = new Map([["7", { prNumber: 7, body: "old", ours: true }]]);
    const { result } = run("update_pr_comment", { repo: "acme/app", commentId: "7", body: "Done." }, MARKED, {
      comments,
    });
    expect(await result).toEqual({ commentId: "7" });
    expect(comments.get("7")!.body).toBe(`Done.\n\n${MARKED.marker}`);
  });

  test("both need marker idempotency", async () => {
    for (const [name, params] of [
      ["upsert_pr_comment", { repo: "acme/app", number: 7, body: "x" }],
      ["update_pr_comment", { repo: "acme/app", commentId: "c1", body: "x" }],
    ] as const) {
      await expect(run(name, params).result).rejects.toMatchObject({
        permanent: true,
        message: expect.stringContaining("marker_comment"),
      });
    }
  });
});
