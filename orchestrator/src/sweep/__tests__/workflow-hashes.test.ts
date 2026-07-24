import { describe, expect, test } from "bun:test";

import {
  buildWorkflowHashSnapshot,
  diffWorkflowHashSnapshots,
  prApplicationVersionWarningMessages,
  serializeWorkflowHashSnapshot,
  type WorkflowHashSnapshot,
} from "../workflow-hashes.ts";

const original: WorkflowHashSnapshot = {
  sdkVersion: "4.21.6",
  workflows: {
    AlphaWorkflow: "alpha-old",
    RemovedWorkflow: "removed-hash",
  },
};

describe("DBOS workflow hash snapshots", () => {
  test("hashes each workflow source with MD5", () => {
    expect(
      buildWorkflowHashSnapshot(
        [{ name: "AlphaWorkflow", source: "async function Alpha() {}" }],
        "4.21.6",
      ),
    ).toEqual({
      sdkVersion: "4.21.6",
      workflows: {
        AlphaWorkflow: "5a7c3e1279f4b5e883bccb2610bc03ae",
      },
    });
  });

  test("identical snapshots have no differences", () => {
    expect(diffWorkflowHashSnapshots(original, original)).toEqual([]);
  });

  test("reports a changed workflow by name", () => {
    const current: WorkflowHashSnapshot = {
      ...original,
      workflows: {
        ...original.workflows,
        AlphaWorkflow: "alpha-new",
      },
    };

    expect(diffWorkflowHashSnapshots(original, current)).toEqual([
      { kind: "changed", name: "AlphaWorkflow" },
    ]);
  });

  test("reports added and removed workflows", () => {
    const current: WorkflowHashSnapshot = {
      ...original,
      workflows: {
        AddedWorkflow: "added-hash",
        AlphaWorkflow: "alpha-old",
      },
    };

    expect(diffWorkflowHashSnapshots(original, current)).toEqual([
      { kind: "added", name: "AddedWorkflow" },
      { kind: "removed", name: "RemovedWorkflow" },
    ]);
  });

  test("reports an SDK version change separately", () => {
    const current: WorkflowHashSnapshot = {
      ...original,
      sdkVersion: "4.22.0",
    };

    expect(diffWorkflowHashSnapshots(original, current)).toEqual([
      {
        kind: "sdk-version-changed",
        previousVersion: "4.21.6",
        currentVersion: "4.22.0",
      },
    ]);
  });

  test("describes workflow body changes against main as PR warnings", () => {
    const current: WorkflowHashSnapshot = {
      ...original,
      workflows: {
        ...original.workflows,
        AlphaWorkflow: "alpha-new",
      },
    };

    expect(prApplicationVersionWarningMessages(original, current)).toEqual([
      "this PR changes the DBOS application version: workflow AlphaWorkflow " +
        "body changed vs main — in-flight executions will be stranded and " +
        "swept on the next rollout (ADR 0104)",
    ]);
  });

  test("describes added, removed, and SDK changes against main", () => {
    const current: WorkflowHashSnapshot = {
      sdkVersion: "4.22.0",
      workflows: {
        AddedWorkflow: "added-hash",
        AlphaWorkflow: "alpha-old",
      },
    };

    expect(prApplicationVersionWarningMessages(original, current)).toEqual([
      "this PR changes the DBOS application version: workflow AddedWorkflow " +
        "was added vs main — in-flight executions will be stranded and swept " +
        "on the next rollout (ADR 0104)",
      "this PR changes the DBOS application version: workflow RemovedWorkflow " +
        "was removed vs main — in-flight executions will be stranded and " +
        "swept on the next rollout (ADR 0104)",
      "this PR changes the DBOS application version: DBOS SDK changed vs main " +
        "(4.21.6 → 4.22.0) — in-flight executions will be stranded and swept " +
        "on the next rollout (ADR 0104)",
    ]);
  });

  test("serialization is byte-stable, key-sorted, and newline-terminated", () => {
    const first = buildWorkflowHashSnapshot(
      [
        { name: "ZuluWorkflow", source: "zulu" },
        { name: "AlphaWorkflow", source: "alpha" },
      ],
      "4.21.6",
    );
    const second = buildWorkflowHashSnapshot(
      [
        { name: "AlphaWorkflow", source: "alpha" },
        { name: "ZuluWorkflow", source: "zulu" },
      ],
      "4.21.6",
    );

    const serialized = serializeWorkflowHashSnapshot(first);
    expect(serialized).toBe(serializeWorkflowHashSnapshot(second));
    expect(serialized).toBe(
      '{\n' +
        '  "sdkVersion": "4.21.6",\n' +
        '  "workflows": {\n' +
        '    "AlphaWorkflow": "2c1743a391305fbf367df8e4f069f9f9",\n' +
        '    "ZuluWorkflow": "7ab2493176d187c505a837d3c5cf8af5"\n' +
        "  }\n" +
        "}\n",
    );
  });
});
