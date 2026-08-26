/** ADR 0120: the runtime wrapper's handle-write site — a successful action
 * by an instance-bound run writes every declared handle template that
 * renders; retries converge; a conflict is a typed permanent failure. Uses
 * the REAL built-in slack connector (post_message declares the thread AND
 * own-ts templates) over a fake transport, so the manifest and the writer
 * are exercised together. */

import { beforeEach, describe, expect, test } from "bun:test";

import { invalidateRegistry } from "../../../connectors/registry.ts";
import type { BuiltinActionDeps } from "../builtin.ts";
import { IntegrationActionError } from "../errors.ts";
import { makeIntegrationActionRuntime } from "../runtime.ts";

interface HandleWrite {
  automationId: string;
  handle: string;
  instanceId: string;
  writtenBy: string;
}

function fakeLedger(preOwned: Record<string, string> = {}) {
  const owners = new Map(Object.entries(preOwned));
  const writes: HandleWrite[] = [];
  return {
    writes,
    instances: {
      async recordInstanceHandle(input: HandleWrite) {
        writes.push(input);
        const holder = owners.get(input.handle);
        if (holder === undefined) {
          owners.set(input.handle, input.instanceId);
          return { kind: "recorded" as const };
        }
        return holder === input.instanceId
          ? { kind: "already_ours" as const }
          : { kind: "conflict" as const, instanceId: holder };
      },
    },
  };
}

function slackPostDeps(ledger: ReturnType<typeof fakeLedger>) {
  const posts: Array<{ channel: string; text: string; thread_ts?: string }> = [];
  // slack.post_message is a builtin execution: it talks through the bot-token
  // slack client seam, not the generic HTTP op. Built from scratch (the
  // execute.test.ts precedent) so a test only reaches seams it provides.
  const builtinDeps: BuiltinActionDeps = {
    runOp: async () => {
      throw new Error("not used by slack.post_message");
    },
    githubPoster: () => {
      throw new Error("not used");
    },
    linearClient: () => {
      throw new Error("not used");
    },
    slackClient: async () => ({
      chat: {
        postMessage: async (args) => {
          posts.push(args);
          return { ts: "1724.200", channel: "C1" };
        },
        update: async () => {
          throw new Error("not used");
        },
      },
      conversations: {
        join: async () => {
          throw new Error("not used");
        },
      },
    }),
  };
  return {
    posts,
    deps: {
      connectors: { list: async () => [] },
      instances: ledger.instances,
      builtinDeps,
    },
  };
}

const BASE = {
  provider: "slack",
  actionId: "post_message",
  runId: "autorun:auto-1:main:i-ai_one:d1",
  stepPath: "notify",
  automationId: "auto-1",
  instanceId: "ai_one",
};

beforeEach(() => invalidateRegistry());

describe("makeIntegrationActionRuntime handle writes (ADR 0120)", () => {
  test("a threaded reply writes the thread handle AND its own ts (redundant handles)", async () => {
    const ledger = fakeLedger();
    const { deps } = slackPostDeps(ledger);
    const outputs = await makeIntegrationActionRuntime(deps).execute({
      ...BASE,
      params: { channel: "C1", text: "hi", threadTs: "1724.100" },
    });
    expect(outputs).toMatchObject({ ts: "1724.200", channel: "C1" });
    expect(ledger.writes.map((w) => w.handle)).toEqual([
      "slack:C1:1724.100",
      "slack:C1:1724.200",
    ]);
    expect(ledger.writes[0]).toMatchObject({
      automationId: "auto-1",
      instanceId: "ai_one",
      writtenBy: "autorun:auto-1:main:i-ai_one:d1:notify",
    });
  });

  test("a top-level post writes only its own ts; a replay converges as already_ours", async () => {
    const ledger = fakeLedger();
    const { deps } = slackPostDeps(ledger);
    const params = { channel: "C1", text: "hi" };
    await makeIntegrationActionRuntime(deps).execute({ ...BASE, params });
    expect(ledger.writes.map((w) => w.handle)).toEqual(["slack:C1:1724.200"]);
    // The DBOS step replays after a crash-before-checkpoint: same write,
    // already_ours, no error.
    await makeIntegrationActionRuntime(deps).execute({ ...BASE, params });
    expect(ledger.writes).toHaveLength(2);
  });

  test("a handle another workstream owns is a typed permanent failure — never a silent rebind", async () => {
    const ledger = fakeLedger({ "slack:C1:1724.100": "ai_other" });
    const { deps } = slackPostDeps(ledger);
    await expect(
      makeIntegrationActionRuntime(deps).execute({
        ...BASE,
        params: { channel: "C1", text: "hi", threadTs: "1724.100" },
      }),
    ).rejects.toMatchObject({
      name: "IntegrationActionError",
      permanent: true,
    });
  });

  test("an unbound run writes nothing", async () => {
    const ledger = fakeLedger();
    const { deps } = slackPostDeps(ledger);
    await makeIntegrationActionRuntime(deps).execute({
      provider: "slack",
      actionId: "post_message",
      params: { channel: "C1", text: "hi" },
      runId: "autorun:auto-1:d1",
      stepPath: "notify",
    });
    expect(ledger.writes).toEqual([]);
  });

  test("the error type is the engine's retry-policy contract", () => {
    expect(new IntegrationActionError("x", true).permanent).toBe(true);
  });
});
