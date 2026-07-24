import { DBOS } from "@dbos-inc/dbos-sdk";

const POLL_INTERVAL_MS = 100;
const READY_TIMEOUT_MS = 15_000;

function requiredEnv(name: string): string {
  const value = process.env[name];
  if (!value) throw new Error(`${name} is required`);
  return value;
}

async function pollUntil(
  description: string,
  check: () => Promise<{ ready: boolean; observed: unknown }>,
): Promise<void> {
  const deadline = Date.now() + READY_TIMEOUT_MS;
  let lastObserved: unknown;
  while (true) {
    const result = await check();
    lastObserved = result.observed;
    if (result.ready) return;
    if (Date.now() >= deadline) {
      throw new Error(
        `${description} within ${READY_TIMEOUT_MS}ms; last observed: ` +
          JSON.stringify(lastObserved),
      );
    }
    await Bun.sleep(POLL_INTERVAL_MS);
  }
}

async function flushStrandedMarker(): Promise<void> {
  await new Promise<void>((resolve, reject) => {
    process.stdout.write("STRANDED\n", (error) => {
      if (error) reject(error);
      else resolve();
    });
  });
}

async function main(): Promise<never> {
  requiredEnv("SWEEP_TEST_RUNID");
  requiredEnv("SWEEP_TEST_BODY");
  const workflowKind = requiredEnv("SWEEP_TEST_WORKFLOW");
  const workflowId = requiredEnv("SWEEP_TEST_WORKFLOW_ID");
  const databaseUrl = requiredEnv("ORCHESTRATOR_DATABASE_URL");
  requiredEnv("DBOS__APPVERSION");
  requiredEnv("DBOS__VMID");

  if (workflowKind !== "recv" && workflowKind !== "changed") {
    throw new Error(
      `SWEEP_TEST_WORKFLOW must be "recv" or "changed", received ${workflowKind}`,
    );
  }

  const workflows = await import("./sweep-test-workflows.ts");

  DBOS.setConfig({
    name: "engrams-orchestrator",
    systemDatabaseUrl: databaseUrl,
    systemDatabaseSchemaName: "dbos",
    runAdminServer: false,
  });
  await DBOS.launch();

  const workflow =
    workflowKind === "recv"
      ? workflows.sweepTestRecvWorkflow
      : workflows.sweepTestChangedWorkflow;
  await DBOS.startWorkflow(workflow, { workflowID: workflowId })();

  await pollUntil(
    workflowKind === "changed"
      ? `workflow ${workflowId} to record alpha`
      : `workflow ${workflowId} to park its first receive timeout`,
    async () => {
      const [status, steps] = await Promise.all([
        DBOS.getWorkflowStatus(workflowId),
        DBOS.listWorkflowSteps(workflowId),
      ]);
      const expectedStep = workflowKind === "changed" ? "alpha" : "DBOS.sleep";
      return {
        ready:
          status?.status === "PENDING" &&
          (steps ?? []).some(
            (step) =>
              step.name === expectedStep &&
              (workflowKind !== "changed" || step.output === "a"),
          ),
        observed: {
          status: status?.status,
          steps: (steps ?? []).map((step) => ({
            functionID: step.functionID,
            name: step.name,
            output: step.output,
          })),
        },
      };
    },
  );

  await flushStrandedMarker();
  process.exit(0);
}

try {
  await main();
} catch (error) {
  console.error("strand-child failed", error);
  process.exit(1);
}
