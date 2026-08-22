import { beforeEach, describe, expect, test } from "bun:test";

import { invalidateRegistry } from "../../../connectors/registry.ts";
import type { IntegrationOpRequest, IntegrationOpResult } from "../../../integrations/run-op.ts";
import { makeGithubReviewPoster } from "../../../reviews/github-review.ts";
import { actionClientId } from "../client-id.ts";
import { defaultBuiltinActionDeps, type BuiltinActionDeps } from "../builtin.ts";
import { IntegrationActionError } from "../errors.ts";
import { executeIntegrationAction, type ExecuteIntegrationActionDeps } from "../execute.ts";

const emptySource = { list: async () => [] };
const CTX = { runId: "autorun:auto-1:webhook:d1", stepPath: "notify" };

function json(status: number, value: unknown): IntegrationOpResult {
  return {
    status,
    body: new TextEncoder().encode(JSON.stringify(value)),
    contentType: "application/json",
    truncated: false,
  };
}

/** Fake Mode-A op: records calls, answers from a queue. */
function fakeRunOp(responses: IntegrationOpResult[]) {
  const calls: Array<{ provider: string; req: IntegrationOpRequest }> = [];
  const runOp = async (provider: string, req: IntegrationOpRequest): Promise<IntegrationOpResult> => {
    calls.push({ provider, req });
    const next = responses.shift();
    if (!next) throw new Error(`fakeRunOp exhausted (call ${calls.length})`);
    return next;
  };
  return { calls, runOp };
}

function deps(
  runOp: (provider: string, req: IntegrationOpRequest) => Promise<IntegrationOpResult>,
  extra: Partial<ExecuteIntegrationActionDeps> = {},
): ExecuteIntegrationActionDeps {
  return { runOp, connectors: emptySource, ...extra };
}

async function expectActionError(
  promise: Promise<unknown>,
  match: { permanent: boolean; message?: RegExp },
): Promise<void> {
  try {
    await promise;
  } catch (error) {
    expect(error).toBeInstanceOf(IntegrationActionError);
    expect((error as IntegrationActionError).permanent).toBe(match.permanent);
    if (match.message) expect((error as Error).message).toMatch(match.message);
    return;
  }
  throw new Error("expected the action to fail");
}

beforeEach(() => invalidateRegistry());

describe("executeIntegrationAction — http", () => {
  test("renders the path with URI-encoded segments and substitutes the body", async () => {
    const f = fakeRunOp([json(201, { ok: true })]);
    const outputs = await executeIntegrationAction(
      {
        provider: "github",
        actionId: "set_commit_status",
        params: { repo: "acme/repo", sha: "ab c%", state: "success", context: "engrams" },
      },
      CTX,
      deps(f.runOp),
    );
    expect(f.calls).toHaveLength(1);
    const req = f.calls[0]!.req;
    expect(req.method).toBe("POST");
    expect(req.path).toBe("/repos/acme/repo/statuses/ab%20c%25");
    const body = JSON.parse(req.body as string) as Record<string, unknown>;
    expect(body).toEqual({ state: "success", context: "engrams" });
    // No declared output → the status fallback.
    expect(outputs).toEqual({ status: 201 });
  });

  test("maps declared outputs over the parsed response", async () => {
    // update_issue_comment is idempotency natural → no marker scan call.
    const f = fakeRunOp([json(200, { id: 7 })]);
    const outputs = await executeIntegrationAction(
      {
        provider: "github",
        actionId: "update_issue_comment",
        params: { repo: "acme/repo", commentId: 7, body: "edited" },
      },
      CTX,
      deps(f.runOp),
    );
    expect(f.calls).toHaveLength(1);
    expect(f.calls[0]!.req.method).toBe("PATCH");
    expect(outputs).toEqual({ status: 200 });
  });

  test("classifies failures: 404 permanent, 502 transient, 429 transient", async () => {
    const call = (status: number) =>
      executeIntegrationAction(
        {
          provider: "github",
          actionId: "update_issue_comment",
          params: { repo: "acme/repo", commentId: 7, body: "x" },
        },
        CTX,
        deps(fakeRunOp([json(status, {})]).runOp),
      );
    await expectActionError(call(404), { permanent: true });
    await expectActionError(call(502), { permanent: false });
    await expectActionError(call(429), { permanent: false });
  });

  test("marker_comment: appends the marker and short-circuits when it already exists", async () => {
    // Fresh post: scan (empty) then create.
    const fresh = fakeRunOp([json(200, []), json(201, { id: 42 })]);
    const outputs = await executeIntegrationAction(
      {
        provider: "github",
        actionId: "create_issue_comment",
        params: { repo: "acme/repo", number: 5, body: "hello" },
      },
      CTX,
      deps(fresh.runOp),
    );
    expect(fresh.calls).toHaveLength(2);
    expect(fresh.calls[0]!.req.method).toBe("GET");
    const posted = JSON.parse(fresh.calls[1]!.req.body as string) as { body: string };
    expect(posted.body).toContain("hello");
    expect(posted.body).toContain(`<!-- engrams-automation:${CTX.runId}:${CTX.stepPath} -->`);
    expect(outputs).toEqual({ commentId: 42 });

    // Replay: the scan finds the marker; no second create.
    const replay = fakeRunOp([
      json(200, [{ id: 42, body: `hi\n\n<!-- engrams-automation:${CTX.runId}:${CTX.stepPath} -->` }]),
    ]);
    const replayed = await executeIntegrationAction(
      {
        provider: "github",
        actionId: "create_issue_comment",
        params: { repo: "acme/repo", number: 5, body: "hello" },
      },
      CTX,
      deps(replay.runOp),
    );
    expect(replay.calls).toHaveLength(1);
    expect(replayed).toEqual({ commentId: 42 });
  });

  test("caps oversized mapped outputs with a truncation flag", async () => {
    const huge = "x".repeat(300 * 1024);
    const f = fakeRunOp([json(200, []), json(201, { id: huge })]);
    const outputs = await executeIntegrationAction(
      {
        provider: "github",
        actionId: "create_issue_comment",
        params: { repo: "acme/repo", number: 5, body: "hello" },
      },
      CTX,
      deps(f.runOp),
    );
    expect(outputs["truncated"]).toBe(true);
    expect((outputs["commentId"] as string).length).toBeLessThanOrEqual(4096);
  });
});

describe("executeIntegrationAction — graphql", () => {
  test("posts the document with params as variables and a deterministic client id", async () => {
    const f = fakeRunOp([
      json(200, { data: { commentCreate: { success: true, comment: { id: "c1" } } } }),
    ]);
    const outputs = await executeIntegrationAction(
      {
        provider: "linear",
        actionId: "create_comment",
        params: { issueId: "issue-1", body: "ping" },
      },
      CTX,
      deps(f.runOp),
    );
    expect(f.calls[0]!.req.path).toBe("/graphql");
    const body = JSON.parse(f.calls[0]!.req.body as string) as {
      query: string;
      variables: Record<string, unknown>;
    };
    expect(body.query).toContain("commentCreate");
    expect(body.variables).toEqual({
      issueId: "issue-1",
      body: "ping",
      id: actionClientId(CTX.runId, CTX.stepPath),
    });
    expect(outputs).toEqual({ commentId: "c1" });
  });

  test("classifies GraphQL errors: rate limit transient, bad input permanent", async () => {
    const call = (message: string) =>
      executeIntegrationAction(
        { provider: "linear", actionId: "create_comment", params: { issueId: "i", body: "b" } },
        CTX,
        deps(fakeRunOp([json(200, { errors: [{ message }] })]).runOp),
      );
    await expectActionError(call("rate limit exceeded"), { permanent: false });
    await expectActionError(call("issueId must reference an issue"), { permanent: true });
  });
});

describe("executeIntegrationAction — builtins", () => {
  // Built from scratch (never spread from production defaults) so a test can
  // only ever reach the seams it explicitly provides.
  function builtinDeps(
    runOp: (provider: string, req: IntegrationOpRequest) => Promise<IntegrationOpResult>,
    overrides: Partial<BuiltinActionDeps> = {},
  ): BuiltinActionDeps {
    return {
      runOp,
      githubPoster: () => makeGithubReviewPoster({ runIntegrationOp: runOp }),
      slackClient: async () => {
        throw new Error("slack client not provided in this test");
      },
      linearClient: () => {
        throw new Error("linear client not provided in this test");
      },
      ...overrides,
    };
  }

  test("github.post_pr_review rides the real poster incl. the 422 summary-only fallback", async () => {
    const f = fakeRunOp([
      json(200, []), // marker scan
      json(422, { message: "was submitted too quickly" }), // inline attempt
      json(200, { id: 99 }), // summary-only fallback
    ]);
    const outputs = await executeIntegrationAction(
      {
        provider: "github",
        actionId: "post_pr_review",
        params: {
          repo: "acme/repo",
          prNumber: 12,
          commitId: "abc123",
          summary: "Looks fine.",
          comments: [{ path: "src/a.ts", line: 3, body: "nit" }],
        },
      },
      CTX,
      deps(f.runOp, { builtinDeps: builtinDeps(f.runOp) }),
    );
    expect(f.calls).toHaveLength(3);
    const fallback = JSON.parse(f.calls[2]!.req.body as string) as Record<string, unknown>;
    expect(fallback["comments"]).toBeUndefined();
    expect(String(fallback["body"])).toContain(`<!-- engrams-automation:${CTX.runId}:${CTX.stepPath} -->`);
    expect(outputs).toMatchObject({ posted: true, inline_posted: false, github_review_id: "99" });
  });

  test("github.post_pr_review short-circuits when the marker is already on a review", async () => {
    const marker = `<!-- engrams-automation:${CTX.runId}:${CTX.stepPath} -->`;
    const f = fakeRunOp([json(200, [{ body: `Looks fine.\n\n${marker}` }])]);
    const outputs = await executeIntegrationAction(
      {
        provider: "github",
        actionId: "post_pr_review",
        params: { repo: "acme/repo", prNumber: 12, summary: "Looks fine." },
      },
      CTX,
      deps(f.runOp, { builtinDeps: builtinDeps(f.runOp) }),
    );
    expect(f.calls).toHaveLength(1);
    expect(outputs).toMatchObject({ posted: false, already_posted: true });
  });

  test("slack.post_message maps ts/channel through the declared outputs", async () => {
    const sent: Array<Record<string, unknown>> = [];
    const f = fakeRunOp([]);
    const outputs = await executeIntegrationAction(
      {
        provider: "slack",
        actionId: "post_message",
        params: { channel: "C1", text: "hi", threadTs: "1.1" },
      },
      CTX,
      deps(f.runOp, {
        builtinDeps: builtinDeps(f.runOp, {
          slackClient: async () => ({
            chat: {
              postMessage: async (args) => {
                sent.push(args);
                return { ts: "1.2", channel: "C1" };
              },
              update: async () => ({}),
            },
          }),
        }),
      }),
    );
    expect(sent[0]).toEqual({ channel: "C1", text: "hi", thread_ts: "1.1" });
    expect(outputs).toEqual({ ts: "1.2", channel: "C1" });
  });

  test("linear.create_issue mints the deterministic client id", async () => {
    const created: Array<Record<string, unknown>> = [];
    const f = fakeRunOp([]);
    const outputs = await executeIntegrationAction(
      {
        provider: "linear",
        actionId: "create_issue",
        params: { teamId: "T1", title: "Fix it", description: "Body" },
      },
      CTX,
      deps(f.runOp, {
        builtinDeps: builtinDeps(f.runOp, {
          linearClient: () => ({
            findIssue: async () => null,
            createIssue: async (input) => {
              created.push(input as unknown as Record<string, unknown>);
              return { id: input.id, identifier: "ENG-1", url: "https://linear.app/i/ENG-1" };
            },
            createBlockingRelation: async () => {},
            readWorkspace: async () => ({ teams: [], projects: [], labels: [] }),
          }),
        }),
      }),
    );
    expect(created[0]!["id"]).toBe(actionClientId(CTX.runId, CTX.stepPath));
    expect(outputs).toEqual({
      issueId: actionClientId(CTX.runId, CTX.stepPath),
      identifier: "ENG-1",
      url: "https://linear.app/i/ENG-1",
    });
  });
});

describe("executeIntegrationAction — validation and lookup", () => {
  test("rejects unknown providers, unknown actions, bad and unknown params as permanent", async () => {
    const f = fakeRunOp([]);
    await expectActionError(
      executeIntegrationAction({ provider: "nope", actionId: "x", params: {} }, CTX, deps(f.runOp)),
      { permanent: true, message: /unknown provider/ },
    );
    await expectActionError(
      executeIntegrationAction({ provider: "github", actionId: "zap", params: {} }, CTX, deps(f.runOp)),
      { permanent: true, message: /no action/ },
    );
    await expectActionError(
      executeIntegrationAction(
        { provider: "github", actionId: "set_commit_status", params: { repo: "a/b" } },
        CTX,
        deps(f.runOp),
      ),
      { permanent: true, message: /invalid params/ },
    );
    await expectActionError(
      executeIntegrationAction(
        {
          provider: "github",
          actionId: "set_commit_status",
          params: { repo: "a/b", sha: "s", state: "success", context: "c", extra: true },
        },
        CTX,
        deps(f.runOp),
      ),
      { permanent: true, message: /unknown property/ },
    );
    expect(f.calls).toHaveLength(0);
  });

  test("rejects path traversal through whole-segment params", async () => {
    await expectActionError(
      executeIntegrationAction(
        {
          provider: "github",
          actionId: "set_commit_status",
          params: { repo: "acme/../secrets", sha: "s", state: "success", context: "c" },
        },
        CTX,
        deps(fakeRunOp([]).runOp),
      ),
      { permanent: true, message: /invalid segment/ },
    );
  });
});
