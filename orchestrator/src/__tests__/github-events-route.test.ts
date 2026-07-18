import { createHmac } from "node:crypto";
import { describe, expect, test } from "bun:test";

import type { EnrollmentRow } from "../db/enrollments.ts";
import type { DispatchReviewInput } from "../workflows/dispatch-review.ts";
import { makeGithubEventsRoute } from "../routes/github-events.ts";

const SECRET = "github-route-secret";
const PATH = "/api/v1/integrations/github/events";
const enrollment: EnrollmentRow = {
  repo: "openai/engrams",
  triggerMode: "auto",
  autofix: "off",
  profileId: null,
  createdAt: new Date("2026-07-17T00:00:00Z"),
  updatedAt: new Date("2026-07-17T00:00:00Z"),
};

function headers(body: string, event: string) {
  return {
    "content-type": "application/json",
    "x-github-event": event,
    "x-github-delivery": "delivery-1",
    "x-hub-signature-256": `sha256=${createHmac("sha256", SECRET).update(body).digest("hex")}`,
  };
}

function pullRequestBody() {
  return JSON.stringify({
    action: "opened",
    number: 100,
    repository: { full_name: enrollment.repo },
    pull_request: {
      head: { sha: "head-sha" },
      base: { ref: "main" },
      draft: false,
    },
  });
}

function commentBody(
  body: string,
  authorAssociation = "MEMBER",
  senderType = "User",
) {
  return JSON.stringify({
    action: "created",
    repository: { full_name: enrollment.repo },
    issue: { number: 100, pull_request: { url: "https://api.github.test/pr/100" } },
    comment: { id: 42, body, author_association: authorAssociation },
    sender: { type: senderType },
  });
}

function app(enrolled = true) {
  const dispatches: DispatchReviewInput[] = [];
  return {
    dispatches,
    app: makeGithubEventsRoute({
      webhookSecret: async () => SECRET,
      mentionHandle: "acme-reviewer",
      enrollments: { get: async () => enrolled ? enrollment : null },
      dispatch: async (input) => {
        dispatches.push(input);
        return { enrolled: true, workflowId: "review:wf" };
      },
    }),
  };
}

describe("POST /api/v1/integrations/github/events", () => {
  test("rejects an over-cap content-length before signature verification", async () => {
    let secretCalls = 0;
    const route = makeGithubEventsRoute({
      webhookSecret: async () => {
        secretCalls++;
        return SECRET;
      },
    });
    const body = "{}";
    const res = await route.request(PATH, {
      method: "POST",
      body,
      headers: {
        ...headers(body, "ping"),
        "content-length": String(2 * 1024 * 1024 + 1),
      },
    });
    expect(res.status).toBe(413);
    expect(secretCalls).toBe(0);
  });

  test("rejects a bad signature", async () => {
    const body = JSON.stringify({ zen: "hi" });
    const fixture = app();
    const res = await fixture.app.request(PATH, {
      method: "POST",
      body,
      headers: { ...headers(body, "ping"), "x-hub-signature-256": "sha256=bad" },
    });
    expect(res.status).toBe(401);
  });

  test("acks ping", async () => {
    const body = JSON.stringify({ zen: "hi" });
    const fixture = app();
    const res = await fixture.app.request(PATH, {
      method: "POST",
      body,
      headers: headers(body, "ping"),
    });
    expect(res.status).toBe(200);
    expect(fixture.dispatches).toEqual([]);
  });

  test("drops un-enrolled repos", async () => {
    const body = pullRequestBody();
    const fixture = app(false);
    const res = await fixture.app.request(PATH, {
      method: "POST",
      body,
      headers: headers(body, "pull_request"),
    });
    expect(res.status).toBe(200);
    expect(fixture.dispatches).toEqual([]);
  });

  test("dispatches an opened non-draft PR for auto enrollment", async () => {
    const body = pullRequestBody();
    const fixture = app();
    const res = await fixture.app.request(PATH, {
      method: "POST",
      body,
      headers: headers(body, "pull_request"),
    });
    expect(res.status).toBe(200);
    expect(fixture.dispatches).toEqual([{
      repo: enrollment.repo,
      prNumber: 100,
      trigger: "opened",
      idempotencyKey: "delivery-1",
      headSha: "head-sha",
    }]);
  });

  test("dispatches a review command (mentioning the configured App handle) with its focus", async () => {
    const body = commentBody("@acme-reviewer review focus on auth");
    const fixture = app();
    const res = await fixture.app.request(PATH, {
      method: "POST",
      body,
      headers: headers(body, "issue_comment"),
    });
    expect(res.status).toBe(200);
    expect(fixture.dispatches).toEqual([{
      repo: enrollment.repo,
      prNumber: 100,
      trigger: "command",
      idempotencyKey: "delivery-1",
      focus: "focus on auth",
    }]);
  });

  test("drops commands from unauthorized commenters", async () => {
    const body = commentBody("@acme-reviewer review", "NONE");
    const fixture = app();
    const res = await fixture.app.request(PATH, {
      method: "POST",
      body,
      headers: headers(body, "issue_comment"),
    });
    expect(res.status).toBe(200);
    expect(fixture.dispatches).toEqual([]);
  });

  test("drops commands from bot senders", async () => {
    const body = commentBody("@acme-reviewer stop", "MEMBER", "Bot");
    const fixture = app();
    const res = await fixture.app.request(PATH, {
      method: "POST",
      body,
      headers: headers(body, "issue_comment"),
    });
    expect(res.status).toBe(200);
    expect(fixture.dispatches).toEqual([]);
  });

  test("ignores a junk comment", async () => {
    const body = commentBody("looks good");
    const fixture = app();
    const res = await fixture.app.request(PATH, {
      method: "POST",
      body,
      headers: headers(body, "issue_comment"),
    });
    expect(res.status).toBe(200);
    expect(fixture.dispatches).toEqual([]);
  });
});
