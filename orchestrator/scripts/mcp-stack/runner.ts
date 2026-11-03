import { ConnectError } from "@connectrpc/connect";
import { bootstrapHumans, publishOperator } from "./bootstrap.ts";
import { parseArgs } from "./cli.ts";
import {
  loadContext,
  requireThat,
  RunnerFailure,
  sha256,
  type Context,
} from "./context.ts";
import { completeAssertions, type CaseDriver } from "./driver.ts";
import { Evidence } from "./evidence.ts";
import { canonicalJson } from "./guest.ts";
import { REQUIRED_CASES, SCENARIOS, type ScenarioId } from "./manifest.ts";
import { DatabaseObserver } from "./observer.ts";
import { preflight, publicPrerequisites } from "./preflight.ts";

export type DriverRegistry = Record<ScenarioId, CaseDriver>;
export function validateDrivers(
  drivers: Record<string, unknown>,
): asserts drivers is DriverRegistry {
  const names = Object.keys(drivers);
  requireThat(
    names.length === SCENARIOS.length &&
      SCENARIOS.every((s) => typeof drivers[s.id] === "function") &&
      names.every((name) => SCENARIOS.some((s) => s.id === name)),
    "complete_driver_registry",
  );
}
export function safeFailure(error: unknown): RunnerFailure {
  if (
    error instanceof RunnerFailure &&
    /^[A-Za-z0-9_./:-]{1,200}$/.test(error.seam)
  )
    return error;
  if (error instanceof ConnectError)
    return new RunnerFailure(
      "TRANSPORT_FAILED",
      `public_rpc_code_${error.code}`,
    );
  return new RunnerFailure("ASSERTION_FAILED", "unexpected_runner_error");
}

/** A failed or missing case leaves an incomplete artifact and stops the matrix. */
export async function runMatrix(ctx: Context, drivers: DriverRegistry) {
  validateDrivers(drivers);
  const evidence = await Evidence.create(ctx);
  let database: DatabaseObserver | undefined;
  try {
    database = await DatabaseObserver.connect(ctx);
    const supervisor = await preflight(ctx, database);
    const publication = await publishOperator(ctx);
    const principals = await bootstrapHumans(ctx);
    const ready = await publicPrerequisites(ctx, principals.admin);
    await evidence.prerequisites({
      supervisorSha256: sha256(canonicalJson(supervisor)),
      publicationSha256: sha256(canonicalJson(publication)),
      publicStateSha256: sha256(canonicalJson(ready)),
    });
    for (const selected of REQUIRED_CASES) {
      try {
        const assertions = completeAssertions(
          selected.id,
          await drivers[selected.id]({
            ctx,
            principals,
            database,
            harness: selected.harness,
          }),
        );
        const recorded = [];
        for (const assertion of assertions) {
          const observationHashes = [];
          for (const observation of assertion.observations)
            observationHashes.push(
              await evidence.observe({ ...observation, caseKey: selected.key }),
            );
          recorded.push({ assertion: assertion.assertion, observationHashes });
        }
        await evidence.result({
          key: selected.key,
          status: "PASS",
          assertions: recorded,
        });
      } catch (error) {
        const failure = safeFailure(error);
        await evidence.result({
          key: selected.key,
          status: failure.code === "NEEDS_CONTEXT" ? "NEEDS_CONTEXT" : "FAIL",
          assertions: [],
          seam: failure.seam,
        });
        throw failure;
      }
    }
    await evidence.finish();
  } finally {
    const results = await Promise.allSettled([
      evidence.close(),
      ...(database ? [database.close()] : []),
    ]);
    requireThat(
      results.every((result) => result.status === "fulfilled"),
      "runner_observer_evidence_close",
    );
  }
}

/** Only the executable invokes this watchdog. The adapter owns service teardown on every exit. */
export async function main(drivers: DriverRegistry, args: readonly string[]) {
  const begun = performance.now();
  const expired = () => {
    process.stderr.write('{"status":"FAIL","seam":"runner_total_deadline"}\n');
    process.exit(124);
  };
  let watchdog = setTimeout(expired, 7200000);
  try {
    const selected = parseArgs(args);
    validateDrivers(drivers);
    const ctx = await loadContext(selected.context);
    clearTimeout(watchdog);
    const remaining = ctx.deadlines.totalMs - (performance.now() - begun);
    requireThat(remaining > 0, "runner_context_deadline");
    watchdog = setTimeout(expired, remaining);
    await runMatrix(ctx, drivers);
    process.stdout.write(
      JSON.stringify({
        status: "PASS",
        selected: selected.selected.length,
        passed: REQUIRED_CASES.length,
        evidenceFile: ctx.evidenceFile,
      }) + "\n",
    );
  } catch (error) {
    const failure = safeFailure(error);
    process.stderr.write(
      JSON.stringify({
        status: failure.code === "NEEDS_CONTEXT" ? "NEEDS_CONTEXT" : "FAIL",
        code: failure.code,
        seam: failure.seam,
      }) + "\n",
    );
    process.exitCode = 1;
  } finally {
    clearTimeout(watchdog);
  }
}
