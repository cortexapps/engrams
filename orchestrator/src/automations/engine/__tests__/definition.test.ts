import { describe, expect, test } from "bun:test";

import { registerEngineBlocks } from "../blocks/index.ts";
import {
  cronEntrypointOf,
  DefinitionError,
  entrypointOf,
  entrypointsOf,
  validateDefinition,
  type AutomationDefinition,
} from "../definition.ts";

registerEngineBlocks();

function def(overrides: Partial<AutomationDefinition> = {}): unknown {
  return {
    engine: 1,
    trigger: { kind: "cron", schedule: "0 9 * * 1-5", timezone: "UTC" },
    blocks: [
      {
        id: "launch",
        type: "create_session",
        config: { profileId: "p1", promptTemplate: "Do the thing for ${{ trigger.automation.name }}" },
      },
    ],
    inputsSchema: [],
    settings: { endSessionsOnFinish: false },
    ...overrides,
  };
}

describe("validateDefinition", () => {
  test("accepts a minimal user definition", () => {
    const parsed = validateDefinition(def(), { kind: "user" });
    expect(parsed.blocks[0]!.type).toBe("create_session");
  });

  test("continueOnly must be a subset of eventKeys and needs policy join", () => {
    const integration = (continueOnly: string[], policy?: "join" | "queue") =>
      def({
        trigger: {
          kind: "integration",
          provider: "slack",
          connectionId: "c",
          eventKeys: ["app_mention", "message"],
          continueOnly,
        },
        settings: {
          endSessionsOnFinish: false,
          ...(policy ? { concurrency: { keyTemplate: "k", policy } } : {}),
        },
      } as Partial<AutomationDefinition>);
    expect(validateDefinition(integration(["message"], "join"), { kind: "user" }).trigger).toMatchObject({
      continueOnly: ["message"],
    });
    expect(() => validateDefinition(integration(["reaction_added"], "join"), { kind: "user" })).toThrow(
      /continueOnly event "reaction_added" is not one of the trigger's eventKeys/,
    );
    expect(() => validateDefinition(integration(["message"], "queue"), { kind: "user" })).toThrow(
      /continueOnly needs settings.concurrency.policy "join"/,
    );
    expect(() => validateDefinition(integration(["message"]), { kind: "user" })).toThrow(
      /continueOnly needs settings.concurrency.policy "join"/,
    );
  });

  test("rejects duplicate block ids across nesting", () => {
    const raw = def({
      blocks: [
        { id: "a", type: "filter", config: { conditions: { mode: "all", conditions: [] } } },
        {
          id: "b",
          type: "branch",
          config: { conditions: { mode: "all", conditions: [] } },
          then: [{ id: "a", type: "end_session", config: { session: { blockId: "x" } } }],
        },
      ],
    } as Partial<AutomationDefinition>);
    expect(() => validateDefinition(raw, { kind: "user" })).toThrow(/duplicate block id/);
  });

  test("rejects unknown block types and bad configs with a block+field address", () => {
    expect(() =>
      validateDefinition(def({ blocks: [{ id: "x", type: "teleport", config: {} }] } as never), {
        kind: "user",
      }),
    ).toThrow(/unknown block type/);
    try {
      validateDefinition(
        def({ blocks: [{ id: "x", type: "create_session", config: {} }] } as never),
        { kind: "user" },
      );
      throw new Error("expected DefinitionError");
    } catch (error) {
      expect(error).toBeInstanceOf(DefinitionError);
      expect((error as DefinitionError).blockId).toBe("x");
    }
  });

  test("system block types are builtin-only", () => {
    const raw = def({
      blocks: [{ id: "x", type: "system.review_policy_gate", config: { reviewId: "r-1" } }],
    } as never);
    expect(() => validateDefinition(raw, { kind: "user" })).toThrow(/reserved for built-in/);
    // A built-in may reference it (phase 4.2 registers the review blocks),
    // and its config schema is enforced like any other block's.
    expect(validateDefinition(raw, { kind: "builtin" }).blocks[0]!.type).toBe(
      "system.review_policy_gate",
    );
    const badConfig = def({
      blocks: [{ id: "x", type: "system.review_policy_gate", config: {} }],
    } as never);
    expect(() => validateDefinition(badConfig, { kind: "builtin" })).toThrow(DefinitionError);
    // An unregistered system type is still unknown for a built-in.
    const unknown = def({ blocks: [{ id: "x", type: "system.not_a_thing", config: {} }] } as never);
    expect(() => validateDefinition(unknown, { kind: "builtin" })).toThrow(/unknown block type/);
  });

  test("rejects invalid Liquid templates with the offending field", () => {
    const raw = def({
      blocks: [
        {
          id: "x",
          type: "create_session",
          config: { profileId: "p", promptTemplate: "${{ event.title | upcase" },
        },
      ],
    } as never);
    try {
      validateDefinition(raw, { kind: "user" });
      throw new Error("expected DefinitionError");
    } catch (error) {
      expect(error).toBeInstanceOf(DefinitionError);
      expect((error as DefinitionError).field).toBe("promptTemplate");
    }
  });

  test("finalize hooks: valid action hooks pass; waits and control blocks are refused", () => {
    const withHook = (block: unknown) =>
      def({
        settings: {
          endSessionsOnFinish: false,
          onFinalize: [{ when: ["failed"], block }],
        },
      } as never);
    const ok = validateDefinition(
      withHook({
        id: "report",
        type: "integration_action",
        config: { provider: "github", actionId: "update_issue_comment", params: { body: "x" } },
      }),
      { kind: "user" },
    );
    expect(ok.settings.onFinalize?.[0]?.block.id).toBe("report");
    // send_prompt parks on the mailbox; a hook may never wait.
    expect(() =>
      validateDefinition(
        withHook({
          id: "nudge",
          type: "send_prompt",
          config: { session: { blockId: "launch" }, promptTemplate: "x", waitFor: { kind: "none" } },
        }),
        { kind: "user" },
      ),
    ).toThrow(/cannot wait/);
    expect(() =>
      validateDefinition(
        withHook({ id: "w", type: "wait_event", config: {} }),
        { kind: "user" },
      ),
    ).toThrow(/cannot wait/);
    expect(() =>
      validateDefinition(
        withHook({ id: "b", type: "branch", config: { conditions: { mode: "all", conditions: [] } }, then: [] }),
        { kind: "user" },
      ),
    ).toThrow(/not allowed in a finalize hook/);
    // Hook ids share the automation's id space.
    expect(() =>
      validateDefinition(
        withHook({
          id: "launch",
          type: "integration_action",
          config: { provider: "github", actionId: "x", params: {} },
        }),
        { kind: "user" },
      ),
    ).toThrow(/duplicate block id/);
  });

  test("branch/loop nesting rules", () => {
    expect(() =>
      validateDefinition(
        def({
          blocks: [{ id: "x", type: "branch", config: { conditions: { mode: "all", conditions: [] } } }],
        } as never),
        { kind: "user" },
      ),
    ).toThrow(/then/);
    expect(() =>
      validateDefinition(
        def({
          blocks: [
            {
              id: "x",
              type: "end_session",
              config: { session: { blockId: "y" } },
              body: [],
            },
          ],
        } as never),
        { kind: "user" },
      ),
    ).toThrow(/only loop blocks/);
  });
});

describe("entrypoints (ADR 0119 D9)", () => {
  const withEntrypoints = (entrypoints: unknown) => def({ entrypoints } as Partial<AutomationDefinition>);
  const sweep = (id = "sweep") => ({
    id,
    trigger: { kind: "manual" },
    blocks: [{ id: `${id}_probe`, type: "session_status", config: { session: { template: "s" } } }],
  });

  test("extra entrypoints validate and round-trip", () => {
    const parsed = validateDefinition(withEntrypoints([sweep()]), { kind: "user" });
    expect(parsed.entrypoints).toHaveLength(1);
    expect(entrypointsOf(parsed).map((e) => e.id)).toEqual(["main", "sweep"]);
    expect(entrypointOf(parsed, "sweep")?.blocks[0]?.id).toBe("sweep_probe");
  });

  test('"main" is the implicit top-level entrypoint and cannot be redeclared', () => {
    expect(() => validateDefinition(withEntrypoints([sweep("main")]), { kind: "user" })).toThrow(
      DefinitionError,
    );
  });

  test("entrypoint ids are unique", () => {
    expect(() =>
      validateDefinition(withEntrypoints([sweep("a"), sweep("a")]), { kind: "user" }),
    ).toThrow(/duplicate entrypoint id/);
  });

  test("block ids stay unique across ALL entrypoints (one steps.* namespace)", () => {
    const clash = {
      id: "other",
      trigger: { kind: "manual" },
      blocks: [{ id: "launch", type: "session_status", config: { session: { template: "s" } } }],
    };
    expect(() => validateDefinition(withEntrypoints([clash]), { kind: "user" })).toThrow(
      /duplicate block id "launch"/,
    );
  });

  test("at most one cron trigger per automation (single next_fire_at)", () => {
    const cronEp = {
      id: "tick",
      trigger: { kind: "cron", schedule: "*/5 * * * *", timezone: "UTC" },
      blocks: [],
    };
    // The main trigger in def() is already cron.
    expect(() => validateDefinition(withEntrypoints([cronEp]), { kind: "user" })).toThrow(
      /at most one cron trigger/,
    );
    // Cron on the extra entrypoint with a non-cron main is fine.
    const manualMain = def({
      trigger: { kind: "manual" },
      entrypoints: [cronEp],
    } as Partial<AutomationDefinition>);
    const parsed = validateDefinition(manualMain, { kind: "user" });
    expect(cronEntrypointOf(parsed)?.id).toBe("tick");
  });

  test("the legacy webhook trigger stays main-only", () => {
    const webhookEp = {
      id: "hook",
      trigger: { kind: "webhook", registrationId: "r", events: ["push"] },
      blocks: [],
    };
    expect(() => validateDefinition(withEntrypoints([webhookEp]), { kind: "user" })).toThrow(
      DefinitionError,
    );
  });

  test("a message-handler cap is per entrypoint, not per definition", () => {
    // Two entrypoints may EACH carry a handler type; a run walks only one.
    // (User graphs cannot register handlers, so assert via the reserved
    // system gate instead: the validator rejects the system type first,
    // proving the walk covers entrypoint blocks.)
    const sysEp = {
      id: "relay",
      trigger: { kind: "manual" },
      blocks: [{ id: "fx", type: "system.slack_thread_relay", config: {} }],
    };
    expect(() => validateDefinition(withEntrypoints([sysEp]), { kind: "user" })).toThrow(
      /reserved for built-in automations/,
    );
  });
});
