import { z } from "zod";
import { describe, expect, test } from "bun:test";

import { registerEngineBlocks } from "../blocks/index.ts";
import { registerBlock, unregisterBlockForTest } from "../blocks/registry.ts";
import {
  concurrencySettingsOf,
  cronEntrypointOf,
  declaredEntrypointsOf,
  DefinitionError,
  entrypointOf,
  entrypointsOf,
  instanceSettingsOf,
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
    const parsed = validateDefinition(def());
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
    expect(validateDefinition(integration(["message"], "join")).trigger).toMatchObject({
      continueOnly: ["message"],
    });
    expect(() => validateDefinition(integration(["reaction_added"], "join"))).toThrow(
      /continueOnly event "reaction_added" is not one of the trigger's eventKeys/,
    );
    expect(() => validateDefinition(integration(["message"], "queue"))).toThrow(
      /continueOnly needs settings.concurrency.policy "join"/,
    );
    expect(() => validateDefinition(integration(["message"]))).toThrow(
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
    expect(() => validateDefinition(raw)).toThrow(/duplicate block id/);
  });

  test("rejects unknown block types and bad configs with a block+field address", () => {
    expect(() =>
      validateDefinition(def({ blocks: [{ id: "x", type: "teleport", config: {} }] } as never)),
    ).toThrow(/unknown block type/);
    try {
      validateDefinition(
        def({ blocks: [{ id: "x", type: "create_session", config: {} }] } as never));
      throw new Error("expected DefinitionError");
    } catch (error) {
      expect(error).toBeInstanceOf(DefinitionError);
      expect((error as DefinitionError).blockId).toBe("x");
    }
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
      validateDefinition(raw);
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
      }));
    expect(ok.settings.onFinalize?.[0]?.block.id).toBe("report");
    // send_prompt parks on the mailbox; a hook may never wait.
    expect(() =>
      validateDefinition(
        withHook({
          id: "nudge",
          type: "send_prompt",
          config: { session: { blockId: "launch" }, promptTemplate: "x", waitFor: { kind: "none" } },
        })),
    ).toThrow(/cannot wait/);
    expect(() =>
      validateDefinition(
        withHook({ id: "w", type: "wait_event", config: {} })),
    ).toThrow(/cannot wait/);
    expect(() =>
      validateDefinition(
        withHook({ id: "b", type: "branch", config: { conditions: { mode: "all", conditions: [] } }, then: [] })),
    ).toThrow(/not allowed in a finalize hook/);
    // Hook ids share the automation's id space.
    expect(() =>
      validateDefinition(
        withHook({
          id: "launch",
          type: "integration_action",
          config: { provider: "github", actionId: "x", params: {} },
        })),
    ).toThrow(/duplicate block id/);
  });

  test("branch/loop nesting rules", () => {
    expect(() =>
      validateDefinition(
        def({
          blocks: [{ id: "x", type: "branch", config: { conditions: { mode: "all", conditions: [] } } }],
        } as never)),
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
        } as never)),
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
    const parsed = validateDefinition(withEntrypoints([sweep()]));
    expect(parsed.entrypoints).toHaveLength(1);
    expect(entrypointsOf(parsed).map((e) => e.id)).toEqual(["main", "sweep"]);
    expect(entrypointOf(parsed, "sweep")?.blocks[0]?.id).toBe("sweep_probe");
  });

  test('"main" is the implicit top-level entrypoint and cannot be redeclared', () => {
    expect(() => validateDefinition(withEntrypoints([sweep("main")]))).toThrow(
      DefinitionError,
    );
  });

  test("entrypoint ids are unique", () => {
    expect(() =>
      validateDefinition(withEntrypoints([sweep("a"), sweep("a")])),
    ).toThrow(/duplicate entrypoint id/);
  });

  test("block ids stay unique across ALL entrypoints (one steps.* namespace)", () => {
    const clash = {
      id: "other",
      trigger: { kind: "manual" },
      blocks: [{ id: "launch", type: "session_status", config: { session: { template: "s" } } }],
    };
    expect(() => validateDefinition(withEntrypoints([clash]))).toThrow(
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
    expect(() => validateDefinition(withEntrypoints([cronEp]))).toThrow(
      /at most one cron trigger/,
    );
    // Cron on the extra entrypoint with a non-cron main is fine.
    const manualMain = def({
      trigger: { kind: "manual" },
      entrypoints: [cronEp],
    } as Partial<AutomationDefinition>);
    const parsed = validateDefinition(manualMain);
    expect(cronEntrypointOf(parsed)?.id).toBe("tick");
  });

  test("the legacy webhook trigger stays main-only", () => {
    const webhookEp = {
      id: "hook",
      trigger: { kind: "webhook", registrationId: "r", events: ["push"] },
      blocks: [],
    };
    expect(() => validateDefinition(withEntrypoints([webhookEp]))).toThrow(
      DefinitionError,
    );
  });
});

describe("settings.instance (ADR 0120)", () => {
  // Raw fixture, deliberately outside the parsed types: validateDefinition
  // re-parses from unknown, and several cases pass invalid shapes on purpose.
  const instanced = (instance: Record<string, unknown>, extra: Record<string, unknown> = {}) =>
    def({
      settings: { endSessionsOnFinish: false, instance } as AutomationDefinition["settings"],
      ...(extra as Partial<AutomationDefinition>),
    });

  test("accepts a key template with per-entrypoint admission and input templates", () => {
    const parsed = validateDefinition(
      instanced(
        {
          keyTemplate: "project-${{ inputs.project_id }}",
          inputs: { project_id: "${{ event.raw.project.id }}" },
          entrypoints: { main: { admit: "open" } },
        },
        {
          inputsSchema: [{ key: "project_id", label: "Project", type: "string" }],
        },
      ));
    expect(parsed.settings.instance?.keyTemplate).toContain("project-");
  });

  test("rejects an instance input that is not an inputsSchema field", () => {
    expect(() =>
      validateDefinition(
        instanced({ keyTemplate: "k", inputs: { ghost: "${{ event.raw.x }}" } })),
    ).toThrow('not an inputsSchema field');
  });

  test("rejects admission for an unknown entrypoint", () => {
    expect(() =>
      validateDefinition(
        instanced({ keyTemplate: "k", entrypoints: { ghost: { admit: "open" } } })),
    ).toThrow('unknown entrypoint "ghost"');
  });

  test("handle_match needs a delivery-bearing trigger", () => {
    expect(() =>
      validateDefinition(
        instanced({ keyTemplate: "k", entrypoints: { main: { admit: "handle_match" } } })),
    ).toThrow("delivery-bearing trigger");
    const parsed = validateDefinition(
      instanced(
        { keyTemplate: "k", entrypoints: { main: { admit: "handle_match" } } },
        {
          trigger: {
            kind: "integration",
            provider: "slack",
            connectionId: "c1",
            eventKeys: ["message"],
          },
        },
      ));
    expect(parsed.settings.instance?.entrypoints?.main?.admit).toBe("handle_match");
  });

  test("a bad key template fails at save time", () => {
    expect(() =>
      validateDefinition(instanced({ keyTemplate: "${{ unclosed" })),
    ).toThrow();
  });
});

describe("the agent: an automation is an agent with ways in", () => {
  const THREAD_KEY =
    '${{ event.raw.team_id }}:${{ event.raw.event.channel }}:${{ event.raw.event | coalesce: "thread_ts", "ts" }}';
  const slack = (eventKeys: string[]) => ({
    kind: "integration",
    provider: "slack",
    connectionId: "c1",
    eventKeys,
  });
  const admit = {
    id: "admit",
    type: "filter",
    config: { conditions: { mode: "all", conditions: [{ path: "event.raw.event.text", op: "is_true" }] } },
  };
  /** A Slack answer bot: a mention starts the thread's agent, a reply
   * reaches it, a reaction closes it. */
  const bot = (extra: Record<string, unknown> = {}) =>
    def({
      trigger: slack(["app_mention"]),
      blocks: [admit],
      entrypoints: [
        { id: "reply", trigger: slack(["message"]), role: "reach", blocks: [] },
        { id: "done", trigger: slack(["reaction_added"]), role: "close", blocks: [] },
      ],
      agent: {
        scope: { kind: "key", keyTemplate: THREAD_KEY, labelTemplate: "${{ event.raw.event.text }}" },
        profileId: "${{ inputs.profile }}",
        promptTemplate: "${{ event.raw.event.text }}",
        openingPromptTemplate: "The thread so far: ${{ event.raw.event.text }}",
      },
      ...extra,
    } as Partial<AutomationDefinition>);

  test("every way in ends on the agent step its role names; the editor sees only what was written", () => {
    const parsed = validateDefinition(bot());
    const ways = entrypointsOf(parsed);
    expect(ways.map((w) => w.blocks.map((b) => `${b.id}:${b.type}`))).toEqual([
      ["admit:filter", "agent:run_agent"],
      ["agent:run_agent"],
      ["agent_close:close_agent"],
    ]);
    // The scope decides reuse; the opening prompt rides along.
    expect(ways[0]!.blocks[1]!.config).toMatchObject({
      reuse: "workstream",
      profileId: "${{ inputs.profile }}",
      openingPromptTemplate: "The thread so far: ${{ event.raw.event.text }}",
    });
    expect(ways[0]!.blocks[1]!.config).not.toHaveProperty("scope");
    expect(declaredEntrypointsOf(parsed).map((w) => w.blocks.length)).toEqual([1, 0, 0]);
    expect(entrypointOf(parsed, "reply")?.blocks[0]?.id).toBe("agent");
  });

  test("the workstream settings derive from the scope and the roles", () => {
    const parsed = validateDefinition(bot());
    expect(parsed.settings.instance).toBeUndefined();
    expect(instanceSettingsOf(parsed)).toEqual({
      keyTemplate: THREAD_KEY,
      labelTemplate: "${{ event.raw.event.text }}",
      entrypoints: { reply: { admit: "require" }, done: { admit: "require" } },
    });
    const whole = validateDefinition(
      def({
        agent: { scope: { kind: "automation" }, profileId: "p1", promptTemplate: "go" },
      } as Partial<AutomationDefinition>),
    );
    expect(instanceSettingsOf(whole)).toEqual({ keyTemplate: "agent" });
    const each = validateDefinition(
      def({ agent: { scope: { kind: "event" }, profileId: "p1", promptTemplate: "go" } } as Partial<AutomationDefinition>),
    );
    expect(instanceSettingsOf(each)).toBeUndefined();
    expect(entrypointsOf(each)[0]!.blocks.at(-1)!.config).toMatchObject({ reuse: "never" });
  });

  test("a block automation keeps its declared workstream settings", () => {
    const parsed = validateDefinition(
      def({
        settings: { endSessionsOnFinish: false, instance: { keyTemplate: "k" } },
      } as Partial<AutomationDefinition>),
    );
    expect(instanceSettingsOf(parsed)).toEqual({ keyTemplate: "k" });
    expect(entrypointsOf(parsed)[0]!.blocks.map((b) => b.id)).toEqual(["launch"]);
  });

  test("the agent's config validates as the executor's, addressed to the agent", () => {
    expect(() =>
      validateDefinition(bot({ agent: { scope: { kind: "event" }, profileId: "", promptTemplate: "x" } })),
    ).toThrow(DefinitionError);
    try {
      validateDefinition(bot({ agent: { scope: { kind: "event" }, profileId: "", promptTemplate: "x" } }));
    } catch (error) {
      expect(error).toMatchObject({ blockId: "__agent__", field: "profileId" });
    }
    expect(() =>
      validateDefinition(
        bot({
          agent: { scope: { kind: "key", keyTemplate: "${{ unclosed" }, profileId: "p", promptTemplate: "x" },
        }),
      ),
    ).toThrow();
    expect(() =>
      validateDefinition(
        bot({ agent: { scope: { kind: "key", keyTemplate: "k" }, profileId: "p", promptTemplate: "{{ nope }}" } }),
      ),
    ).toThrow(/never rendered/);
  });

  test("a way in with no role never opens the agent's workstream, and the agent's runs queue per workstream", () => {
    const parsed = validateDefinition(
      bot({
        entrypoints: [
          { id: "reply", trigger: slack(["message"]), role: "reach", blocks: [] },
          { id: "log", trigger: slack(["reaction_added"]), role: "none", blocks: [{ ...admit, id: "log_admit" }] },
        ],
      }),
    );
    expect(instanceSettingsOf(parsed)?.entrypoints).toEqual({
      reply: { admit: "require" },
      log: { admit: "optional" },
    });
    expect(entrypointOf(parsed, "log")?.blocks.map((b) => b.id)).toEqual(["log_admit"]);
    // One run at a time per thread, unless the definition says otherwise.
    expect(concurrencySettingsOf(parsed)).toEqual({ keyTemplate: "agent", policy: "queue" });
    expect(
      concurrencySettingsOf(
        validateDefinition(bot({ settings: { endSessionsOnFinish: false, concurrency: { keyTemplate: "k", policy: "skip" } } })),
      ),
    ).toEqual({ keyTemplate: "k", policy: "skip" });
    expect(
      concurrencySettingsOf(
        validateDefinition(def({ agent: { scope: { kind: "event" }, profileId: "p1", promptTemplate: "go" } } as Partial<AutomationDefinition>)),
      ),
    ).toBeUndefined();
  });

  test("an agent that keeps its session refuses endSessionsOnFinish", () => {
    expect(() => validateDefinition(bot({ settings: { endSessionsOnFinish: true } }))).toThrow(
      /keeps its session between events/,
    );
    try {
      validateDefinition(bot({ settings: { endSessionsOnFinish: true } }));
    } catch (error) {
      expect(error).toMatchObject({ blockId: "__settings__", field: "endSessionsOnFinish" });
    }
    // A session per event may end with the run.
    const each = validateDefinition(
      def({
        agent: { scope: { kind: "event" }, profileId: "p1", promptTemplate: "go" },
        settings: { endSessionsOnFinish: true },
      } as Partial<AutomationDefinition>),
    );
    expect(each.settings.endSessionsOnFinish).toBe(true);
  });

  test("a schema issue in the agent or a role is addressed to the agent or the way in", () => {
    const expectError = (raw: unknown, where: { blockId: string | null; field: string }) => {
      try {
        validateDefinition(raw);
        throw new Error("validated");
      } catch (error) {
        expect(error).toMatchObject(where);
      }
    };
    expectError(
      bot({ agent: { scope: { kind: "key", keyTemplate: "" }, profileId: "p", promptTemplate: "x" } }),
      { blockId: "__agent__", field: "scope.keyTemplate" },
    );
    expectError(bot({ agent: { scope: { kind: "nope" }, profileId: "p", promptTemplate: "x" } }), {
      blockId: "__agent__",
      field: "scope.kind",
    });
    expectError(bot({ role: "sometimes" as unknown as AutomationDefinition["role"] }), { blockId: "__trigger__", field: "role" });
    expectError(
      bot({ entrypoints: [{ id: "reply", trigger: slack(["message"]), role: "sometimes", blocks: [] }] }),
      { blockId: "__entrypoints__", field: "role" },
    );
  });

  test("an agent automation never also declares settings.instance", () => {
    expect(() =>
      validateDefinition(
        bot({ settings: { endSessionsOnFinish: false, instance: { keyTemplate: "k" } } }),
      ),
    ).toThrow(/derived from the agent's scope/);
  });

  test("run_agent is not a block, and the agent step's ids are reserved", () => {
    expect(() =>
      validateDefinition(
        def({ blocks: [{ id: "a", type: "run_agent", config: { profileId: "p", promptTemplate: "x" } }] }),
      ),
    ).toThrow(/is not a block/);
    expect(() =>
      validateDefinition(
        bot({ blocks: [{ id: "agent", type: "filter", config: { conditions: { mode: "all", conditions: [] } } }] }),
      ),
    ).toThrow(/is the agent step's/);
    // Without an agent the id is an ordinary one.
    expect(
      validateDefinition(
        def({ blocks: [{ id: "agent", type: "filter", config: { conditions: { mode: "all", conditions: [] } } }] }),
      ).blocks[0]!.id,
    ).toBe("agent");
    // A finalize hook is held to the same rules.
    const hook = (block: Record<string, unknown>) =>
      bot({ settings: { endSessionsOnFinish: false, onFinalize: [{ when: ["failed"], block }] } });
    expect(() =>
      validateDefinition(hook({ id: "h", type: "close_agent", config: {} })),
    ).toThrow(/is not a block/);
    expect(() =>
      validateDefinition(hook({ id: "agent_close", type: "state_delete", config: { key: "k" } })),
    ).toThrow(/is the agent step's/);
  });

  test("a role needs an agent, and reach/close need a standing one", () => {
    expect(() => validateDefinition(def({ role: "reach" } as Partial<AutomationDefinition>))).toThrow(
      /has a role but the automation has no agent/,
    );
    expect(() =>
      validateDefinition(
        bot({ agent: { scope: { kind: "event" }, profileId: "p", promptTemplate: "x" } }),
      ),
    ).toThrow(/cannot reach an agent scoped to each event/);
  });
});

describe("the {{ }} delimiter guard", () => {
  test("plain {{ }} in a rendered field is refused with the fix in the message", () => {
    expect(() =>
      validateDefinition(
        def({
          blocks: [
            {
              id: "launch",
              type: "create_session",
              config: { profileId: "p1", promptTemplate: "Do {{ inputs.thing }}" },
            },
          ],
        })),
    ).toThrow(/never rendered/);
  });

  test("a session {template} ref carries the full template contract", () => {
    const withSessionRef = (template: string) =>
      def({
        blocks: [
          {
            id: "nudge",
            type: "send_prompt",
            config: {
              session: { template },
              promptTemplate: "hello",
              waitFor: { kind: "none" },
            },
          },
        ],
      });
    expect(() => validateDefinition(withSessionRef("{{ steps.x.value }}"))).toThrow(
      /never rendered/,
    );
    const parsed = validateDefinition(withSessionRef("${{ steps.x.value }}"));
    expect(parsed.blocks[0]!.type).toBe("send_prompt");
  });

  test("JS source and condition data keep their braces; the raw-literal escape renders", () => {
    const parsed = validateDefinition(
      def({
        blocks: [
          {
            id: "facts",
            type: "code",
            config: { mode: "value", source: "export default () => {{ nested: true }};" },
          },
          {
            id: "gate",
            type: "filter",
            config: {
              conditions: {
                mode: "all",
                conditions: [{ path: "event.raw.text", op: "equals", value: "{{ literal }}" }],
              },
            },
          },
          {
            id: "launch",
            type: "create_session",
            config: { profileId: "p1", promptTemplate: "brace via ${{ '{{' }} escape" },
          },
        ],
      }));
    expect(parsed.blocks).toHaveLength(3);
  });
});
