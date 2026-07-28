/** Background hydration for review_target rows backfilled without provider ids. */

import type { Logger } from "pino";

import type {
  ReviewTargetHydrationStore,
  UnhydratedReviewTarget,
} from "../db/review-target-hydration.ts";
import { isPermanentGithubFailure } from "./github-review.ts";
import type { PrContext } from "./pr-context.ts";

export const TARGET_HYDRATION_INTERVAL_MS = 60_000;
export const TARGET_HYDRATION_BATCH_SIZE = 25;

export interface TargetHydratorConfig {
  intervalMs: number;
  batchSize: number;
}

export const DEFAULT_TARGET_HYDRATOR_CONFIG = {
  intervalMs: TARGET_HYDRATION_INTERVAL_MS,
  batchSize: TARGET_HYDRATION_BATCH_SIZE,
} as const satisfies TargetHydratorConfig;

export interface TargetHydrationReader {
  fetchPrContext(repo: string, number: number): Promise<{
    headSha: string;
    baseSha: string;
    pr: PrContext;
  }>;
}

export interface TargetHydratorDeps {
  config: TargetHydratorConfig;
  store: ReviewTargetHydrationStore;
  github: TargetHydrationReader;
  now: () => Date;
  log: Logger;
  setInterval?: (
    fn: () => void,
    ms: number,
  ) => ReturnType<typeof setInterval>;
  clearInterval?: (timer: ReturnType<typeof setInterval>) => void;
}

export interface TargetHydrationResult {
  scanned: number;
  hydrated: number;
  alreadyHydrated: number;
  permanentlyFailed: number;
  transientFailures: number;
}

async function markPermanentFailure(
  deps: TargetHydratorDeps,
  target: UnhydratedReviewTarget,
  error: unknown,
): Promise<void> {
  try {
    await deps.store.markFailed(target.id, deps.now());
  } catch (stampError) {
    deps.log.error(
      { targetId: target.id, repo: target.repo, number: target.number, error, stampError },
      "review target hydration failed permanently and could not stamp the row",
    );
    return;
  }
  deps.log.error(
    { targetId: target.id, repo: target.repo, number: target.number, error },
    "review target hydration failed permanently",
  );
}

/** One bounded, deterministic hydration step. One bad row never aborts peers. */
export async function runTargetHydration(
  deps: TargetHydratorDeps,
): Promise<TargetHydrationResult> {
  const targets = await deps.store.listUnhydrated(deps.config.batchSize);
  const result: TargetHydrationResult = {
    scanned: targets.length,
    hydrated: 0,
    alreadyHydrated: 0,
    permanentlyFailed: 0,
    transientFailures: 0,
  };

  for (const target of targets) {
    if (target.provider !== "github") {
      result.permanentlyFailed++;
      await markPermanentFailure(
        deps,
        target,
        new Error(`unsupported review target provider: ${target.provider}`),
      );
      continue;
    }

    try {
      const resolved = await deps.github.fetchPrContext(target.repo, target.number);
      const providerId = resolved.pr.providerId;
      if (providerId === null) {
        result.permanentlyFailed++;
        await markPermanentFailure(
          deps,
          target,
          new Error("GitHub pull request response carried no provider id"),
        );
        continue;
      }
      const outcome = await deps.store.hydrate(
        target.id,
        providerId,
        resolved.pr,
        deps.now(),
      );
      if (outcome === "updated") result.hydrated++;
      else result.alreadyHydrated++;
    } catch (error) {
      if (isPermanentGithubFailure(error)) {
        result.permanentlyFailed++;
        await markPermanentFailure(deps, target, error);
      } else {
        result.transientFailures++;
        deps.log.warn(
          { targetId: target.id, repo: target.repo, number: target.number, error },
          "review target hydration failed transiently",
        );
      }
    }
  }

  // A quiet run is the steady state once the backfill drains, so it logs at
  // debug — a line every interval forever is noise nobody reads. A run that did
  // something logs at info, because "no un-hydrated rows remain" is the signal
  // that `provider_id` can become NOT NULL and `claimTargetId` can be retired
  // (ADR 0100 decision 11), and that transition should be visible in the log
  // rather than something an operator has to go query for.
  const level = result.scanned > 0 ? "info" : "debug";
  deps.log[level](result, "review target hydration run");

  return result;
}

/** Thin scheduler around runTargetHydration, matching the sweeper split. */
export class TargetHydrator {
  readonly #deps: TargetHydratorDeps;
  #timer: ReturnType<typeof setInterval> | null = null;
  #tick: Promise<TargetHydrationResult> | null = null;

  constructor(deps: TargetHydratorDeps) {
    this.#deps = deps;
  }

  runOnce(): Promise<TargetHydrationResult> {
    this.#tick ??= runTargetHydration(this.#deps).finally(() => {
      this.#tick = null;
    });
    return this.#tick;
  }

  async start(): Promise<void> {
    if (this.#timer !== null) return;
    this.#deps.log.info(
      {
        intervalMs: this.#deps.config.intervalMs,
        batchSize: this.#deps.config.batchSize,
      },
      "review target hydrator started",
    );
    await this.runOnce();
    const schedule = this.#deps.setInterval ?? setInterval;
    this.#timer = schedule(
      () =>
        void this.runOnce().catch((error) =>
          this.#deps.log.warn({ error }, "review target hydration tick failed"),
        ),
      this.#deps.config.intervalMs,
    );
  }

  async stop(): Promise<void> {
    if (this.#timer !== null) {
      const cancel = this.#deps.clearInterval ?? clearInterval;
      cancel(this.#timer);
      this.#timer = null;
    }
    await this.#tick?.catch(() => {});
  }
}
