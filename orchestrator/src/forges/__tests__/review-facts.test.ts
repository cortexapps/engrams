/** Admission of a forge event to the code-review built-in, for a forge that
 * is not GitHub. */

import { describe, expect, test } from "bun:test";

import type { ReviewEvent } from "../forge.ts";
import { enrolledEntry, reviewFacts } from "../review-facts.ts";
import { eventPayload, fakeComment, fakePullRequest, makeFakeForge } from "./fake-forge.ts";

const INPUTS = {
  // The event names the repository `Acme/App.git`; the forge's key matches it.
  repos: { "acme/app": { mode: "auto" }, "acme/manual": { mode: "on_request" } },
  profile: "p1",
  mention: "@engrams",
  categories: ["security-privacy"],
  instructions: "Be terse.",
};

function facts(event: ReviewEvent, inputs: Record<string, unknown> = INPUTS) {
  const { forge, state } = makeFakeForge();
  return { state, value: reviewFacts(forge, { eventKey: "any", payload: eventPayload(event), inputs }) };
}

const change = (overrides: Partial<Extract<ReviewEvent, { kind: "change" }>> = {}): ReviewEvent => ({
  kind: "change",
  trigger: "opened",
  draft: false,
  pr: fakePullRequest(),
  ...overrides,
});

describe("reviewFacts", () => {
  test("an enrolled pull request in auto mode is admitted with its facts", async () => {
    expect(await facts(change()).value).toEqual({
      admit: true,
      repo: "Acme/App.git",
      repo_name: "App.git",
      pr_number: 7,
      pr_url: "https://forge.test/acme/app/7",
      trigger: "opened",
      head_sha: "h".repeat(40),
      base_sha: "b".repeat(40),
      pr_context: expect.objectContaining({ providerId: "pr-7", providerUpdatedAt: null }),
      categories: ["security-privacy"],
      instructions: "Be terse.",
    });
  });

  test("the enrolment match uses the forge's repository key", () => {
    const { forge } = makeFakeForge();
    expect(enrolledEntry(forge, INPUTS.repos, "ACME/app.git")).toEqual({ mode: "auto" });
    expect(enrolledEntry(forge, INPUTS.repos, "acme/other")).toBeNull();
    expect(enrolledEntry(forge, ["acme/app"], "acme/app")).toBeNull();
  });

  test("a repository that is not enrolled starts nothing", async () => {
    expect(await facts(change({ pr: fakePullRequest({ repo: "acme/other" }) })).value).toBeNull();
  });

  test("a draft, a repository not in auto mode, and no profile start nothing", async () => {
    expect(await facts(change({ draft: true })).value).toBeNull();
    expect(await facts(change({ pr: fakePullRequest({ repo: "acme/manual" }) })).value).toBeNull();
    expect(await facts(change(), { ...INPUTS, profile: "" }).value).toBeNull();
  });

  test("a review command from a bot starts nothing and asks the forge nothing", async () => {
    const { value, state } = facts(fakeComment("@engrams review", { authorId: "maintainer", fromBot: true }));
    expect(await value).toBeNull();
    expect(state.asked).toEqual([]);
  });

  test("a review command needs a person the forge accepts", async () => {
    const outsider = facts(fakeComment("@engrams review", { authorId: "stranger" }));
    expect(await outsider.value).toBeNull();
    expect(outsider.state.asked).toEqual(["stranger"]);

    // Commands need no auto mode.
    const member = facts(
      fakeComment("@engrams review", { authorId: "maintainer", pr: fakePullRequest({ repo: "acme/manual" }) }),
    );
    expect(await member.value).toMatchObject({ admit: true, trigger: "command", repo: "acme/manual" });
  });

  test("a comment that is not a review command asks the forge nothing", async () => {
    for (const body of ["looks good", "@engrams stop", "@other review"]) {
      const { value, state } = facts(fakeComment(body, { authorId: "maintainer" }));
      expect(await value).toBeNull();
      expect(state.asked).toEqual([]);
    }
  });

  test("a dispatch is admitted without auto mode", async () => {
    const event: ReviewEvent = { kind: "dispatch", pr: fakePullRequest({ repo: "acme/manual" }) };
    expect(await facts(event).value).toMatchObject({ admit: true, trigger: "dispatch" });
  });

  test("an update starts nothing, and a payload that is no review event starts nothing", async () => {
    expect(await facts({ kind: "update", pr: fakePullRequest() }).value).toBeNull();
    const { forge } = makeFakeForge();
    expect(await reviewFacts(forge, { eventKey: "push", payload: {}, inputs: INPUTS })).toBeNull();
  });
});
