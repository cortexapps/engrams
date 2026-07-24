import { createHash } from "node:crypto";

export interface WorkflowSource {
  name: string;
  source: string;
}

export interface WorkflowHashSnapshot {
  sdkVersion: string;
  workflows: Record<string, string>;
}

export type WorkflowHashDiff =
  | { kind: "added"; name: string }
  | { kind: "changed"; name: string }
  | { kind: "removed"; name: string }
  | {
      kind: "sdk-version-changed";
      previousVersion: string;
      currentVersion: string;
    };

function md5(source: string): string {
  return createHash("md5").update(source).digest("hex");
}

function compareNames(left: string, right: string): number {
  return left < right ? -1 : left > right ? 1 : 0;
}

function sortedRecord(
  entries: Iterable<[string, string]>,
): Record<string, string> {
  return Object.fromEntries(
    Array.from(entries).sort(([left], [right]) => compareNames(left, right)),
  );
}

export function buildWorkflowHashSnapshot(
  workflowSources: readonly WorkflowSource[],
  sdkVersion: string,
): WorkflowHashSnapshot {
  return {
    sdkVersion,
    workflows: sortedRecord(
      workflowSources.map(({ name, source }) => [name, md5(source)]),
    ),
  };
}

export function diffWorkflowHashSnapshots(
  previous: WorkflowHashSnapshot,
  current: WorkflowHashSnapshot,
): WorkflowHashDiff[] {
  const workflowNames = new Set([
    ...Object.keys(previous.workflows),
    ...Object.keys(current.workflows),
  ]);
  const diffs: WorkflowHashDiff[] = [];

  for (const name of Array.from(workflowNames).sort(compareNames)) {
    const previousHash = previous.workflows[name];
    const currentHash = current.workflows[name];
    if (previousHash === undefined) {
      diffs.push({ kind: "added", name });
    } else if (currentHash === undefined) {
      diffs.push({ kind: "removed", name });
    } else if (previousHash !== currentHash) {
      diffs.push({ kind: "changed", name });
    }
  }

  if (previous.sdkVersion !== current.sdkVersion) {
    diffs.push({
      kind: "sdk-version-changed",
      previousVersion: previous.sdkVersion,
      currentVersion: current.sdkVersion,
    });
  }

  return diffs;
}

export function prApplicationVersionWarningMessages(
  main: WorkflowHashSnapshot,
  checkedOut: WorkflowHashSnapshot,
): string[] {
  const consequence =
    " — in-flight executions will be stranded and swept on the next rollout " +
    "(ADR 0104)";

  return diffWorkflowHashSnapshots(main, checkedOut).map((diff) => {
    const prefix = "this PR changes the DBOS application version: ";
    switch (diff.kind) {
      case "changed":
        return `${prefix}workflow ${diff.name} body changed vs main${consequence}`;
      case "added":
        return `${prefix}workflow ${diff.name} was added vs main${consequence}`;
      case "removed":
        return `${prefix}workflow ${diff.name} was removed vs main${consequence}`;
      case "sdk-version-changed":
        return (
          `${prefix}DBOS SDK changed vs main ` +
          `(${diff.previousVersion} → ${diff.currentVersion})${consequence}`
        );
    }
  });
}

export function serializeWorkflowHashSnapshot(
  snapshot: WorkflowHashSnapshot,
): string {
  return `${JSON.stringify(
    {
      sdkVersion: snapshot.sdkVersion,
      workflows: sortedRecord(Object.entries(snapshot.workflows)),
    },
    null,
    2,
  )}\n`;
}
