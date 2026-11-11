import { describe, expect, test } from "bun:test";

// This module loads before the forge registry here; the registry must not
// read a provider's spec while it loads.
import { makeGitlabForge } from "../forge.ts";
import { registerEngineBlocks } from "../../../automations/engine/blocks/index.ts";
import { gitlabEventSource } from "../events.ts";
import { GITLAB_REVIEW } from "../review-variant.ts";
import { reviewDefinition } from "../../../automations/builtins/review.ts";
import { validateDefinition } from "../../../automations/engine/definition.ts";
import { ForgeRequestError, SAFE_REPO_PATH, type GitForge } from "../../../forges/forge.ts";
import { requireForge, reviewVariantFor } from "../../../forges/registry.ts";
import { reviewFacts } from "../../../forges/review-facts.ts";
import {
  runIntegrationOp,
  type IntegrationOpRequest,
  type IntegrationOpResult,
} from "../../../integrations/run-op.ts";

registerEngineBlocks();

function response(status: number, body: unknown): IntegrationOpResult {
  return {
    status,
    body: new TextEncoder().encode(JSON.stringify(body)),
    contentType: "application/json",
    truncated: false,
  };
}

/** A forge over scripted responses, in call order. */
function scripted(responses: IntegrationOpResult[]) {
  const calls: IntegrationOpRequest[] = [];
  const run: typeof runIntegrationOp = async (provider, request) => {
    expect(provider).toBe("gitlab");
    calls.push(request);
    const next = responses.shift();
    if (!next) throw new Error("no scripted response left");
    return next;
  };
  return { calls, forge: makeGitlabForge({ runIntegrationOp: run }) };
}

const REPO = "acme/platform/api";
const MR_PATH = "/api/v4/projects/acme%2Fplatform%2Fapi/merge_requests/7";
const DIFF_REFS = { base_sha: "b".repeat(40), start_sha: "c".repeat(40), head_sha: "a".repeat(40) };

/** The merge request as `GET merge_requests/:iid` returns it. */
const MERGE_REQUEST = {
  id: 9001,
  iid: 7,
  title: "Add the endpoint",
  state: "opened",
  draft: false,
  web_url: "https://gitlab.example.com/acme/platform/api/-/merge_requests/7",
  updated_at: "2026-01-16T05:56:25.000Z",
  author: { username: "dev" },
  source_branch: "feature",
  target_branch: "main",
  sha: DIFF_REFS.head_sha,
  diff_refs: DIFF_REFS,
  changes_count: "3",
};

/** A merge-request hook, as GitLab documents it. */
function mergeRequestHook(action: string, attributes: Record<string, unknown> = {}) {
  return {
    object_kind: "merge_request",
    event_type: "merge_request",
    user: { id: 11, username: "dev" },
    project: {
      id: 5,
      path_with_namespace: REPO,
      web_url: "https://gitlab.example.com/acme/platform/api",
    },
    object_attributes: {
      id: 9001,
      iid: 7,
      title: "Add the endpoint",
      state: "opened",
      action,
      draft: false,
      source_branch: "feature",
      target_branch: "main",
      url: MERGE_REQUEST.web_url,
      updated_at: "2026-01-16T05:56:25.000Z",
      last_commit: { id: DIFF_REFS.head_sha },
      ...attributes,
    },
  };
}

function noteHook(note: string, user: Record<string, unknown> = { id: 11, username: "dev" }) {
  return {
    object_kind: "note",
    event_type: "note",
    user,
    project: mergeRequestHook("open").project,
    object_attributes: { id: 301, note, noteable_type: "MergeRequest", action: "create", system: false },
    merge_request: { id: 9001, iid: 7, title: "Add the endpoint", state: "opened", source_branch: "feature", target_branch: "main", last_commit: { id: DIFF_REFS.head_sha } },
  };
}

const INPUTS: Record<string, unknown> = {
  repos: { "Acme/Platform/API": { mode: "auto" } },
  profile: "p1",
  mention: "@engrams",
};

describe("GitLab event source", () => {
  const extract = (headers: Record<string, string>, payload: Record<string, unknown>) =>
    gitlabEventSource.extract(new Headers(headers), payload, new Date(0));

  test("keys a merge-request hook by its action and a comment by what it is on", () => {
    expect(extract({ "webhook-id": "d1" }, mergeRequestHook("open"))).toEqual({
      kind: "event",
      event: { eventKey: "merge_request.open", deliveryId: "d1", scopeValue: "acme/platform/api" },
    });
    expect(extract({ "idempotency-key": "d2" }, noteHook("hi"))).toMatchObject({
      event: { eventKey: "note.merge_request", deliveryId: "d2" },
    });
    expect(extract({ "webhook-id": "d3" }, { object_kind: "push" })).toMatchObject({
      event: { eventKey: "push" },
    });
  });

  test("skips a delivery with no retry-stable id or no kind", () => {
    expect(extract({ "x-gitlab-event-uuid": "u" }, mergeRequestHook("open"))).toMatchObject({ kind: "skip" });
    expect(extract({ "webhook-id": "d" }, {})).toMatchObject({ kind: "skip" });
  });
});

describe("GitLab review facts", () => {
  const facts = (eventKey: string, payload: Record<string, unknown>, forge: GitForge = makeGitlabForge(), inputs = INPUTS) =>
    reviewFacts(forge, { eventKey, payload, inputs });

  test("an opened merge request of an enrolled project starts a review", async () => {
    const value = await facts("merge_request.open", mergeRequestHook("open"));
    expect(value).toMatchObject({
      admit: true,
      repo: REPO,
      repo_name: "api",
      pr_number: 7,
      pr_url: MERGE_REQUEST.web_url,
      trigger: "opened",
      head_sha: DIFF_REFS.head_sha,
      // The hook carries no diff base: the pass asks the forge.
      base_sha: "",
      pr_context: { providerId: "9001", state: "open", headBranch: "feature", baseBranch: "main" },
    });
    expect(SAFE_REPO_PATH.test(value!.repo)).toBe(true);
  });

  test("an update starts a review only when it brings commits or ends draft", async () => {
    const update = (attributes: Record<string, unknown>, changes?: unknown) =>
      facts("merge_request.update", { ...mergeRequestHook("update", attributes), ...(changes ? { changes } : {}) });
    expect(await update({ oldrev: "d".repeat(40) })).toMatchObject({ trigger: "synchronize" });
    expect(await update({}, { draft: { previous: true, current: false } })).toMatchObject({
      trigger: "ready_for_review",
    });
    expect(await update({}, { title: { previous: "a", current: "b" } })).toBeNull();
  });

  test("a draft, an unlisted project, on-request mode, and no profile start nothing", async () => {
    expect(await facts("merge_request.open", mergeRequestHook("open", { draft: true }))).toBeNull();
    const none = { ...INPUTS, repos: {} };
    expect(await facts("merge_request.open", mergeRequestHook("open"), makeGitlabForge(), none)).toBeNull();
    const onRequest = { ...INPUTS, repos: { [REPO]: { mode: "on_request" } } };
    expect(await facts("merge_request.open", mergeRequestHook("open"), makeGitlabForge(), onRequest)).toBeNull();
    const noProfile = { ...INPUTS, profile: "" };
    expect(await facts("merge_request.open", mergeRequestHook("open"), makeGitlabForge(), noProfile)).toBeNull();
    expect(await facts("merge_request.close", mergeRequestHook("close"))).toBeNull();
  });

  test("a review command needs a member with Developer access", async () => {
    const developer = scripted([response(200, { access_level: 30 }), response(200, { id: 99 })]);
    expect(await facts("note.merge_request", noteHook("please look\n@engrams review"), developer.forge)).toMatchObject({
      trigger: "command",
      pr_number: 7,
      pr_url: MERGE_REQUEST.web_url,
    });
    expect(developer.calls[0]!.path).toBe("/api/v4/projects/acme%2Fplatform%2Fapi/members/all/11");

    // The connection's own user: its review quotes findings, and a command
    // in a quote starts nothing.
    const self = scripted([response(200, { access_level: 40 }), response(200, { id: 11 })]);
    expect(await facts("note.merge_request", noteHook("@engrams review"), self.forge)).toBeNull();

    const reporter = scripted([response(200, { access_level: 20 })]);
    expect(await facts("note.merge_request", noteHook("@engrams review"), reporter.forge)).toBeNull();
    const stranger = scripted([response(404, { message: "404 Not found" })]);
    expect(await facts("note.merge_request", noteHook("@engrams review"), stranger.forge)).toBeNull();
  });

  test("a comment that is no command, or from a token's bot user, asks nobody", async () => {
    const forge = scripted([]);
    expect(await facts("note.merge_request", noteHook("looks good"), forge.forge)).toBeNull();
    const bot = noteHook("@engrams review", { id: 12, username: "project_5_bot_a1b2" });
    expect(await facts("note.merge_request", bot, forge.forge)).toBeNull();
    expect(forge.calls).toEqual([]);
  });

  test("the dispatch payload is admitted without mode auto", async () => {
    const onRequest = { ...INPUTS, repos: { [REPO]: { mode: "on_request" } } };
    const payload = GITLAB_REVIEW.dispatchPayload({ repo: REPO, prNumber: 7 });
    expect(await facts("review.dispatch", payload, makeGitlabForge(), onRequest)).toMatchObject({
      trigger: "dispatch",
      pr_number: 7,
      head_sha: "",
    });
  });
});

describe("GitLab forge", () => {
  test("reads a merge request's heads and context", async () => {
    const { forge, calls } = scripted([response(200, MERGE_REQUEST)]);
    expect(await forge.fetchPullRequest(REPO, 7)).toEqual({
      headSha: DIFF_REFS.head_sha,
      baseSha: DIFF_REFS.base_sha,
      pr: {
        providerId: "9001",
        title: "Add the endpoint",
        author: "dev",
        state: "open",
        url: MERGE_REQUEST.web_url,
        providerUpdatedAt: new Date("2026-01-16T05:56:25.000Z"),
        headBranch: "feature",
        baseBranch: "main",
        additions: null,
        deletions: null,
        changedFiles: 3,
      },
    });
    expect(calls[0]).toMatchObject({ method: "GET", path: MR_PATH });
  });

  test("a merge request with no diff refs yet is a failure a retry can clear", async () => {
    const { forge } = scripted([response(200, { ...MERGE_REQUEST, diff_refs: null })]);
    const error = await forge.fetchPullRequest(REPO, 7).catch((e: unknown) => e);
    expect(error).toBeInstanceOf(ForgeRequestError);
    expect(forge.isPermanentError(error)).toBe(false);
  });

  test("a project the token cannot see is a permanent failure", async () => {
    const { forge } = scripted([response(404, { message: "404 Project Not Found" })]);
    const error = await forge.fetchPullRequest(REPO, 7).catch((e: unknown) => e);
    expect(forge.isPermanentError(error)).toBe(true);
    expect(forge.isPermanentError(new ForgeRequestError(429, "slow down", ""))).toBe(false);
  });

  const comments = [
    { findingId: "f1", path: "src/a.ts", line: 4, side: "RIGHT", body: "A finding." },
    { findingId: "f2", path: "src/a.ts", line: 2, side: "LEFT", body: "Another finding." },
  ];
  const post = (forge: GitForge) =>
    forge.postReview({
      repo: REPO,
      prNumber: 7,
      commitId: DIFF_REFS.head_sha,
      buildSummary: (inline) => `${inline ? "summary" : "summary with every finding"}\n\n<!-- m -->`,
      marker: "<!-- m -->",
      comments,
    });
  // A renamed file. Old line 2 is gone, new lines 2 and 3 are added, and
  // old line 3 is new line 4.
  const DIFFS = [
    {
      old_path: "src/old-a.ts",
      new_path: "src/a.ts",
      diff: "@@ -1,4 +1,5 @@\n one\n-two\n+two!\n+extra\n three\n four\n",
    },
  ];
  const paths = (calls: IntegrationOpRequest[]) => calls.map((call) => `${call.method} ${call.path}`);
  const PAGE = "?per_page=100&page=1";

  test("posts the comments as drafts, publishes each, then posts the summary", async () => {
    const { forge, calls } = scripted([
      // A draft an earlier attempt left, and one that is not this post's.
      response(200, [{ id: 30, note: "A finding.\n\n<!-- m -->" }, { id: 31, note: "a person's draft" }]),
      response(204, {}),
      response(200, MERGE_REQUEST),
      response(200, []),
      response(200, DIFFS),
      response(201, { id: 41 }),
      response(201, { id: 42 }),
      response(200, {}),
      response(200, {}),
      response(201, { id: 500 }),
    ]);
    expect(await post(forge)).toEqual({
      providerReviewId: "500",
      posted: true,
      inlinePosted: true,
      summaryMd: "summary\n\n<!-- m -->",
    });
    expect(paths(calls)).toEqual([
      `GET ${MR_PATH}/draft_notes${PAGE}`,
      `DELETE ${MR_PATH}/draft_notes/30`,
      `GET ${MR_PATH}`,
      `GET ${MR_PATH}/discussions${PAGE}`,
      `GET ${MR_PATH}/diffs${PAGE}`,
      `POST ${MR_PATH}/draft_notes`,
      `POST ${MR_PATH}/draft_notes`,
      `PUT ${MR_PATH}/draft_notes/41/publish`,
      `PUT ${MR_PATH}/draft_notes/42/publish`,
      `POST ${MR_PATH}/notes`,
    ]);
    const where = { ...DIFF_REFS, position_type: "text", new_path: "src/a.ts", old_path: "src/old-a.ts" };
    // A line both sides have carries both numbers; a removed line only its own.
    expect(JSON.parse(calls[5]!.body as string)).toEqual({
      note: "A finding.\n\n<!-- m -->",
      position: { ...where, new_line: 4, old_line: 3 },
    });
    expect(JSON.parse(calls[6]!.body as string).position).toEqual({ ...where, old_line: 2 });
  });

  test("an attempt after a partial publish posts only what is missing", async () => {
    const { forge, calls } = scripted([
      response(200, []),
      response(200, MERGE_REQUEST),
      response(200, [{ id: "d1", notes: [{ id: 1, type: "DiffNote", body: "A finding.\n\n<!-- m -->" }] }]),
      response(200, DIFFS),
      response(201, { id: 42 }),
      response(200, {}),
      response(201, { id: 500 }),
    ]);
    expect(await post(forge)).toMatchObject({ inlinePosted: true });
    expect(paths(calls).slice(4)).toEqual([
      `POST ${MR_PATH}/draft_notes`,
      `PUT ${MR_PATH}/draft_notes/42/publish`,
      `POST ${MR_PATH}/notes`,
    ]);
    expect(JSON.parse(calls[4]!.body as string).note).toBe("Another finding.\n\n<!-- m -->");
  });

  test("a head that moved after the review takes no inline comment", async () => {
    const moved = { ...MERGE_REQUEST, diff_refs: { ...DIFF_REFS, head_sha: "e".repeat(40) } };
    const { forge, calls } = scripted([response(200, []), response(200, moved), response(201, { id: 502 })]);
    expect(await post(forge)).toMatchObject({
      inlinePosted: false,
      summaryMd: "summary with every finding\n\n<!-- m -->",
    });
    expect(paths(calls).at(-1)).toBe(`POST ${MR_PATH}/notes`);
  });

  test("a comment the diff refuses deletes the drafts and posts the full summary", async () => {
    const { forge, calls } = scripted([
      response(200, []),
      response(200, MERGE_REQUEST),
      response(200, []),
      response(200, DIFFS),
      response(201, { id: 41 }),
      response(400, { message: "400 Bad request - Note {:line_code=>[\"can't be blank\"]}" }),
      response(204, {}),
      response(201, { id: 501 }),
    ]);
    expect(await post(forge)).toMatchObject({
      inlinePosted: false,
      summaryMd: "summary with every finding\n\n<!-- m -->",
    });
    expect(paths(calls).slice(6)).toEqual([`DELETE ${MR_PATH}/draft_notes/41`, `POST ${MR_PATH}/notes`]);
  });

  test("a draft that fails for another reason fails the post and leaves no draft", async () => {
    const { forge, calls } = scripted([
      response(200, []),
      response(200, MERGE_REQUEST),
      response(200, []),
      response(200, DIFFS),
      response(201, { id: 41 }),
      response(403, { message: "403 Forbidden" }),
      response(204, {}),
    ]);
    await expect(post(forge)).rejects.toBeInstanceOf(ForgeRequestError);
    expect(paths(calls).at(-1)).toBe(`DELETE ${MR_PATH}/draft_notes/41`);
  });

  test("a list that does not end is an error, not a part of the answer", async () => {
    const full = Array.from({ length: 100 }, (_, id) => ({ id: `d${id}`, notes: [] }));
    const { forge } = scripted(Array.from({ length: 20 }, () => response(200, full)));
    await expect(forge.hasReviewWithMarker(REPO, 7, "<!-- m -->")).rejects.toBeInstanceOf(ForgeRequestError);
  });

  test("finds a posted review, and a status comment, by its marker", async () => {
    // Only the summary says the review is complete: an inline comment with
    // the marker is a review that stopped before its summary.
    const inline = { id: "d2", notes: [{ id: 2, body: "A finding.\n\n<!-- m -->", type: "DiffNote" }] };
    const summary = { id: "d3", notes: [{ id: 3, body: "summary\n\n<!-- m -->", type: null }] };
    const found = scripted([response(200, [inline, summary])]);
    expect(await found.forge.hasReviewWithMarker(REPO, 7, "<!-- m -->")).toBe(true);
    const partial = scripted([response(200, [inline])]);
    expect(await partial.forge.hasReviewWithMarker(REPO, 7, "<!-- m -->")).toBe(false);

    const notes = scripted([response(200, [{ id: 9, body: "x" }, { id: 8, body: "status <!-- k -->" }])]);
    expect(await notes.forge.findCommentWithMarker(REPO, 7, "<!-- k -->")).toMatchObject({ id: "7:8" });
    expect(notes.calls[0]!.path).toBe(`${MR_PATH}/notes?per_page=100&page=1&sort=desc&order_by=created_at`);
    const down = scripted([response(500, {})]);
    expect(await down.forge.findCommentWithMarker(REPO, 7, "<!-- k -->")).toBeNull();
  });

  test("a status comment's id names its merge request", async () => {
    const { forge, calls } = scripted([response(201, { id: 8 }), response(200, { id: 8 })]);
    const id = await forge.typedReview.createComment(REPO, 7, "status");
    expect(id).toBe("7:8");
    await forge.typedReview.updateComment(REPO, id, "new status");
    expect(calls[1]).toMatchObject({ method: "PUT", path: `${MR_PATH}/notes/8` });
    await expect(forge.typedReview.updateComment(REPO, "8", "x")).rejects.toBeInstanceOf(ForgeRequestError);
  });

  test("lists diff comments with their thread root", async () => {
    const { forge } = scripted([
      response(200, [
        {
          id: "d1",
          notes: [
            { id: 1, type: "DiffNote", body: "root", author: { username: "bot" }, position: { new_path: "a.ts" } },
            { id: 2, type: "DiffNote", body: "reply", author: { username: "dev" }, position: { new_path: "a.ts" } },
          ],
        },
        { id: "d2", notes: [{ id: 3, type: null, body: "a plain comment", author: { username: "dev" } }] },
      ]),
    ]);
    expect(await forge.listReviewComments(REPO, 7)).toEqual([
      { id: "1", inReplyToId: null, authorLogin: "bot", body: "root", path: "a.ts" },
      { id: "2", inReplyToId: "1", authorLogin: "dev", body: "reply", path: "a.ts" },
    ]);
  });

  test("reads the project and number from a merge-request URL", () => {
    const { parsePullRequestUrl } = makeGitlabForge();
    expect(parsePullRequestUrl(MERGE_REQUEST.web_url)).toEqual({ repo: REPO, number: 7 });
    expect(parsePullRequestUrl("https://gitlab.example.com/acme/api/-/issues/7")).toBeNull();
    expect(parsePullRequestUrl("https://gitlab.example.com/acme/api/pull/7")).toBeNull();
  });
});

describe("GitLab review variant", () => {
  test("the registry serves the forge and its built-in", () => {
    expect(requireForge("gitlab").provider).toBe("gitlab");
    expect(reviewVariantFor("gitlab")?.key).toBe("gitlab_review");
  });

  test("its definition is valid", () => {
    expect(() => validateDefinition(reviewDefinition(GITLAB_REVIEW))).not.toThrow();
  });
});
