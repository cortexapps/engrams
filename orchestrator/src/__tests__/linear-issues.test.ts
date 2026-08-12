/**
 * The Linear calls the ticket sync makes.
 *
 * Nothing here reaches Linear: the transport is the `runIntegrationOp` seam,
 * faked. What is worth asserting is the classification, because the ledger
 * shows the class to a person — "401, reconnect" and "the team id is wrong"
 * are different problems with different buttons.
 */
import { describe, expect, test } from "bun:test";

import {
  LinearError,
  makeLinearIssueClient,
  type RunIntegrationOp,
} from "../integrations/linear-issues.ts";

interface Call {
  provider: string;
  query: string;
  variables: Record<string, unknown>;
}

function fakeRunOp(
  responses: ReadonlyArray<{ status: number; body: unknown }>,
): { run: RunIntegrationOp; calls: Call[] } {
  const queue = [...responses];
  const calls: Call[] = [];
  const run = (async (provider, request) => {
    const parsed = JSON.parse(String(request.body)) as {
      query: string;
      variables: Record<string, unknown>;
    };
    calls.push({ provider, query: parsed.query, variables: parsed.variables });
    const next = queue.shift();
    if (!next) throw new Error("no response queued");
    return {
      status: next.status,
      body: new TextEncoder().encode(JSON.stringify(next.body)),
      contentType: "application/json",
      truncated: false,
    };
  }) as RunIntegrationOp;
  return { run, calls };
}

const ISSUE = {
  id: "3f2a0b1c-1111-4111-8111-111111111111",
  identifier: "ENG-412",
  url: "https://linear.app/acme/issue/ENG-412",
};

describe("the Linear issue client", () => {
  test("creates an issue at the id the caller reserved", async () => {
    const fake = fakeRunOp([{ status: 200, body: { data: { issueCreate: { success: true, issue: ISSUE } } } }]);
    const client = makeLinearIssueClient({ runIntegrationOp: fake.run });

    const issue = await client.createIssue({
      id: ISSUE.id,
      teamId: "team-platform",
      title: "Add org quota columns",
      description: "Add the columns.",
      labelIds: ["label-spec-mode"],
    });

    expect(issue).toEqual(ISSUE);
    expect(fake.calls[0]?.provider).toBe("linear");
    const input = fake.calls[0]?.variables["input"] as Record<string, unknown>;
    // The reserved id is the whole of N4: it must reach Linear.
    expect(input["id"]).toBe(ISSUE.id);
    expect(input["teamId"]).toBe("team-platform");
    expect(input["labelIds"]).toEqual(["label-spec-mode"]);
    // An absent project is absent, not null — Linear rejects a null project.
    expect(input).not.toHaveProperty("projectId");
  });

  test("an expired token is reported as the reconnect case", async () => {
    const fake = fakeRunOp([{ status: 401, body: { errors: [{ message: "Authentication required" }] } }]);
    const client = makeLinearIssueClient({ runIntegrationOp: fake.run });

    const failure = await client
      .createIssue({ id: ISSUE.id, teamId: "team", title: "t", description: "d" })
      .catch((error: unknown) => error);

    expect(failure).toBeInstanceOf(LinearError);
    expect((failure as LinearError).code).toBe("unauthenticated");
    expect((failure as LinearError).status).toBe(401);
    expect((failure as LinearError).retryable).toBe(false);
  });

  test("a 200 with a GraphQL error is still a failure", async () => {
    const fake = fakeRunOp([
      {
        status: 200,
        body: {
          errors: [
            { message: "Argument Validation Error", extensions: { type: "invalid input" } },
          ],
        },
      },
    ]);
    const client = makeLinearIssueClient({ runIntegrationOp: fake.run });

    const failure = await client
      .createIssue({ id: ISSUE.id, teamId: "team", title: "t", description: "d" })
      .catch((error: unknown) => error);

    expect(failure).toBeInstanceOf(LinearError);
    expect((failure as LinearError).code).toBe("invalid_request");
  });

  test("rate limiting is retryable, and a refusal is not", async () => {
    const limited = fakeRunOp([{ status: 429, body: { errors: [{ message: "slow down" }] } }]);
    const failure = await makeLinearIssueClient({ runIntegrationOp: limited.run })
      .createIssue({ id: ISSUE.id, teamId: "team", title: "t", description: "d" })
      .catch((error: unknown) => error);
    expect((failure as LinearError).code).toBe("rate_limited");
    expect((failure as LinearError).retryable).toBe(true);
  });

  test("an unknown issue id reads as absent, not as an error", async () => {
    const fake = fakeRunOp([
      {
        status: 200,
        body: { errors: [{ message: "Entity not found: Issue", extensions: { type: "invalid input" } }] },
      },
    ]);
    const client = makeLinearIssueClient({ runIntegrationOp: fake.run });

    expect(await client.findIssue(ISSUE.id)).toBeNull();
  });

  test("an existing issue is found, which is how a retry adopts", async () => {
    const fake = fakeRunOp([{ status: 200, body: { data: { issue: ISSUE } } }]);
    const client = makeLinearIssueClient({ runIntegrationOp: fake.run });

    expect(await client.findIssue(ISSUE.id)).toEqual(ISSUE);
    expect(fake.calls[0]?.variables["id"]).toBe(ISSUE.id);
  });

  test("a blocking relation names the blocker and the blocked", async () => {
    const fake = fakeRunOp([{ status: 200, body: { data: { issueRelationCreate: { success: true } } } }]);
    const client = makeLinearIssueClient({ runIntegrationOp: fake.run });

    await client.createBlockingRelation({
      id: "relation-1",
      blockerIssueId: "issue-a",
      blockedIssueId: "issue-b",
    });

    const input = fake.calls[0]?.variables["input"] as Record<string, unknown>;
    expect(input).toEqual({
      id: "relation-1",
      issueId: "issue-a",
      relatedIssueId: "issue-b",
      type: "blocks",
    });
  });

  test("a coordinator that cannot make the call reads as not connected", async () => {
    const run = (async () => {
      throw new Error('unknown connector "linear"');
    }) as RunIntegrationOp;
    const failure = await makeLinearIssueClient({ runIntegrationOp: run })
      .readWorkspace()
      .catch((error: unknown) => error);

    expect(failure).toBeInstanceOf(LinearError);
    expect((failure as LinearError).code).toBe("unauthenticated");
    expect((failure as Error).message).toContain("unknown connector");
  });

  test("the workspace picker reads the three collections", async () => {
    const fake = fakeRunOp([
      {
        status: 200,
        body: {
          data: {
            teams: { nodes: [{ id: "team-platform", name: "Platform" }] },
            projects: { nodes: [{ id: "project-quota", name: "Quota & billing" }] },
            issueLabels: { nodes: [{ id: "label-spec", name: "spec-mode" }, { id: "bad" }] },
          },
        },
      },
    ]);

    const workspace = await makeLinearIssueClient({ runIntegrationOp: fake.run }).readWorkspace();

    expect(workspace.teams).toEqual([{ id: "team-platform", name: "Platform" }]);
    expect(workspace.projects).toEqual([{ id: "project-quota", name: "Quota & billing" }]);
    // A node without a name is dropped rather than rendered as "undefined".
    expect(workspace.labels).toEqual([{ id: "label-spec", name: "spec-mode" }]);
  });
});
