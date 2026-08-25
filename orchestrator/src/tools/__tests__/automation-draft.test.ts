import { describe, expect, test } from "bun:test";
import { z } from "zod";

import { compileToolManifest } from "../manifest.ts";
import { createToolRegistry, type ToolContext } from "../registry.ts";
import {
  registerAutomationDraftTools,
  type AutomationDraftToolDeps,
} from "../automation-draft.ts";
import { draftBlockCatalog } from "../../automations/draft-catalog.ts";
import { AUTOMATION_DRAFT_TASK_TYPE } from "../../automations/draft.ts";
import { registerEngineBlocks, V1_BLOCK_TYPES } from "../../automations/engine/blocks/index.ts";
import { SaveVersionConflictError } from "../../db/automations.ts";
import type { AutomationRow } from "../../db/automations.ts";
import type { AutomationDefinition } from "../../automations/engine/definition.ts";

const SESSION = "draft-session-1";

function context(toolName: string, sessionId = SESSION): ToolContext {
  return {
    sessionId,
    userId: "admin-1",
    capabilities: [],
    taskType: AUTOMATION_DRAFT_TASK_TYPE,
    toolCallId: `call-${toolName}`,
    toolName,
  };
}

function definition(overrides: Partial<AutomationDefinition> = {}): AutomationDefinition {
  return {
    engine: 1,
    trigger: { kind: "manual" },
    blocks: [],
    inputsSchema: [],
    settings: { endSessionsOnFinish: false },
    ...overrides,
  };
}

function row(def: AutomationDefinition, version = 1): AutomationRow {
  return {
    id: "auto-1",
    name: "Draft",
    description: "",
    enabled: false,
    kind: "user",
    builtinKey: null,
    currentVersion: version,
    inputs: {},
    blockOverrides: {},
    endSessionsOnFinish: false,
    createdByUserId: "admin-1",
    nextFireAt: null,
    lastFiredAt: null,
    draftSessionId: SESSION,
    createdAt: new Date(0),
    updatedAt: new Date(0),
    archivedAt: null,
    version: {
      automationId: "auto-1",
      version,
      trigger: def.trigger,
      blocks: def.blocks,
      entrypoints: def.entrypoints ?? [],
      inputsSchema: def.inputsSchema,
      settings: def.settings,
      createdByUserId: null,
      createdAt: new Date(0),
    },
  };
}

interface Harness {
  deps: AutomationDraftToolDeps;
  saved: Array<{ automationId: string; definition: AutomationDefinition }>;
  meta: Array<Record<string, unknown>>;
}

function harness(current: AutomationRow | null, options: { raceToVersion?: number } = {}): Harness {
  const saved: Harness["saved"] = [];
  const meta: Harness["meta"] = [];
  const deps: AutomationDraftToolDeps = {
    store: {
      async getByDraftSession(sessionId) {
        return current && current.draftSessionId === sessionId ? current : null;
      },
      async saveVersion(automationId, def, _user, _meta, opts) {
        // The in-transaction fence: the row may have advanced past the
        // handler's read (a concurrent human save).
        const liveVersion = options.raceToVersion ?? current?.currentVersion ?? 1;
        if (opts?.expectedVersion !== undefined && opts.expectedVersion !== liveVersion) {
          throw new SaveVersionConflictError(liveVersion);
        }
        saved.push({ automationId, definition: def });
        return current;
      },
      async updateMeta(_id, patch) {
        meta.push(patch as Record<string, unknown>);
        return current;
      },
      async list() {
        return [
          row(definition({ trigger: { kind: "cron", schedule: "0 9 * * *", timezone: "UTC" } })),
        ].map((r) => ({ ...r, id: "other-1", name: "Existing", draftSessionId: null }));
      },
    },
    profiles: {
      async getActive(id) {
        return id === "prof-1" ? ({ id: "prof-1" } as never) : null;
      },
      async list() {
        return [
          { id: "prof-1", name: "CI", description: "", repos: [] } as never,
        ];
      },
    },
    events: {
      connectors: { list: async () => [] },
      connections: { getDefault: async () => null },
      integrationEvents: {
        getLatest: async () => null,
        listObservedEventKeys: async () => [],
      },
    },
    now: () => new Date("2026-08-25T00:00:00Z"),
  };
  return { deps, saved, meta };
}

async function call(deps: AutomationDraftToolDeps, toolName: string, input: object) {
  const registry = createToolRegistry();
  registerAutomationDraftTools(registry, deps);
  const tool = registry.get(toolName);
  if (tool?.handling !== "handled") throw new Error(`${toolName} is not a handled tool`);
  return tool.handler(context(toolName), tool.input.parse(input) as never);
}

describe("automation draft tools", () => {
  test("the family is gated to the draft task type with flat object schemas", () => {
    const registry = createToolRegistry();
    registerAutomationDraftTools(registry, harness(null).deps);
    const manifest = compileToolManifest(registry, [], AUTOMATION_DRAFT_TASK_TYPE);
    const names = manifest.map((t) => t.name).sort();
    expect(names).toEqual([
      "automation_propose",
      "automation_read",
      "automation_set_meta",
      "automation_test",
    ]);
    for (const tool of manifest) {
      // One anyOf at the top level kills the whole tools/list (MCP).
      expect((tool.inputSchema as { type?: string }).type).toBe("object");
      expect("anyOf" in (tool.inputSchema as object)).toBe(false);
    }
    // Not visible to other task types.
    expect(compileToolManifest(registry, [], "chat").map((t) => t.name)).toEqual([]);
  });

  test("a session with no bound draft is refused", async () => {
    const h = harness(null);
    await expect(call(h.deps, "automation_read", { part: "draft" })).rejects.toThrow(
      /not bound to a draft automation/,
    );
    await expect(
      call(h.deps, "automation_propose", {
        definition_json: JSON.stringify(definition()),
        expected_version: 1,
      }),
    ).rejects.toThrow(/not bound/);
  });

  test("propose validates and saves; validation failures are soft refusals with addresses", async () => {
    const h = harness(row(definition()));
    const good = definition({
      blocks: [
        {
          id: "launch",
          type: "create_session",
          config: { profileId: "prof-1", promptTemplate: "go" },
        },
      ],
    });
    const ok = (await call(h.deps, "automation_propose", {
      definition_json: JSON.stringify(good),
      expected_version: 1,
    })) as { applied: boolean; new_version?: number };
    expect(ok.applied).toBe(true);
    expect(ok.new_version).toBe(2);
    expect(h.saved).toHaveLength(1);

    const bad = (await call(h.deps, "automation_propose", {
      definition_json: JSON.stringify(
        definition({ blocks: [{ id: "x", type: "no_such_block", config: {} }] }),
      ),
      expected_version: 1,
    })) as { applied: boolean; errors?: Array<{ block_id: string; field: string }> };
    expect(bad.applied).toBe(false);
    expect(bad.errors?.[0]).toMatchObject({ block_id: "x", field: "type" });
    expect(h.saved).toHaveLength(1); // nothing extra saved

    const junk = (await call(h.deps, "automation_propose", {
      definition_json: "{not json",
      expected_version: 1,
    })) as { applied: boolean; errors?: Array<{ field: string }> };
    expect(junk.applied).toBe(false);
    expect(junk.errors?.[0]?.field).toBe("definition_json");
  });

  test("an unknown profile is a soft refusal pointing at the block", async () => {
    const h = harness(row(definition()));
    const result = (await call(h.deps, "automation_propose", {
      definition_json: JSON.stringify(
        definition({
          blocks: [
            {
              id: "launch",
              type: "create_session",
              config: { profileId: "ghost", promptTemplate: "go" },
            },
          ],
        }),
      ),
      expected_version: 1,
    })) as { applied: boolean; errors?: Array<{ block_id: string; field: string }> };
    expect(result.applied).toBe(false);
    expect(result.errors?.[0]).toMatchObject({ block_id: "launch", field: "profileId" });
  });

  test("the version fence returns the live definition instead of clobbering", async () => {
    const h = harness(row(definition(), 4));
    const result = (await call(h.deps, "automation_propose", {
      definition_json: JSON.stringify(definition()),
      expected_version: 3,
    })) as {
      applied: boolean;
      current_version?: number;
      definition_json?: string;
    };
    expect(result.applied).toBe(false);
    expect(result.current_version).toBe(4);
    expect(JSON.parse(result.definition_json!)).toMatchObject({ engine: 1 });
    expect(h.saved).toHaveLength(0);
  });

  test("a human save landing DURING the propose trips the in-transaction fence", async () => {
    // The pre-check passes (the handler read version 1), but the store's
    // FOR UPDATE sees version 2 — the race the fence exists for.
    const h = harness(row(definition(), 1), { raceToVersion: 2 });
    const result = (await call(h.deps, "automation_propose", {
      definition_json: JSON.stringify(definition()),
      expected_version: 1,
    })) as { applied: boolean; current_version?: number; definition_json?: string };
    expect(result.applied).toBe(false);
    expect(result.current_version).toBe(2);
    expect(result.definition_json).toBeDefined();
    expect(h.saved).toHaveLength(0);
  });

  test("read: draft, profiles, org_automations, and the block catalog", async () => {
    const h = harness(row(definition()));
    const draft = (await call(h.deps, "automation_read", { part: "draft" })) as {
      content: { version: number; definition: AutomationDefinition };
    };
    expect(draft.content.version).toBe(1);
    expect(draft.content.definition.trigger.kind).toBe("manual");

    const profiles = (await call(h.deps, "automation_read", { part: "profiles" })) as {
      content: Array<{ id: string }>;
    };
    expect(profiles.content[0]?.id).toBe("prof-1");

    const org = (await call(h.deps, "automation_read", { part: "org_automations" })) as {
      content: Array<{ name: string; definition: AutomationDefinition }>;
    };
    expect(org.content[0]).toMatchObject({
      name: "Existing",
      definition: { engine: 1, trigger: { kind: "cron" } },
    });
    // The FULL graph rides along, not just the trigger.
    expect(Array.isArray(org.content[0]?.definition.blocks)).toBe(true);
    expect(org.content[0]?.definition.settings).toBeDefined();
  });

  test("automation_test renders the draft against a payload, side-effect free", async () => {
    const h = harness(
      row(
        definition({
          blocks: [
            {
              id: "note",
              type: "state_get",
              config: { key: "ticket:${{ event.raw.id }}" },
            },
          ],
        }),
      ),
    );
    const result = (await call(h.deps, "automation_test", {
      payload_json: JSON.stringify({ id: "ENG-1" }),
    })) as { blocks: Array<{ block_id: string; rendered_json: string }>; errors: unknown[] };
    expect(result.errors).toEqual([]);
    expect(result.blocks[0]?.block_id).toBe("note");
    expect(result.blocks[0]?.rendered_json).toContain("ticket:ENG-1");
  });

  test("set_meta names the draft", async () => {
    const h = harness(row(definition()));
    await call(h.deps, "automation_set_meta", { name: "PR test sentinel" });
    expect(h.meta[0]).toEqual({ name: "PR test sentinel" });
  });
});

describe("automation_read patterns", () => {
  test("the patterns part teaches the multi-entrypoint idioms", async () => {
    const h = harness(null);
    const result = (await call(h.deps, "automation_read", { part: "patterns" })) as {
      content: Record<string, unknown>;
    };
    for (const key of [
      "core_model",
      "state",
      "kept_sessions",
      "slack_clarification_thread",
      "pr_feedback_loop",
      "delegation",
      "cron_heartbeat",
    ]) {
      expect(Array.isArray(result.content[key]), key).toBe(true);
    }
  });
});

describe("draftBlockCatalog", () => {
  test("golden: every user block type projects with a real JSON schema", () => {
    registerEngineBlocks();
    const catalog = draftBlockCatalog() as {
      blocks: Array<{ type: string; config_schema: unknown }>;
    };
    // A registry change (new block, renamed type) must be a visible diff here.
    expect(catalog.blocks.map((b) => b.type).sort()).toEqual([...V1_BLOCK_TYPES].sort());
    for (const block of catalog.blocks) {
      const schema = block.config_schema as { type?: string; description?: string };
      expect(
        schema.type === "object" || typeof schema.description === "string",
        `block ${block.type} has no usable schema projection`,
      ).toBe(true);
    }
    const parsed = z
      .object({ trigger_schema: z.unknown(), settings_schema: z.unknown() })
      .safeParse(catalog);
    expect(parsed.success).toBe(true);
  });
});
