import "../src/workflows/slack-thread.ts";
import "../src/workflows/pr-review.ts";
import "../src/workflows/tool-exec.ts";
import "../src/workflows/automation-run.ts";

import { getAllRegisteredFunctions } from "../node_modules/@dbos-inc/dbos-sdk/dist/src/decorators.js";

import {
  buildWorkflowHashSnapshot,
  diffWorkflowHashSnapshots,
  serializeWorkflowHashSnapshot,
  type WorkflowHashDiff,
  type WorkflowHashSnapshot,
} from "../src/sweep/workflow-hashes.ts";

interface RegisteredFunction {
  name: string;
  workflowConfig?: unknown;
  origFunction: Function;
}

interface SdkPackage {
  version: string;
}

const snapshotPath = new URL("../dbos-workflow-hashes.json", import.meta.url);
const snapshotDisplayPath = "orchestrator/dbos-workflow-hashes.json";
const updateCommand = "bun scripts/dbos-workflow-hashes.ts --update";

function annotationMessage(diff: WorkflowHashDiff): string {
  switch (diff.kind) {
    case "changed":
      return `DBOS workflow body changed: ${diff.name} — in-flight executions of the previous deploy will be stranded and swept on the next rollout (ADR 0104); run '${updateCommand}' and commit if intended`;
    case "added":
      return `DBOS workflow added: ${diff.name} — the shared DBOS application version will change and in-flight executions of the previous deploy will be stranded and swept on the next rollout (ADR 0104); run '${updateCommand}' and commit if intended`;
    case "removed":
      return `DBOS workflow removed: ${diff.name} — in-flight executions of the removed workflow will be stranded and require sweep handling after the next rollout (ADR 0104); run '${updateCommand}' and commit if intended`;
    case "sdk-version-changed":
      return `DBOS SDK version changed: ${diff.previousVersion} → ${diff.currentVersion} — the shared DBOS application version will change and in-flight executions of the previous deploy will be stranded and swept on the next rollout (ADR 0104); run '${updateCommand}' and commit if intended`;
  }
}

function printWarning(message: string): void {
  const escaped = message
    .replaceAll("%", "%25")
    .replaceAll("\r", "%0D")
    .replaceAll("\n", "%0A");
  console.log(`::warning file=${snapshotDisplayPath}::${escaped}`);
}

function describeDiff(diff: WorkflowHashDiff): string {
  switch (diff.kind) {
    case "added":
      return `added workflow ${diff.name}`;
    case "changed":
      return `changed workflow ${diff.name}`;
    case "removed":
      return `removed workflow ${diff.name}`;
    case "sdk-version-changed":
      return `changed DBOS SDK version ${diff.previousVersion} → ${diff.currentVersion}`;
  }
}

async function currentSnapshot(): Promise<WorkflowHashSnapshot> {
  const registrations: RegisteredFunction[] = getAllRegisteredFunctions();
  const workflowSources = registrations
    .filter((registration) => registration.workflowConfig)
    .map((registration) => ({
      name: registration.name,
      source: registration.origFunction.toString(),
    }));
  const sdkPackage = (await Bun.file(
    new URL(
      "../node_modules/@dbos-inc/dbos-sdk/package.json",
      import.meta.url,
    ),
  ).json()) as SdkPackage;

  return buildWorkflowHashSnapshot(workflowSources, sdkPackage.version);
}

async function readCommittedSnapshot(): Promise<WorkflowHashSnapshot | null> {
  const file = Bun.file(snapshotPath);
  if (!(await file.exists())) return null;
  return (await file.json()) as WorkflowHashSnapshot;
}

const args = new Set(Bun.argv.slice(2));
const update = args.delete("--update");
const checkOnly = args.delete("--check-only");
if (args.size > 0 || (update && checkOnly)) {
  throw new Error(
    "Usage: bun scripts/dbos-workflow-hashes.ts [--update | --check-only]",
  );
}

const current = await currentSnapshot();
const committed = await readCommittedSnapshot();

if (update) {
  await Bun.write(snapshotPath, serializeWorkflowHashSnapshot(current));
  if (committed === null) {
    console.log(`created ${snapshotDisplayPath}`);
  } else {
    const diffs = diffWorkflowHashSnapshots(committed, current);
    if (diffs.length === 0) {
      console.log(`${snapshotDisplayPath} is already up to date`);
    } else {
      for (const diff of diffs) console.log(describeDiff(diff));
      console.log(`updated ${snapshotDisplayPath}`);
    }
  }
} else if (committed === null) {
  printWarning(
    `DBOS workflow hash snapshot is missing — run '${updateCommand}' and commit ${snapshotDisplayPath}`,
  );
} else {
  const diffs = diffWorkflowHashSnapshots(committed, current);
  if (diffs.length === 0) {
    if (!checkOnly) console.log("DBOS workflow hashes unchanged");
  } else {
    for (const diff of diffs) printWarning(annotationMessage(diff));
  }
}
