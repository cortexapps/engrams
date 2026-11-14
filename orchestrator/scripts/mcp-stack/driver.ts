import type { Context } from "./context.ts";
import { requireThat } from "./context.ts";
import type { Observation } from "./evidence.ts";
import { SCENARIOS, type Harness, type ScenarioId } from "./manifest.ts";
import type { DatabaseObserver } from "./observer.ts";
import type { Principals } from "./scenario-runtime.ts";

export type DriverObservation = Omit<Observation, "caseKey">;
export type DriverAssertion = {
  assertion: string;
  observations: DriverObservation[];
};
export type DriverInput = {
  ctx: Context;
  principals: Principals;
  database: DatabaseObserver;
  harness: Harness;
};
export type CaseDriver = (input: DriverInput) => Promise<DriverAssertion[]>;

/** Validate completeness only. The driver must make each assertion against actual observations. */
export function completeAssertions(
  id: ScenarioId,
  assertions: DriverAssertion[],
) {
  const required = SCENARIOS.find((value) => value.id === id)!;
  requireThat(
    assertions.length === required.assertions.length &&
      new Set(assertions.map((value) => value.assertion)).size ===
        assertions.length &&
      required.assertions.every((name) =>
        assertions.some((value) => value.assertion === name),
      ) &&
      assertions.every((value) => value.observations.length > 0),
    "driver_subassertions_complete",
  );
  return assertions;
}
