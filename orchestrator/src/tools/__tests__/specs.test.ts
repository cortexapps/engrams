import { describe, expect, test } from "bun:test";

import { compileToolManifest } from "../manifest.ts";
import { createToolRegistry, type ToolContext } from "../registry.ts";
import {
  registerSpecTools,
  type SpecMutationResult,
  type SpecToolDeps,
  type SpecToolDocumentService,
} from "../specs.ts";

const SPEC_ID = "019fe2ff-0464-75f3-bb20-a8c1844579b9";

function context(toolName: string): ToolContext {
  return {
    sessionId: "019fe2ff-0464-75f3-bb20-a8c1844579b8",
    userId: "user-1",
    capabilities: [],
    toolCallId: `call-${toolName}`,
    toolName,
  };
}

function documentService(
  mutation: (name: string, input: object) => Promise<SpecMutationResult>,
): SpecToolDocumentService {
  return {
    read: async (specId, sectionId) => ({
      specId,
      rev: 8n,
      markdown: sectionId === undefined ? "# Live revision 8" : "Live section revision 8",
      ...(sectionId === undefined ? {} : { sectionId }),
    }),
    updateSection: async (_specId, input) => mutation("updateSection", input),
    setSectionState: async (_specId, input) => mutation("setSectionState", input),
    addOpenQuestion: async (_specId, input) => mutation("addOpenQuestion", input),
    resolveOpenQuestion: async (_specId, input) => mutation("resolveOpenQuestion", input),
    updateBlock: async (_specId, input) => mutation("updateBlock", input),
    proposeAlternatives: async (_specId, input) => ({
      ...(await mutation("proposeAlternatives", input)),
      setId: "set-1",
    }),
    decideAlternative: async (_specId, input) => mutation("decideAlternative", input),
    updateNotes: async (_specId, input) => mutation("updateNotes", input),
    proposeTickets: async (_specId, input) => mutation("proposeTickets", input),
  };
}

function recorder(
  result: SpecMutationResult = {
    applied: true,
    newRev: 8n,
    concurrentEditors: ["Ari", "Sam"],
  },
) {
  const mutations: Array<{ name: string; input: object }> = [];
  const refreshes: object[] = [];
  const presence: Array<{ action: "enter" | "leave"; input: object }> = [];
  const deps: SpecToolDeps = {
    resolveSpecForSession: async () => ({ id: SPEC_ID }),
    documents: documentService(async (name, input) => {
      mutations.push({ name, input });
      return result;
    }),
    projection: {
      request: async (input) => {
        refreshes.push(input);
      },
    },
    presence: {
      enter: async (input) => {
        presence.push({ action: "enter", input });
      },
      leave: async (input) => {
        presence.push({ action: "leave", input });
      },
    },
  };
  return { deps, mutations, refreshes, presence };
}

async function call(deps: SpecToolDeps, toolName: string, input: object): Promise<unknown> {
  const registry = createToolRegistry();
  registerSpecTools(registry, deps);
  const tool = registry.get(toolName);
  if (tool?.handling !== "handled") throw new Error(`${toolName} is not a handled tool`);
  return tool.handler(context(toolName), tool.input.parse(input));
}

describe("spec tools", () => {
  test("registers the complete family as handled synchronous tools with flat schemas", () => {
    const registry = createToolRegistry();
    registerSpecTools(registry, recorder().deps);

    expect(compileToolManifest(registry)).toEqual([]);
    const manifest = compileToolManifest(registry, undefined, "spec");
    expect(manifest.map((tool) => tool.name)).toEqual([
      "spec_read",
      "spec_update_section",
      "spec_set_section_state",
      "spec_add_open_question",
      "spec_resolve_open_question",
      "spec_update_block",
      "spec_propose_alternatives",
      "spec_decide_alternative",
      "spec_update_notes",
      "spec_propose_tickets",
    ]);
    for (const tool of registry.all()) {
      expect(tool).toMatchObject({ handling: "handled", execution: "sync" });
    }
    for (const tool of manifest) {
      expect(
        (tool.inputSchema as { type?: string }).type,
        `tool ${tool.name} must emit a type:"object" inputSchema`,
      ).toBe("object");
      expect(JSON.stringify(tool.inputSchema)).not.toContain('"anyOf"');
    }
  });

  test("spec_read uses the live service instead of the last projection", async () => {
    const state = recorder();
    const result = await call(state.deps, "spec_read", {});

    expect(result).toEqual({
      spec_id: SPEC_ID,
      rev: "8",
      markdown: "# Live revision 8",
    });
    expect(result).not.toEqual({
      rev: 7,
      markdown: "# Published projection revision 7",
    });
    expect(state.refreshes).toHaveLength(0);
  });

  test("an expected revision mismatch does not mutate or request a refresh", async () => {
    const state = recorder();
    state.deps.documents.updateSection = async () => {
      return { applied: false, newRev: 9n, concurrentEditors: ["Ari"] };
    };

    const result = await call(state.deps, "spec_update_section", {
      section_id: "failure-modes",
      markdown: "replacement",
      expected_rev: "8",
    });

    expect(result).toEqual({
      applied: false,
      new_rev: "9",
      concurrent_editors: ["Ari"],
    });
    expect(state.refreshes).toHaveLength(0);
  });

  test("an applied mutation requests exactly one projection refresh", async () => {
    const state = recorder();
    const result = await call(state.deps, "spec_update_section", {
      section_id: "failure-modes",
      markdown: "New failure modes",
      expected_rev: "7",
    });

    expect(result).toEqual({
      applied: true,
      new_rev: "8",
      concurrent_editors: ["Ari", "Sam"],
    });
    expect(state.refreshes).toEqual([
      {
        specId: SPEC_ID,
        sessionId: context("spec_update_section").sessionId,
        source: "agent-tool-mutation",
      },
    ]);
  });

  test("a selection-scoped update carries every span field to the document service", async () => {
    const state = recorder({
      applied: true,
      newRev: 9n,
      concurrentEditors: [],
      transcriptChip: {
        kind: "spec_tracked_edit",
        specId: SPEC_ID,
        sectionId: "failure-modes",
        before: "retry forever",
        after: "retry three times",
      },
    });
    const result = await call(state.deps, "spec_update_section", {
      section_id: "failure-modes",
      markdown: "retry three times",
      selection_start: "yjs-section://failure-modes/0102",
      selection_end: "yjs-section://failure-modes/0304",
      selection_text: "retry forever",
      selection_spec_id: SPEC_ID,
      selection_revision: "8",
      selection_fingerprint: "a".repeat(64),
    });

    expect(state.mutations[0]).toMatchObject({
      name: "updateSection",
      input: {
        sectionId: "failure-modes",
        markdown: "retry three times",
        selection: {
          specId: SPEC_ID,
          sectionId: "failure-modes",
          revision: "8",
          startAnchor: "yjs-section://failure-modes/0102",
          endAnchor: "yjs-section://failure-modes/0304",
          selectedText: "retry forever",
          sliceFingerprint: "a".repeat(64),
        },
      },
    });
    expect(result).toMatchObject({
      applied: true,
      transcript_chip: {
        kind: "spec_tracked_edit",
        before: "retry forever",
      },
    });
  });

  test("selection fields must be complete without adding a union to the schema", () => {
    const registry = createToolRegistry();
    registerSpecTools(registry, recorder().deps);
    const tool = registry.get("spec_update_section");
    if (!tool) throw new Error("spec_update_section is not registered");

    expect(() =>
      tool.input.parse({
        section_id: "context",
        markdown: "replacement",
        selection_start: "yjs-section://context/01",
      }),
    ).toThrow();
    const manifest = compileToolManifest(registry, undefined, "spec").find(
      (entry) => entry.name === "spec_update_section",
    );
    expect(JSON.stringify(manifest?.inputSchema)).not.toContain('"anyOf"');
  });

  test("an applied block mutation returns its checkpoint id", async () => {
    const checkpointId = "00000000-0000-4000-8000-000000001124";
    const state = recorder({
      applied: true,
      newRev: 9n,
      concurrentEditors: ["Ari"],
      checkpointId,
    });

    const result = await call(state.deps, "spec_update_block", {
      section_id: "design",
      block_id: "request-flow",
      source: "flowchart LR\nA --> B",
    });

    expect(result).toEqual({
      applied: true,
      new_rev: "9",
      concurrent_editors: ["Ari"],
      checkpoint_id: checkpointId,
    });
  });

  test("an applied mutation waits for its projection refresh", async () => {
    const state = recorder();
    let release = () => {};
    state.deps.projection.request = () =>
      new Promise<void>((resolve) => {
        release = resolve;
      });
    let settled = false;
    const pending = call(state.deps, "spec_update_section", {
      section_id: "failure-modes",
      markdown: "New failure modes",
    }).then(() => {
      settled = true;
    });

    await Bun.sleep(0);
    expect(settled).toBe(false);
    release();
    await pending;
    expect(settled).toBe(true);
  });

  test("a section tool publishes section presence and clears it after failure", async () => {
    const state = recorder();
    state.deps.documents.updateSection = async () => {
      throw new Error("document update failed");
    };

    await expect(
      call(state.deps, "spec_update_section", {
        section_id: "failure-modes",
        markdown: "New failure modes",
      }),
    ).rejects.toThrow("document update failed");
    expect(state.presence).toEqual([
      {
        action: "enter",
        input: {
          specId: SPEC_ID,
          sessionId: context("spec_update_section").sessionId,
          toolCallId: "call-spec_update_section",
          sectionId: "failure-modes",
        },
      },
      {
        action: "leave",
        input: {
          specId: SPEC_ID,
          sessionId: context("spec_update_section").sessionId,
          toolCallId: "call-spec_update_section",
        },
      },
    ]);
  });

  test("every section-scoped tool enters and leaves presence", async () => {
    const cases: Array<{ name: string; input: object; sectionId: string }> = [
      { name: "spec_read", input: { section_id: "context" }, sectionId: "context" },
      {
        name: "spec_update_section",
        input: { section_id: "context", markdown: "new" },
        sectionId: "context",
      },
      {
        name: "spec_set_section_state",
        input: { section_id: "context", state: "drafted" },
        sectionId: "context",
      },
      {
        name: "spec_add_open_question",
        input: { section_id: "context", question: "Question?" },
        sectionId: "context",
      },
      {
        name: "spec_resolve_open_question",
        input: {
          section_id: "context",
          question_id: "00000000-0000-4000-8000-000000000001",
          answer_markdown: "Answer",
        },
        sectionId: "context",
      },
      {
        name: "spec_update_block",
        input: { section_id: "context", block_id: "diagram", source: "new" },
        sectionId: "context",
      },
    ];

    for (const item of cases) {
      const state = recorder();
      await call(state.deps, item.name, item.input);
      expect(
        state.presence.map((entry) => entry.action),
        item.name,
      ).toEqual(["enter", "leave"]);
      expect(state.presence[0]?.input, item.name).toMatchObject({ sectionId: item.sectionId });
    }
  });

  test("whole-document, notes, and ticket tools do not synthesize section presence", async () => {
    const state = recorder();
    await call(state.deps, "spec_read", {});
    await call(state.deps, "spec_update_notes", { markdown: "notes" });
    await call(state.deps, "spec_propose_tickets", {
      idempotency_key: "proposal-1",
      tickets: [
        {
          client_id: "ticket-1",
          title: "Implement the change",
          description: "Use the approved design.",
          section_id: "context",
        },
      ],
    });

    expect(state.presence).toHaveLength(0);
  });

  test("n/a state requires a reason without adding a union to the schema", () => {
    const registry = createToolRegistry();
    registerSpecTools(registry, recorder().deps);
    const tool = registry.get("spec_set_section_state");
    if (tool === undefined) throw new Error("spec_set_section_state is not registered");

    expect(() => tool.input.parse({ section_id: "scope", state: "n/a" })).toThrow();
    expect(
      tool.input.parse({
        section_id: "scope",
        state: "n/a",
        reason: "No external API",
      }),
    ).toEqual({ section_id: "scope", state: "n/a", reason: "No external API" });
    const manifest = compileToolManifest(registry, undefined, "spec").find(
      (entry) => entry.name === "spec_set_section_state",
    );
    expect(JSON.stringify(manifest?.inputSchema)).not.toContain('"anyOf"');
  });
});
