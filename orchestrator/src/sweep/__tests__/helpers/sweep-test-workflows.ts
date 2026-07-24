import { DBOS } from "@dbos-inc/dbos-sdk";

const runId = process.env["SWEEP_TEST_RUNID"];
if (!runId) {
  throw new Error("SWEEP_TEST_RUNID is required before importing sweep test workflows");
}

const body = process.env["SWEEP_TEST_BODY"] ?? "a";
if (body !== "a" && body !== "b") {
  throw new Error(`SWEEP_TEST_BODY must be "a" or "b", received ${body}`);
}

export const sweepTestRecvWorkflowName = `SweepTestRecv-${runId}`;
export const sweepTestChangedWorkflowName = `SweepTestChanged-${runId}`;

async function recvUntilStop(): Promise<string[]> {
  const received: string[] = [];
  while (true) {
    const message = await DBOS.recv<string>("sweeptest", 2);
    if (message === null) continue;
    if (message === "stop") return received;
    received.push(message);
  }
}

export const sweepTestRecvWorkflow = DBOS.registerWorkflow(
  async function sweepTestRecv(): Promise<string[]> {
    return recvUntilStop();
  },
  { name: sweepTestRecvWorkflowName },
);

const changedWorkflowBody =
  body === "b"
    ? async function sweepTestChangedB(): Promise<string[]> {
        await DBOS.runStep(async () => "b", { name: "beta" });
        return recvUntilStop();
      }
    : async function sweepTestChangedA(): Promise<string[]> {
        await DBOS.runStep(async () => "a", { name: "alpha" });
        return recvUntilStop();
      };

export const sweepTestChangedWorkflow = DBOS.registerWorkflow(
  changedWorkflowBody,
  { name: sweepTestChangedWorkflowName },
);
