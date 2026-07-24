import { describe, expect, test } from "bun:test";
import pino, { type Logger } from "pino";

import { makeInMemorySweepLookupStore } from "../../db/dbos-sweep.ts";
import { failReviewCleanup, notifyThread } from "../cleanups.ts";
import type { FailedWorkflow, SweepContext } from "../policy.ts";

const log: Logger = pino({ level: "silent" });

function workflow(
  overrides: Partial<FailedWorkflow> = {},
): FailedWorkflow {
  return {
    workflowUuid: "wf-terminal",
    name: "SlackThreadWorkflow",
    status: "ERROR",
    updatedAtEpochMs: 1_000,
    ...overrides,
  };
}

function context(
  overrides: Partial<SweepContext> = {},
): {
  ctx: SweepContext;
  messages: Array<{ channel: string; thread_ts?: string; text?: string }>;
  failedReviews: Array<{
    reviewId: string;
    opts: { reason?: string };
  }>;
} {
  const messages: Array<{
    channel: string;
    thread_ts?: string;
    text?: string;
  }> = [];
  const failedReviews: Array<{
    reviewId: string;
    opts: { reason?: string };
  }> = [];
  const ctx: SweepContext = {
    log,
    lookups: makeInMemorySweepLookupStore(),
    slack: async () => ({
      chat: {
        async postMessage(args) {
          messages.push(args);
        },
      },
    }),
    failReview: async (reviewId, opts) => {
      failedReviews.push({ reviewId, opts });
    },
    ...overrides,
  };
  return { ctx, messages, failedReviews };
}

describe("notifyThread", () => {
  test("posts the black-hole warning to the owning channel and thread", async () => {
    const f = context({
      lookups: makeInMemorySweepLookupStore({
        threadSources: {
          "wf-terminal": {
            team: "T123",
            channel: "C456",
            threadRoot: "1721234567.001200",
          },
        },
      }),
    });

    await notifyThread(f.ctx, workflow());

    expect(f.messages).toHaveLength(1);
    expect(f.messages[0]).toMatchObject({
      channel: "C456",
      thread_ts: "1721234567.001200",
    });
    expect(f.messages[0]?.text).toContain("fresh thread");
    expect(f.messages[0]?.text).toContain("no longer reach the agent");
  });

  test("treats an absent or malformed thread source as a successful no-op", async () => {
    const absent = context();
    const malformed = context({
      lookups: makeInMemorySweepLookupStore({
        threadSources: {
          "wf-terminal": { channel: "C456" },
        },
      }),
    });

    await expect(notifyThread(absent.ctx, workflow())).resolves.toBeUndefined();
    await expect(notifyThread(malformed.ctx, workflow())).resolves.toBeUndefined();

    expect(absent.messages).toEqual([]);
    expect(malformed.messages).toEqual([]);
  });
});

describe("failReviewCleanup", () => {
  test.each(["queued", "finding", "verifying"])(
    "fails an active %s review with the sweep reason",
    async (status) => {
      const f = context({
        lookups: makeInMemorySweepLookupStore({
          reviews: {
            "wf-terminal": {
              id: "d120a0dc-78f4-4fae-af9e-0b75dcc4b1df",
              status,
            },
          },
        }),
      });
      const wf = workflow({
        name: "PrReviewWorkflow",
        status: "MAX_RECOVERY_ATTEMPTS_EXCEEDED",
      });

      await failReviewCleanup(f.ctx, wf);

      expect(f.failedReviews).toEqual([
        {
          reviewId: "d120a0dc-78f4-4fae-af9e-0b75dcc4b1df",
          opts: {
            reason:
              "review workflow wf-terminal failed terminally (MAX_RECOVERY_ATTEMPTS_EXCEEDED); swept by orphan sweep",
          },
        },
      ]);
    },
  );

  test("leaves absent and terminal reviews unchanged", async () => {
    const absent = context();
    const terminal = context({
      lookups: makeInMemorySweepLookupStore({
        reviews: {
          "wf-terminal": {
            id: "d120a0dc-78f4-4fae-af9e-0b75dcc4b1df",
            status: "failed",
          },
        },
      }),
    });

    await failReviewCleanup(absent.ctx, workflow());
    await failReviewCleanup(terminal.ctx, workflow());

    expect(absent.failedReviews).toEqual([]);
    expect(terminal.failedReviews).toEqual([]);
  });
});
