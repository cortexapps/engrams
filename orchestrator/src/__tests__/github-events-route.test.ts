import { createHmac } from "node:crypto";
import { describe, expect, test } from "bun:test";

import type { EnrollmentRow } from "../db/enrollments.ts";
import type { UpsertReviewTargetInput } from "../db/reviews.ts";
import { dispatchIntegrationEvent, type DispatchWebhookInput } from "../automations/dispatch.ts";
import type { IntegrationEventDispatchInput } from "../automations/integration-ingress.ts";
import type {
  IntegrationEventStore,
  RecordIntegrationEventInput,
} from "../db/integration-events.ts";
import type { DispatchReviewInput } from "../workflows/dispatch-review.ts";
import type { ReviewIngressStart } from "../workflows/review-ingress.ts";
import { makeGithubEventsRoute } from "../routes/github-events.ts";

const SECRET = "github-route-secret";
const PATH = "/api/v1/integrations/github/events";
const enrollment: EnrollmentRow = {
  repo: "openai/engrams",
  triggerMode: "auto",
  engine: "legacy" as const,
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

/** In-memory ingress-spine seams: the ledger store, connection resolution,
 * and the 2.C dispatch — no database. */
export function fakeIngress() {
  const recorded: RecordIntegrationEventInput[] = [];
  const dispatched: IntegrationEventDispatchInput[] = [];
  const store: IntegrationEventStore = {
    async record(input) {
      const duplicate = recorded.some(
        (e) =>
          e.provider === input.provider &&
          e.connectionId === input.connectionId &&
          e.deliveryId === input.deliveryId,
      );
      if (!duplicate) recorded.push(input);
      return { recorded: !duplicate };
    },
    async sweep() {
      return 0;
    },
    async list() {
      return [];
    },
    async getLatest() {
      return null;
    },
    async listObservedEventKeys() {
      return [];
    },
    async getById() {
      return null;
    },
    async listObservedScopeValues() {
      return [];
    },
  };
  return {
    recorded,
    dispatched,
    deps: {
      store,
      connectionIdFor: async (provider: string) => `conn-${provider}`,
      dispatch: async (input: IntegrationEventDispatchInput) => {
        dispatched.push(input);
      },
    },
  };
}

function app(enrolled = true, enrollmentRow: EnrollmentRow = enrollment, reviewAutomationDisabled = false) {
  const dispatches: DispatchReviewInput[] = [];
  const ingresses: ReviewIngressStart[] = [];
  const refreshes: UpsertReviewTargetInput[] = [];
  const ingress = fakeIngress();
  return {
    dispatches,
    ingresses,
    refreshes,
    ledger: ingress.recorded,
    integrationDispatches: ingress.dispatched,
    app: makeGithubEventsRoute({
      ingress: ingress.deps,
      webhookSecret: async () => SECRET,
      mentionHandle: "acme-reviewer",
      reviewAutomationDisabled,
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
      now: () => new Date("2026-07-22T12:00:00Z"),
    }),
  };
}

const autoAutomationEnrollment: EnrollmentRow = { ...enrollment, engine: "automation" };

describe("the automation-engine route branch (ADR 0119 phase 4.4)", () => {
  test("an automation-engine repo skips legacy ingress, still refreshes the target, and the spine dispatched", async () => {
    const body = pullRequestBody("opened", false);
    const fixture = app(true, autoAutomationEnrollment);
    const res = await fixture.app.request(PATH, {
      method: "POST",
      body,
      headers: headers(body, "pull_request"),
    });
    expect(res.status).toBe(200);
    expect(fixture.ingresses).toEqual([]); // no legacy ingress
    expect(fixture.dispatches).toEqual([]);
    expect(fixture.refreshes).toHaveLength(1); // dossier target kept fresh
    expect(fixture.integrationDispatches).toHaveLength(1); // spine → built-in
  });

  test("the kill switch forces an automation repo back onto legacy ingress", async () => {
    const body = pullRequestBody("opened", false);
    const fixture = app(true, autoAutomationEnrollment, /* reviewAutomationDisabled */ true);
    const res = await fixture.app.request(PATH, {
      method: "POST",
      body,
      headers: headers(body, "pull_request"),
    });
    expect(res.status).toBe(200);
    expect(fixture.ingresses).toHaveLength(1); // legacy ingress ran
    expect(fixture.integrationDispatches).toHaveLength(1); // spine always ledgers+dispatches
  });

  test("a legacy repo is unchanged: legacy ingress starts", async () => {
    const body = pullRequestBody("opened", false);
    const fixture = app(true, enrollment); // engine "legacy"
    await fixture.app.request(PATH, {
      method: "POST",
      body,
      headers: headers(body, "pull_request"),
    });
    expect(fixture.ingresses).toHaveLength(1);
  });

  test("an @mention review on an automation repo skips legacy ingress", async () => {
    const body = commentBody("@acme-reviewer review", "MEMBER", "User");
    const fixture = app(true, autoAutomationEnrollment);
    const res = await fixture.app.request(PATH, {
      method: "POST",
      body,
      headers: headers(body, "issue_comment"),
    });
    expect(res.status).toBe(200);
    expect(fixture.ingresses).toEqual([]);
    expect(fixture.integrationDispatches).toHaveLength(1);
  });

  test("an @mention review on a legacy repo still starts legacy ingress", async () => {
    const body = commentBody("@acme-reviewer review", "MEMBER", "User");
    const fixture = app(true, enrollment);
    await fixture.app.request(PATH, {
      method: "POST",
      body,
      headers: headers(body, "issue_comment"),
    });
    expect(fixture.ingresses).toHaveLength(1);
  });
});

const manualEnrollment: EnrollmentRow = { ...enrollment, triggerMode: "manual" };

describe("POST /api/v1/integrations/github/events", () => {
  test("rejects an over-cap content-length before signature verification", async () => {
    let secretCalls = 0;
    const route = makeGithubEventsRoute({
      webhookSecret: async () => {
        secretCalls++;
        return SECRET;
      },
      ingress: fakeIngress().deps,
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

  test("refuses a signed delivery that carries no delivery id", async () => {
    // X-GitHub-Delivery is part of GitHub's webhook contract, so a signed request
    // without one is malformed. It must not be given a derived key either: an
    // empty key reaches DBOS.send as a real message id, and its notifications
    // table conflicts on that id ALONE, so the first empty-key send would
    // silently swallow every later one for every review in the system.
    const body = pullRequestBody("opened", false);
    const fixture = app();
    const { "x-github-delivery": _omitted, ...withoutDelivery } = headers(
      body,
      "pull_request",
    );
    const res = await fixture.app.request(PATH, {
      method: "POST",
      body,
      headers: withoutDelivery,
    });

    expect(res.status).toBe(400);
    expect(fixture.ingresses).toEqual([]);
    expect(fixture.dispatches).toEqual([]);
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
    // Even a ping is a verified delivery: the spine ledgers it.
    expect(fixture.integrationDispatches).toHaveLength(1);
  });

  test("forwards an event ignored by the PR-review classifier to integration triggers", async () => {
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
    // The retired github-app system registration no longer exists; the
    // 2.C integration-trigger seam is the only automation path.
    expect(fixture.integrationDispatches).toEqual([
      expect.objectContaining({
        provider: "github",
        eventKey: "issues.opened",
        deliveryId: "delivery-1",
        payload: {
          action: "opened",
          issue: { number: 7, title: "Broken" },
          repository: { full_name: enrollment.repo },
        },
      }),
    ]);
  });

  test("every verified delivery lands in the integration-event ledger and the trigger seam", async () => {
    const body = JSON.stringify({
      action: "opened",
      issue: { number: 7, title: "Broken" },
      repository: { full_name: "OpenAI/Engrams" },
      token: "must-not-persist",
    });
    const fixture = app();
    const res = await fixture.app.request(PATH, {
      method: "POST",
      body,
      headers: headers(body, "issues"),
    });
    expect(res.status).toBe(200);
    // The spine ledgered the redacted payload with the lowercased repo scope…
    expect(fixture.ledger).toEqual([
      expect.objectContaining({
        provider: "github",
        connectionId: "conn-github",
        eventKey: "issues.opened",
        deliveryId: "delivery-1",
        scopeValue: "openai/engrams",
      }),
    ]);
    expect(fixture.ledger[0]!.payload["token"]).toBeUndefined();
    // …and dispatched to the trigger seam exactly once.
    expect(fixture.integrationDispatches).toHaveLength(1);
  });

  test("a dispatch fault fails the delivery (500) so the provider retries; the ledger row stays", async () => {
    const body = JSON.stringify({
      action: "opened",
      issue: { number: 1 },
      repository: { full_name: "acme/repo" },
    });
    const ingress = fakeIngress();
    const route = makeGithubEventsRoute({
      webhookSecret: async () => SECRET,
      ingress: {
        ...ingress.deps,
        dispatch: async () => {
          throw new Error("dbos unavailable");
        },
      },
      enrollments: { get: async () => null },
    });
    const res = await route.request(PATH, {
      method: "POST",
      body,
      headers: headers(body, "issues"),
    });
    // Never a 200: GitHub only redelivers on a non-2xx, and nothing else
    // re-drives a ledgered row. The retry dedupes on the delivery id.
    expect(res.status).toBe(500);
    expect(ingress.recorded).toHaveLength(1);
  });

  test("the PR-review classifier still runs on a spine-ledgered delivery", async () => {
    const body = pullRequestBody();
    const fixture = app();
    const res = await fixture.app.request(PATH, {
      method: "POST",
      body,
      headers: headers(body, "pull_request"),
    });
    expect(res.status).toBe(200);
    expect(fixture.ledger).toHaveLength(1);
    expect(fixture.ingresses).toHaveLength(1);
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

describe("ingress → integration-trigger dispatch (2.C)", () => {
  function integrationApp(triggerScope?: { values: string[] } | { fromInput: string }) {
    const ingress = fakeIngress();
    const starts: string[] = [];
    const target = {
      automation: {
        id: "auto-int-1",
        name: "PR triage",
        description: "",
        enabled: true,
        kind: "user",
        builtinKey: null,
        currentVersion: 1,
        inputs: { repos: { [enrollment.repo]: { mode: "auto" } } },
        blockOverrides: {},
        endSessionsOnFinish: false,
        createdByUserId: null,
        nextFireAt: null,
        lastFiredAt: null,
        createdAt: new Date(0),
        updatedAt: new Date(0),
        archivedAt: null,
      },
      definition: {
        engine: 1 as const,
        trigger: {
          kind: "integration" as const,
          provider: "github",
          connectionId: "conn-github",
          eventKeys: ["pull_request.opened"],
          ...(triggerScope ? { scope: triggerScope } : {}),
        },
        blocks: [],
        inputsSchema: [],
        settings: { endSessionsOnFinish: false },
      },
    };
    const dispatchDeps = {
      store: {
        async listEnabledForIntegrationTrigger() {
          return [target];
        },
        async insertRun(input: { id: string }) {
          return { id: input.id } as never;
        },
        async claimConcurrency() {
          return { claimed: true } as const;
        },
        async casConcurrency() {
          return true;
        },
      },
      workflowStarter: {
        async start(_wf: unknown, workflowId: string) {
          starts.push(workflowId);
        },
      },
      sender: { async send() {} },
    };
    const route = makeGithubEventsRoute({
      ingress: {
        store: ingress.deps.store,
        connectionIdFor: async () => "conn-github",
        dispatch: (input) => dispatchIntegrationEvent(input, dispatchDeps),
      },
      webhookSecret: async () => SECRET,
      mentionHandle: "acme-reviewer",
      enrollments: { get: async () => null },
      dispatch: async () => ({
        enrolled: false,
        activePass: false,
        workflowId: "",
        reviewId: "",
      }),
      startIngress: async () => {},
      refreshTarget: async () => true,
      now: () => new Date("2026-08-21T12:00:00Z"),
    });
    return { route, ledger: ingress.recorded, starts };
  }

  test("a signed delivery lands in the ledger and starts a matching run", async () => {
    const fixture = integrationApp({ fromInput: "repos" });
    const body = pullRequestBody();
    const res = await fixture.route.request(PATH, {
      method: "POST",
      body,
      headers: headers(body, "pull_request"),
    });
    expect(res.status).toBe(200);
    expect(fixture.ledger).toHaveLength(1);
    expect(fixture.ledger[0]).toMatchObject({
      provider: "github",
      eventKey: "pull_request.opened",
      scopeValue: enrollment.repo,
    });
    expect(fixture.starts).toEqual(["autorun:auto-int-1:github:delivery-1"]);
  });

  test("a delivery outside the trigger's scope is ledgered but starts nothing", async () => {
    const fixture = integrationApp({ values: ["someone/else"] });
    const body = pullRequestBody();
    const res = await fixture.route.request(PATH, {
      method: "POST",
      body,
      headers: headers(body, "pull_request"),
    });
    expect(res.status).toBe(200);
    expect(fixture.ledger).toHaveLength(1);
    expect(fixture.starts).toEqual([]);
  });
});
