import { createHmac } from "node:crypto";
import { describe, expect, test } from "bun:test";

import type { EnrollmentRow } from "../db/enrollments.ts";
import type { UpsertReviewTargetInput } from "../db/reviews.ts";
import type { DispatchWebhookInput } from "../automations/dispatch.ts";
import type { DispatchReviewInput } from "../workflows/dispatch-review.ts";
import type { ReviewIngressStart } from "../workflows/review-ingress.ts";
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

/** A realistic delivery: GitHub sends the whole pull-request object, and the
 *  route forwards it so the review needs no API call to start (ADR 0100 d11). */
function pullRequestBody(
  action = "opened",
  draft = false,
  pullRequestOverrides: Record<string, unknown> = {},
) {
  return JSON.stringify({
    action,
    number: 100,
    repository: { full_name: enrollment.repo },
    pull_request: {
      id: 2158810101,
      node_id: "PR_kwDOJ1",
      html_url: `https://github.com/${enrollment.repo}/pull/100`,
      title: "Bump quinn-proto from 0.11.14 to 0.11.16",
      user: { login: "dependabot[bot]" },
      state: "open",
      merged: false,
      head: { sha: "head-sha", ref: "dependabot/cargo/quinn-proto-0.11.16" },
      base: { sha: "base-sha", ref: "main" },
      draft,
      additions: 12,
      deletions: 4,
      changed_files: 2,
      updated_at: "2026-07-21T12:34:56Z",
      ...pullRequestOverrides,
    },
  });
}

/** What the route is expected to hand review ingress from that delivery. */
const FORWARDED_PR = {
  providerId: "2158810101",
  url: `https://github.com/${enrollment.repo}/pull/100`,
  providerUpdatedAt: new Date("2026-07-21T12:34:56Z"),
  title: "Bump quinn-proto from 0.11.14 to 0.11.16",
  author: "dependabot[bot]",
  state: "open",
  headBranch: "dependabot/cargo/quinn-proto-0.11.16",
  baseBranch: "main",
  additions: 12,
  deletions: 4,
  changedFiles: 2,
};

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

function app(enrolled = true, enrollmentRow: EnrollmentRow = enrollment) {
  const dispatches: DispatchReviewInput[] = [];
  const ingresses: ReviewIngressStart[] = [];
  const refreshes: UpsertReviewTargetInput[] = [];
  const automationDispatches: DispatchWebhookInput[] = [];
  return {
    dispatches,
    ingresses,
    refreshes,
    automationDispatches,
    app: makeGithubEventsRoute({
      webhookSecret: async () => SECRET,
      mentionHandle: "acme-reviewer",
      enrollments: { get: async () => enrolled ? enrollmentRow : null },
      dispatch: async (input) => {
        dispatches.push(input);
        return {
          enrolled: true,
          activePass: true,
          workflowId: "review:wf",
          reviewId: "review-row-1",
        };
      },
      startIngress: async (input) => {
        ingresses.push(input);
      },
      refreshTarget: async (input) => {
        refreshes.push(input);
        return true;
      },
      automationDispatch: async (input) => {
        automationDispatches.push(input);
      },
      now: () => new Date("2026-07-22T12:00:00Z"),
    }),
  };
}

const manualEnrollment: EnrollmentRow = { ...enrollment, triggerMode: "manual" };

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
    expect(fixture.automationDispatches).toHaveLength(1);
  });

  test("forwards an event ignored by the PR-review classifier to automations", async () => {
    const body = JSON.stringify({
      action: "opened",
      issue: { number: 7, title: "Broken" },
      repository: { full_name: enrollment.repo },
      token: "must-not-persist",
    });
    const fixture = app();
    const res = await fixture.app.request(PATH, {
      method: "POST",
      body,
      headers: headers(body, "issues"),
    });
    expect(res.status).toBe(200);
    expect(fixture.dispatches).toEqual([]);
    expect(fixture.automationDispatches).toEqual([{
      registrationId: "github-app",
      registration: null,
      eventKey: "issues.opened",
      deliveryId: "delivery-1",
      payload: {
        action: "opened",
        issue: { number: 7, title: "Broken" },
        repository: { full_name: enrollment.repo },
      },
      receivedAt: new Date("2026-07-22T12:00:00Z"),
    }]);
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
    expect(fixture.ingresses).toEqual([]);
    // The route still offers the delivery to the existing-only refresh seam;
    // production returns false when this PR has never had a target.
    expect(fixture.refreshes).toHaveLength(1);
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
    expect(fixture.ingresses).toEqual([{
      provider: "github",
      repo: enrollment.repo,
      prNumber: 100,
      trigger: "opened",
      idempotencyKey: "delivery-1",
      headSha: "head-sha",
      baseSha: "base-sha",
      pr: FORWARDED_PR,
    }]);
  });

  test("dispatches a synchronize (push) for auto enrollment", async () => {
    const body = pullRequestBody("synchronize");
    const fixture = app();
    const res = await fixture.app.request(PATH, {
      method: "POST",
      body,
      headers: headers(body, "pull_request"),
    });
    expect(res.status).toBe(200);
    expect(fixture.ingresses).toEqual([{
      provider: "github",
      repo: enrollment.repo,
      prNumber: 100,
      trigger: "synchronize",
      idempotencyKey: "delivery-1",
      headSha: "head-sha",
      baseSha: "base-sha",
      pr: FORWARDED_PR,
    }]);
  });

  test("does NOT auto-review a synchronize (push) under manual enrollment (#802)", async () => {
    const body = pullRequestBody("synchronize");
    const fixture = app(true, manualEnrollment);
    const res = await fixture.app.request(PATH, {
      method: "POST",
      body,
      headers: headers(body, "pull_request"),
    });
    expect(res.status).toBe(200);
    expect(fixture.ingresses).toEqual([]);
    expect(fixture.refreshes).toHaveLength(1);
  });

  test("does NOT auto-review an opened PR under manual enrollment", async () => {
    const body = pullRequestBody("opened");
    const fixture = app(true, manualEnrollment);
    const res = await fixture.app.request(PATH, {
      method: "POST",
      body,
      headers: headers(body, "pull_request"),
    });
    expect(res.status).toBe(200);
    expect(fixture.ingresses).toEqual([]);
    expect(fixture.refreshes).toHaveLength(1);
  });

  test("does NOT auto-review a draft PR even under auto enrollment", async () => {
    const body = pullRequestBody("synchronize", true);
    const fixture = app();
    const res = await fixture.app.request(PATH, {
      method: "POST",
      body,
      headers: headers(body, "pull_request"),
    });
    expect(res.status).toBe(200);
    expect(fixture.ingresses).toEqual([]);
    expect(fixture.refreshes).toHaveLength(1);
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
    expect(fixture.ingresses).toEqual([{
      provider: "github",
      repo: enrollment.repo,
      prNumber: 100,
      trigger: "command",
      idempotencyKey: "delivery-1",
      commentId: "42",
      focus: "focus on auth",
    }]);
  });

  test("closed refreshes an existing target without starting a pass", async () => {
    const body = pullRequestBody("closed", false, { state: "closed" });
    const fixture = app();
    const res = await fixture.app.request(PATH, {
      method: "POST",
      body,
      headers: headers(body, "pull_request"),
    });

    expect(res.status).toBe(200);
    expect(fixture.ingresses).toEqual([]);
    expect(fixture.dispatches).toEqual([]);
    expect(fixture.refreshes).toEqual([{
      provider: "github",
      providerId: "2158810101",
      repo: enrollment.repo,
      number: 100,
      title: "Bump quinn-proto from 0.11.14 to 0.11.16",
      author: "dependabot[bot]",
      state: "closed",
      url: `https://github.com/${enrollment.repo}/pull/100`,
      providerUpdatedAt: new Date("2026-07-21T12:34:56Z"),
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
