/** What a verified forge delivery does besides a review: the `stop` command
 * and the refresh of a reviewed pull request's record. */

import { describe, expect, test } from "bun:test";

import type { UpsertReviewTargetInput } from "../../db/reviews.ts";
import type { ReviewEvent } from "../../forges/forge.ts";
import {
  eventPayload,
  FAKE_PROVIDER,
  fakeComment,
  fakePullRequest,
  makeFakeForge,
} from "../../forges/__tests__/fake-forge.ts";
import type { ReviewCoordinate } from "../automation-review.ts";
import { makeForgeDeliveryHandler } from "../forge-deliveries.ts";

function fixture() {
  const stops: ReviewCoordinate[] = [];
  const refreshes: UpsertReviewTargetInput[] = [];
  const handle = makeForgeDeliveryHandler({
    stopAutomationReview: async (input) => {
      stops.push(input);
      return { stopped: true, runId: "autorun:a:fakeforge:d1" };
    },
    refreshTarget: async (input) => {
      refreshes.push(input);
      return true;
    },
    mentionHandle: async () => "@engrams",
  });
  const { forge, state } = makeFakeForge();
  const deliver = (event: ReviewEvent | null) =>
    handle(forge, "any", event === null ? {} : eventPayload(event));
  return { stops, refreshes, state, deliver };
}

describe("forge deliveries", () => {
  test("a stop command from a person the forge accepts stops the review", async () => {
    const f = fixture();
    await f.deliver(fakeComment("@engrams stop", { authorId: "maintainer" }));
    expect(f.stops).toEqual([{ provider: FAKE_PROVIDER, repo: "Acme/App.git", prNumber: 7 }]);
    expect(f.refreshes).toEqual([]);
  });

  test("a stop command from a bot or from a person the forge rejects stops nothing", async () => {
    const f = fixture();
    await f.deliver(fakeComment("@engrams stop", { authorId: "maintainer", fromBot: true }));
    await f.deliver(fakeComment("@engrams stop", { authorId: "stranger" }));
    expect(f.stops).toEqual([]);
  });

  test("a review command and other comments stop nothing", async () => {
    const f = fixture();
    await f.deliver(fakeComment("@engrams review", { authorId: "maintainer" }));
    await f.deliver(fakeComment("@engrams fix the test", { authorId: "maintainer" }));
    await f.deliver(fakeComment("looks good", { authorId: "maintainer" }));
    expect(f.stops).toEqual([]);
    expect(f.refreshes).toEqual([]);
  });

  test("a change and an update refresh the pull request's record", async () => {
    const f = fixture();
    await f.deliver({ kind: "change", trigger: "synchronize", draft: false, pr: fakePullRequest() });
    const closed = fakePullRequest();
    await f.deliver({ kind: "update", pr: { ...closed, context: { ...closed.context, state: "closed" } } });
    expect(f.refreshes).toEqual([
      {
        provider: FAKE_PROVIDER,
        providerId: "pr-7",
        repo: "Acme/App.git",
        number: 7,
        title: "Fix",
        author: null,
        state: "open",
        url: null,
        providerUpdatedAt: null,
      },
      expect.objectContaining({ providerId: "pr-7", state: "closed" }),
    ]);
    expect(f.stops).toEqual([]);
  });

  test("a pull request with no id refreshes nothing", async () => {
    const f = fixture();
    const pr = fakePullRequest();
    await f.deliver({ kind: "update", pr: { ...pr, context: { ...pr.context, providerId: null } } });
    expect(f.refreshes).toEqual([]);
  });

  test("a dispatch and a payload that is no review event do nothing", async () => {
    const f = fixture();
    await f.deliver({ kind: "dispatch", pr: fakePullRequest() });
    await f.deliver(null);
    expect(f.refreshes).toEqual([]);
    expect(f.stops).toEqual([]);
    expect(f.state.asked).toEqual([]);
  });
});
