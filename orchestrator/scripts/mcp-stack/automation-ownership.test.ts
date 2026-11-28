import { expect, test } from "bun:test";
import {
  chmod,
  link,
  mkdtemp,
  readdir,
  readFile,
  realpath,
  rename,
  rm,
  symlink,
  writeFile,
} from "node:fs/promises";
import { join } from "node:path";
import { tmpdir } from "node:os";
import { create } from "@bufbuild/protobuf";
import {
  AutomationSchema,
  AutomationRunSchema,
} from "../../src/gen/engram/app/v1/automation_pb.ts";
import { TaskSchema } from "../../src/gen/engram/app/v1/task_pb.ts";
import { SessionSchema } from "../../src/gen/engram/app/v1/session_pb.ts";
import { registerEngineBlocks } from "../../src/automations/engine/blocks/index.ts";
import { validateDefinition } from "../../src/automations/engine/definition.ts";
import { definitionOf } from "../../src/db/automations.ts";
import { canonicalJson } from "./guest.ts";
import { sha256 } from "./context.ts";
import {
  recordOwnedAutomation,
  readOwnedAutomationRuns,
  ownedAutomationCleanupSessions,
  type AutomationCleanupClient,
  type ManagedAutomationSession,
} from "./automation-ownership.ts";

const hostId = "11111111-1111-4111-8111-111111111111";
const sessionId = "22222222-2222-4222-8222-222222222222";
const foreignId = "33333333-3333-4333-8333-333333333333";
const taskId = "fixture-root-task";
const runId = "autorun:fixture-automation:manual:fixture-event";
const definition = {
  engine: 1,
  trigger: { kind: "manual" },
  blocks: [
    {
      id: "launch",
      type: "create_session",
      config: {
        profileId: "fixture-profile",
        promptTemplate: "",
        titleTemplate: "Fixture automation task",
        includeEventContext: false,
        keepOnFinish: true,
      },
    },
    {
      id: "lifetime",
      type: "wait_session",
      config: { session: { blockId: "launch" }, until: "ended" },
    },
  ],
  inputsSchema: [],
  settings: { endSessionsOnFinish: false, runDeadlineSeconds: 120 },
};
registerEngineBlocks();
const normalized = validateDefinition(definition);
const projectedDefinition = definitionOf({
  automationId: "fixture-automation",
  version: 1,
  trigger: normalized.trigger,
  blocks: normalized.blocks,
  entrypoints: normalized.entrypoints ?? [],
  agent: normalized.agent ?? null,
  role: normalized.role ?? null,
  promptTemplate: normalized.promptTemplate ?? null,
  inputsSchema: normalized.inputsSchema,
  settings: normalized.settings,
  createdByUserId: "fixture-admin",
  createdAt: new Date("2026-01-01T00:00:00Z"),
});

const owned = {
  automationId: "fixture-automation",
  definitionActorId: "fixture-admin",
  definitionSha256: sha256(canonicalJson(definition)),
  profileId: "fixture-profile",
};
const launchOutput = {
  session_id: sessionId,
  task_id: taskId,
  initial_prompt: false,
  prompt: "",
};
async function fixture() {
  const root = await mkdtemp(
    join(await realpath(tmpdir()), "mcp-automation-owner-"),
  );
  const ctx = {
    runRoot: root,
    runId: "fixture-run",
    sourceCommit: "a".repeat(40),
    hostId,
  };
  await recordOwnedAutomation(ctx, owned);
  await recordOwnedAutomation(ctx, {
    ...owned,
    runId: runId,
  });
  const automation = create(AutomationSchema, {
    id: owned.automationId,
    kind: "user",
    currentVersion: 1,
    createdByUserId: owned.definitionActorId,
    version: {
      automationId: owned.automationId,
      number: 1,
      createdByUserId: owned.definitionActorId,
      definitionJson: JSON.stringify(projectedDefinition),
    },
  });
  const run = create(AutomationRunSchema, {
    brief: {
      id: runId,
      automationId: owned.automationId,
      version: 1,
      triggerSource: "manual",
      dryRun: false,
      status: "running",
    },
    steps: [
      {
        blockId: "launch",
        status: "succeeded",
        sessionId,
        outputsJson: JSON.stringify(launchOutput),
      },
    ],
    sessionIds: [sessionId],
  });
  const task = create(TaskSchema, {
    id: taskId,
    type: "automation",
    rootTaskId: taskId,
    sourceJson: JSON.stringify({
      provider: "automation",
      automationId: owned.automationId,
      runId: runId,
    }),
    sessions: [{ sessionId, role: "primary" }],
  });
  const client: AutomationCleanupClient = {
    automations: {
      getAutomation: async (request) => {
        expect(request.lookup).toEqual({
          case: "id",
          value: owned.automationId,
        });
        return { $typeName: "engram.app.v1.GetAutomationResponse", automation };
      },
    },
    automationRuns: {
      getRun: async (request) => {
        expect(request.runId).toBe(runId);
        return { $typeName: "engram.app.v1.GetRunResponse", run };
      },
    },
    tasks: {
      getTask: async (request) => {
        expect(request.taskId).toBe(taskId);
        return { $typeName: "engram.app.v1.GetTaskResponse", task };
      },
    },
  };
  const managed = new Map<string, ManagedAutomationSession>();
  const proof = () =>
    ownedAutomationCleanupSessions(
      ctx,
      client,
      managed,
      new Set([owned.definitionActorId]),
      new Set([sessionId]),
    );
  return { root, ctx, automation, run, task, client, managed, proof };
}
type Fixture = Awaited<ReturnType<typeof fixture>>;
async function withFixture(action: (value: Fixture) => Promise<void>) {
  const value = await fixture();
  try {
    await action(value);
  } finally {
    await rm(value.root, { recursive: true });
  }
}

test("production definition validation and stored projection preserve the fixture definition hash", () => {
  expect(sha256(canonicalJson(projectedDefinition))).toBe(
    owned.definitionSha256,
  );
});

test("automation ownership retains exact session and task after launch output", async () => {
  await withFixture(async (f) => {
    await recordOwnedAutomation(f.ctx, { ...owned, runId, sessionId, taskId });
    expect(await readOwnedAutomationRuns(f.ctx)).toEqual([
      {
        ...owned,
        runId,
        sessionId,
        taskId,
        version: 1,
        fixtureRunId: f.ctx.runId,
        sourceCommit: f.ctx.sourceCommit,
        hostId,
      },
    ]);
  });
});

for (const boot of [false, true]) {
  test(`automation cleanup proves exact session ownership ${boot ? "with a boot record" : "before host assignment"}`, async () => {
    await withFixture(async (f) => {
      if (boot)
        f.managed.set(sessionId, {
          sessionId,
          actorId: `automation:${owned.automationId}`,
          hostId,
          taskId,
        });
      expect([...(await f.proof())]).toEqual([sessionId]);
      expect(await readOwnedAutomationRuns(f.ctx)).toEqual([
        {
          ...owned,
          runId: runId,
          version: 1,
          fixtureRunId: f.ctx.runId,
          sourceCommit: f.ctx.sourceCommit,
          hostId,
        },
      ]);
    });
  });
}
for (const status of ["completed", "failed", "deadline"]) {
  test(`terminal ${status} automation is provable only for cleanup`, async () => {
    await withFixture(async (f) => {
      f.run.brief!.status = status;
      expect([...(await f.proof())]).toEqual([sessionId]);
    });
  });
}
const conflicts: [string, (f: Fixture) => void][] = [
  [
    "foreign automation",
    (f) => {
      f.automation.id = "foreign-automation";
    },
  ],
  [
    "foreign creator",
    (f) => {
      f.automation.createdByUserId = "foreign-creator";
    },
  ],
  [
    "foreign version creator",
    (f) => {
      f.automation.version!.createdByUserId = "foreign-creator";
    },
  ],
  [
    "changed definition",
    (f) => {
      f.automation.version!.definitionJson = "{}";
    },
  ],
  [
    "wrong run",
    (f) => {
      f.run.brief!.id = "other-run";
    },
  ],
  [
    "run from another automation",
    (f) => {
      f.run.brief!.automationId = "foreign-automation";
    },
  ],
  [
    "wrong run version",
    (f) => {
      f.run.brief!.version = 2;
    },
  ],
  [
    "dry run",
    (f) => {
      f.run.brief!.dryRun = true;
    },
  ],
  [
    "wrong run session",
    (f) => {
      f.run.sessionIds = [foreignId];
    },
  ],
  [
    "wrong launch session",
    (f) => {
      f.run.steps[0]!.sessionId = foreignId;
    },
  ],
  [
    "wrong launch output session",
    (f) => {
      f.run.steps[0]!.outputsJson = JSON.stringify({
        ...launchOutput,
        session_id: foreignId,
      });
    },
  ],
  [
    "wrong root task",
    (f) => {
      f.task.rootTaskId = "foreign-task";
    },
  ],
  [
    "wrong task id",
    (f) => {
      f.task.id = "foreign-task";
    },
  ],
  [
    "wrong task run",
    (f) => {
      f.task.sourceJson = JSON.stringify({
        provider: "automation",
        automationId: owned.automationId,
        runId: "foreign-run",
      });
    },
  ],
  [
    "wrong task automation",
    (f) => {
      f.task.sourceJson = JSON.stringify({
        provider: "automation",
        automationId: "foreign-automation",
        runId: runId,
      });
    },
  ],
  [
    "wrong task session",
    (f) => {
      f.task.sessions[0]!.sessionId = foreignId;
    },
  ],
  [
    "foreign public host",
    (f) => {
      f.task.sessions[0]!.session = create(SessionSchema, {
        id: sessionId,
        hostId: foreignId,
        status: "active",
      });
    },
  ],
];
for (const [name, conflict] of conflicts) {
  test(`automation cleanup rejects ${name}`, async () => {
    await withFixture(async (f) => {
      conflict(f);
      if (name === "wrong launch output session")
        expect([...(await f.proof())]).toEqual([]);
      else await expect(f.proof()).rejects.toThrow();
    });
  });
}
for (const [name, value] of [
  ["actorId", "automation:foreign"],
  ["taskId", "foreign-task"],
  ["hostId", foreignId],
  ["sessionId", foreignId],
] as const) {
  test(`automation cleanup rejects existing boot ${name} conflict`, async () => {
    await withFixture(async (f) => {
      f.managed.set(sessionId, {
        sessionId,
        actorId: `automation:${owned.automationId}`,
        hostId,
        taskId,
        [name]: value,
      });
      await expect(f.proof()).rejects.toThrow(
        "cleanup_automation_managed_identity",
      );
    });
  });
}
for (const mode of [
  "substituted",
  "wrong-name",
  "public",
  "symlink",
  "hardlink",
] as const) {
  test(`automation ownership rejects a ${mode} receipt`, async () => {
    await withFixture(async (f) => {
      const name = (await readdir(f.root)).find((name) =>
        name.startsWith("automation-run-owner-"),
      );
      expect(name).toBeDefined();
      const path = join(f.root, name!);
      if (mode === "substituted") {
        const receipt = JSON.parse(await readFile(path, "utf8"));
        receipt.fixtureRunId = "foreign-run";
        await writeFile(path, JSON.stringify(receipt), { mode: 0o600 });
      } else if (mode === "wrong-name") {
        await rename(
          path,
          join(f.root, `automation-run-owner-${"0".repeat(64)}.json`),
        );
      } else if (mode === "public") await chmod(path, 0o644);
      else if (mode === "hardlink") await link(path, path + ".other");
      else {
        await rename(path, path + ".other");
        await symlink(path + ".other", path);
      }
      await expect(readOwnedAutomationRuns(f.ctx)).rejects.toThrow();
    });
  });
}
