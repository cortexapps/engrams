import { describe, expect, test } from "bun:test";
import pino from "pino";

import type {
  ReviewTargetHydrationStore,
  UnhydratedReviewTarget,
} from "../../db/review-target-hydration.ts";
import { GithubRequestError } from "../github-review.ts";
import type { PrContext } from "../pr-context.ts";
import {
  runTargetHydration,
  type TargetHydrationReader,
} from "../target-hydrator.ts";

const NOW = new Date("2026-07-27T12:00:00Z");
const log = pino({ enabled: false });

function target(id: string, repo = `acme/${id}`): UnhydratedReviewTarget {
  return { id, provider: "github", repo, number: 7 };
}

function pr(providerId: string | null): PrContext {
  return {
    providerId,
    title: "Review me",
    author: "octocat",
    state: "open",
    url: "https://github.test/acme/repo/pull/7",
    providerUpdatedAt: NOW,
    headBranch: "feature",
    baseBranch: "main",
    additions: 1,
    deletions: 0,
    changedFiles: 1,
  };
}

describe("review target hydrator", () => {
  test("hydrates successes, tolerates a concurrent claim, and isolates row failures", async () => {
    const rows = [
      target("success"),
      target("claimed"),
      target("gone"),
      target("rate-limited"),
      target("server-error"),
    ];
    const hydrated: Array<{
      id: string;
      providerId: string;
      title: string | null;
      at: Date;
    }> = [];
    const failed: Array<{ id: string; at: Date }> = [];
    const store: ReviewTargetHydrationStore = {
      async listUnhydrated(limit) {
        expect(limit).toBe(5);
        return rows;
      },
      async hydrate(id, providerId, pr, at) {
        hydrated.push({ id, providerId, title: pr.title, at });
        return id === "claimed" ? "already-hydrated" : "updated";
      },
      async markFailed(id, at) {
        failed.push({ id, at });
      },
    };
    const github: TargetHydrationReader = {
      async fetchPrContext(repo) {
        const id = repo.split("/")[1];
        if (id === "gone") {
          throw new GithubRequestError(
            404,
            "pull request not found",
            JSON.stringify({ message: "Not Found" }),
          );
        }
        if (id === "rate-limited") {
          throw new GithubRequestError(
            403,
            "rate limited",
            JSON.stringify({ message: "You have exceeded a secondary rate limit" }),
          );
        }
        if (id === "server-error") {
          throw new GithubRequestError(
            502,
            "GitHub unavailable",
            JSON.stringify({ message: "Bad Gateway" }),
          );
        }
        return { headSha: "head", baseSha: "base", pr: pr(`provider-${id}`) };
      },
    };

    const result = await runTargetHydration({
      config: { intervalMs: 1_000, batchSize: 5 },
      store,
      github,
      now: () => NOW,
      log,
    });

    expect(result).toEqual({
      scanned: 5,
      hydrated: 1,
      alreadyHydrated: 1,
      permanentlyFailed: 1,
      transientFailures: 2,
    });
    // The whole fetched pull request reaches the store, not just its id — the
    // fetch is the expensive part and the scan never revisits a hydrated row.
    expect(hydrated).toEqual([
      { id: "success", providerId: "provider-success", title: "Review me", at: NOW },
      { id: "claimed", providerId: "provider-claimed", title: "Review me", at: NOW },
    ]);
    expect(failed).toEqual([{ id: "gone", at: NOW }]);
  });

  test("a successful response with no provider id is stamped as permanent", async () => {
    const failed: string[] = [];
    const result = await runTargetHydration({
      config: { intervalMs: 1_000, batchSize: 1 },
      store: {
        async listUnhydrated() {
          return [target("missing-id")];
        },
        async hydrate() {
          throw new Error("hydrate must not run");
        },
        async markFailed(id) {
          failed.push(id);
        },
      },
      github: {
        async fetchPrContext() {
          return { headSha: "head", baseSha: "base", pr: pr(null) };
        },
      },
      now: () => NOW,
      log,
    });

    expect(result.permanentlyFailed).toBe(1);
    expect(failed).toEqual(["missing-id"]);
  });
});
