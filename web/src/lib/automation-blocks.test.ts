import { describe, expect, it } from "vitest";

import {
  AGENT_CLOSE_STEP_ID,
  AGENT_NODE_ID,
  AGENT_STEP_ID,
  blockKind,
  blockTypesOf,
  defaultAgent,
  hasScopedAgents,
  hasSlackTrigger,
  roleOf,
  scopeLabel,
  scopePresetOf,
  setRole,
  groupedBlockKinds,
  insertableBlockKinds,
  removeEntrypoint,
  projectEntrypoint,
  mergeEntrypoint,
  entrypointIds,
  entrypointIdError,
  blockIdsOutsideEntrypoint,
  addEntrypoint,
  applyOverrides,
  diffOverrides,
  findBlock,
  insertBlock,
  isTunable,
  moveBlock,
  nextBlockId,
  parseBlockErrors,
  removeBlock,
  setPath,
  withAgent,
  type AutomationDefinition,
  type BlockDef,
} from "./automation-blocks";

const def = (blocks: BlockDef[]): AutomationDefinition => ({
  engine: 1,
  trigger: { kind: "manual" },
  blocks,
  inputsSchema: [],
  settings: { endSessionsOnFinish: false },
});

describe("tree editing", () => {
  const tree: BlockDef[] = [
    { id: "a", type: "create_session", config: { profileId: "p" } },
    {
      id: "gate",
      type: "branch",
      config: { conditions: { mode: "all", conditions: [] } },
      then: [{ id: "hot", type: "end_session", config: { session: { blockId: "a" } } }],
      else: [],
    },
  ];

  it("inserts into a nested slot and allocates a unique id", () => {
    const id = nextBlockId(tree, "end_session");
    expect(id).toBe("end_session");
    const next = insertBlock(tree, { parentId: "gate", slot: "else" }, 0, {
      id,
      type: "end_session",
      config: {},
    });
    expect(findBlock(next, "gate")!.else!.map((b) => b.id)).toEqual(["end_session"]);
    expect(nextBlockId(next, "end_session")).toBe("end_session_2");
  });

  it("moves within a list and removes recursively", () => {
    const moved = moveBlock(tree, { root: true }, 1, 0);
    expect(moved.map((b) => b.id)).toEqual(["gate", "a"]);
    const removed = removeBlock(tree, "hot");
    expect(findBlock(removed, "gate")!.then).toEqual([]);
    expect(findBlock(removed, "hot")).toBeUndefined();
  });

  it("setPath writes dotted keys and deletes on empty", () => {
    const c = setPath({ waitFor: { kind: "run_end" } }, "waitFor.name", "done");
    expect(c).toEqual({ waitFor: { kind: "run_end", name: "done" } });
    expect(setPath(c, "waitFor.name", "")).toEqual({ waitFor: { kind: "run_end" } });
  });
});

describe("built-in editing model", () => {
  const shipped = def([
    {
      id: "finder",
      type: "create_session",
      tunable: ["promptTemplate", "profileId"],
      config: { profileId: "pr_reviewer", promptTemplate: "Review it.", role: "finder" },
    },
    { id: "recap", type: "relay_close", config: { status: "completed" } },
  ]);

  it("isTunable is by top-level key, including dotted paths", () => {
    const finder = shipped.blocks[0]!;
    expect(isTunable(finder, "promptTemplate")).toBe(true);
    expect(isTunable(finder, "role")).toBe(false);
    expect(isTunable({ ...finder, tunable: ["waitFor"] }, "waitFor.kind")).toBe(true);
  });

  it("applyOverrides layers values; diffOverrides emits only changed tunable keys", () => {
    const effective = applyOverrides(shipped, { finder: { promptTemplate: "Be strict." } });
    expect(findBlock(effective.blocks, "finder")!.config["promptTemplate"]).toBe("Be strict.");

    // Edit a tunable field and (illegally, via the shell) a pinned one.
    const edited = def([
      {
        ...shipped.blocks[0]!,
        config: { profileId: "pr_reviewer", promptTemplate: "Be strict.", role: "verifier" },
      },
      shipped.blocks[1]!,
    ]);
    expect(diffOverrides(shipped, edited)).toEqual({ finder: { promptTemplate: "Be strict." } });
    // Unchanged tunable → absent; pinned change → never sent.
    expect(diffOverrides(shipped, shipped)).toEqual({});
  });

  it("clearing a tunable field reverts to shipped — never a null override the server would reject", () => {
    const withMode = def([
      {
        ...shipped.blocks[0]!,
        tunable: ["promptTemplate", "profileId", "harnessMode"],
        config: { ...shipped.blocks[0]!.config, harnessMode: "plan" },
      },
      shipped.blocks[1]!,
    ]);
    // The user clears harnessMode (the shell deletes the key).
    const cleared = def([
      {
        ...withMode.blocks[0]!,
        config: { profileId: "pr_reviewer", promptTemplate: "Review it.", role: "finder" },
      },
      withMode.blocks[1]!,
    ]);
    expect(diffOverrides(withMode, cleared)).toEqual({});

    // Change, then clear, round-trips to no override.
    const changed = def([
      { ...withMode.blocks[0]!, config: { ...withMode.blocks[0]!.config, harnessMode: "act" } },
      withMode.blocks[1]!,
    ]);
    expect(diffOverrides(withMode, changed)).toEqual({ finder: { harnessMode: "act" } });
    expect(diffOverrides(withMode, cleared)).toEqual({});

    // A field absent from the SHIPPED config set by the user is an override;
    // clearing it again removes the override.
    const added = def([
      { ...shipped.blocks[0]!, config: { ...shipped.blocks[0]!.config, harnessMode: "plan" } },
      shipped.blocks[1]!,
    ]);
    const shippedTunable = def([
      { ...shipped.blocks[0]!, tunable: ["promptTemplate", "profileId", "harnessMode"] },
      shipped.blocks[1]!,
    ]);
    expect(diffOverrides(shippedTunable, added)).toEqual({ finder: { harnessMode: "plan" } });
    expect(diffOverrides(shippedTunable, shippedTunable)).toEqual({});
  });
});

describe("parseBlockErrors", () => {
  it("routes block.field, definition-level, and unparseable messages", () => {
    expect(
      parseBlockErrors(
        "finder.promptTemplate: template is invalid; trigger.eventKeys: at least one",
      ),
    ).toEqual([
      { blockId: "finder", field: "promptTemplate", message: "template is invalid" },
      { blockId: "", field: "trigger.eventKeys", message: "at least one" },
    ]);
    expect(parseBlockErrors("something odd happened")).toEqual([
      { blockId: "", field: "form", message: "something odd happened" },
    ]);
  });
});

describe("state + probe blocks in the palette (ADR 0119 D10/D11)", () => {
  it("every engine block the orchestrator registers has a typed catalog entry", () => {
    for (const kind of [
      "state_get",
      "state_set",
      "state_delete",
      "state_list",
      "session_status",
      "lookup_pr_session",
      "review_open_pass",
      "review_stage",
      "review_settle",
      "review_close_pass",
      "review_record_post",
      "resolve_user",
      "relay_session",
      "relay_close",
    ]) {
      const spec = blockKind(kind);
      expect(spec.description).not.toBe("Unknown block kind.");
      expect(insertableBlockKinds(new Set()).some((s) => s.kind === kind)).toBe(true);
    }
  });

  it("a built-in-only kind renders with its own label and is never offered", () => {
    expect(blockKind("slack_choice").label).toBe("Slack choice");
    expect(blockKind("slack_choice").inspector).toBe("readonly");
    expect(insertableBlockKinds(new Set()).some((s) => s.kind === "slack_choice")).toBe(false);
  });

  it("groups the insertable kinds by purpose, the internals under Advanced; the agent is not a block", () => {
    const groups = groupedBlockKinds(new Set(["openrouter"]));
    expect(groups.map((g) => g.id)).toEqual(["flow", "sessions", "integrations", "advanced"]);
    expect(groups.flatMap((g) => g.kinds.map((k) => k.kind))).not.toContain("run_agent");
    const advanced = groups.find((g) => g.id === "advanced")!.kinds.map((k) => k.kind);
    for (const kind of ["loop", "wait_event", "state_get", "review_open_pass", "relay_close"]) {
      expect(advanced).toContain(kind);
    }
    // Every insertable kind is in exactly one group.
    const all = groups.flatMap((g) => g.kinds.map((k) => k.kind));
    expect(new Set(all).size).toBe(all.length);
    expect(all.length).toBe(insertableBlockKinds(new Set(["openrouter"])).length);
  });

  it("a router-backed kind is insertable only while its router has a key", () => {
    expect(insertableBlockKinds(new Set()).some((s) => s.kind === "decide")).toBe(false);
    expect(insertableBlockKinds(new Set(["openrouter"])).some((s) => s.kind === "decide")).toBe(
      true,
    );
    // An existing block still renders with its own inspector.
    expect(blockKind("decide").inspector).toBe("decide");
  });

  it("defaults satisfy the summary renderers", () => {
    for (const spec of insertableBlockKinds(new Set(["openrouter"]))) {
      expect(() => spec.summary(spec.defaults())).not.toThrow();
    }
  });
});

describe("entrypoint projection (ADR 0119 D9)", () => {
  const def = (): AutomationDefinition => ({
    engine: 1,
    trigger: { kind: "manual" },
    blocks: [{ id: "launch", type: "create_session", config: {} }],
    entrypoints: [
      {
        id: "sweep",
        trigger: { kind: "cron", schedule: "* * * * *", timezone: "UTC" } as never,
        blocks: [{ id: "probe", type: "session_status", config: {} }],
      },
    ],
    inputsSchema: [],
    settings: { endSessionsOnFinish: false },
  });

  it("projects an entrypoint's trigger + blocks to the top level; main is the definition itself", () => {
    const d = def();
    expect(projectEntrypoint(d, "main")).toBe(d);
    const sweep = projectEntrypoint(d, "sweep");
    expect(sweep.trigger.kind).toBe("cron");
    expect(sweep.blocks.map((b) => b.id)).toEqual(["probe"]);
  });

  it("merge folds an edited projection back and restores main's own trigger + blocks", () => {
    const d = def();
    const edited = {
      ...projectEntrypoint(d, "sweep"),
      blocks: [{ id: "probe", type: "session_status", config: { x: 1 } }],
    };
    const merged = mergeEntrypoint(d, "sweep", edited);
    expect(merged.trigger.kind).toBe("manual");
    expect(merged.blocks.map((b) => b.id)).toEqual(["launch"]);
    expect(merged.entrypoints?.[0]?.blocks[0]?.config).toEqual({ x: 1 });
  });

  it("add/remove round-trip; removing the last extra drops the field entirely", () => {
    let d = addEntrypoint(def(), "feedback");
    expect(entrypointIds(d)).toEqual(["main", "sweep", "feedback"]);
    d = removeEntrypoint(removeEntrypoint(d, "feedback"), "sweep");
    expect(entrypointIds(d)).toEqual(["main"]);
    expect("entrypoints" in d).toBe(false);
  });

  it("id validation: shape, the reserved main, and duplicates", () => {
    const d = def();
    expect(entrypointIdError(d, "feedback")).toBeNull();
    expect(entrypointIdError(d, "main")).toContain("implicit");
    expect(entrypointIdError(d, "sweep")).toContain("exists");
    expect(entrypointIdError(d, "Bad-Id")).toContain("Lowercase");
  });

  it("reserved ids keep nextBlockId unique across entrypoints", () => {
    const d = def();
    const reserved = blockIdsOutsideEntrypoint(d, "sweep");
    expect(reserved).toEqual(["launch"]);
    // Inserting a create_session inside "sweep" must not mint "launch"
    // again — the server holds block ids unique automation-wide.
    expect(nextBlockId(projectEntrypoint(d, "sweep").blocks, "create_session", reserved)).toBe(
      "create_session",
    );
    expect(nextBlockId([], "create_session", ["create_session"])).toBe("create_session_2");
  });
});

describe("the agent with ways in", () => {
  const base = (): AutomationDefinition => ({
    engine: 1,
    trigger: {
      kind: "integration",
      provider: "slack",
      connectionId: "c",
      eventKeys: ["app_mention"],
    },
    blocks: [],
    entrypoints: [{ id: "done", trigger: { kind: "manual" }, blocks: [] }],
    inputsSchema: [],
    settings: { endSessionsOnFinish: false },
  });

  it("a new agent is scoped by the trigger: a thread for Slack, a PR for GitHub, a session per event otherwise", () => {
    expect(scopePresetOf(defaultAgent(base().trigger).scope)).toBe("slack_thread");
    expect(
      scopePresetOf(
        defaultAgent({ kind: "integration", provider: "github", connectionId: "c", eventKeys: [] })
          .scope,
      ),
    ).toBe("pull_request");
    expect(defaultAgent({ kind: "cron", schedule: "* * * * *", timezone: "UTC" }).scope).toEqual({
      kind: "event",
    });
  });

  it("the scope badge reads the agent's scope; a session per event has none", () => {
    const d = base();
    expect(scopeLabel({ ...d, agent: defaultAgent(d.trigger) })).toBe("per thread");
    expect(
      scopeLabel({ ...d, agent: { ...defaultAgent(d.trigger), scope: { kind: "automation" } } }),
    ).toBe("one agent");
    expect(
      scopeLabel({ ...d, agent: { ...defaultAgent(d.trigger), scope: { kind: "event" } } }),
    ).toBeUndefined();
    expect(
      scopeLabel({
        ...d,
        agent: { ...defaultAgent(d.trigger), scope: { kind: "key", keyTemplate: "${{ x }}" } },
      }),
    ).toBe("per key");
    expect(hasScopedAgents({ ...d, agent: defaultAgent(d.trigger) })).toBe(true);
    expect(
      hasScopedAgents({ ...d, agent: { ...defaultAgent(d.trigger), scope: { kind: "event" } } }),
    ).toBe(false);
  });

  it("a role is read and written per way in; start is stored as no field", () => {
    let d = base();
    expect(roleOf(d, "main")).toBe("start");
    d = setRole(d, "done", "close");
    expect(roleOf(d, "done")).toBe("close");
    d = setRole(setRole(d, "main", "reach"), "main", "start");
    expect("role" in d).toBe(false);
    expect(d.entrypoints?.[0]?.role).toBe("close");
  });

  it("the ledger's agent steps take the agent's types, so their sentences read", () => {
    const d = { ...base(), agent: defaultAgent(base().trigger) };
    const types = blockTypesOf(d);
    expect(types.get(AGENT_STEP_ID)).toBe("run_agent");
    expect(types.get(AGENT_CLOSE_STEP_ID)).toBe("close_agent");
    expect(blockTypesOf(base()).has(AGENT_STEP_ID)).toBe(false);
  });

  it("an agent error is addressed to the agent node", () => {
    expect(
      parseBlockErrors("__agent__.profileId: pick a profile; admit.conditions: empty"),
    ).toEqual([
      { blockId: AGENT_NODE_ID, field: "profileId", message: "pick a profile" },
      { blockId: "admit", field: "conditions", message: "empty" },
    ]);
  });

  it("a way in's role error lands on its trigger row", () => {
    expect(
      parseBlockErrors("__trigger__.role: needs an agent; __entrypoints__.role: needs an agent"),
    ).toEqual([
      { blockId: "", field: "role", message: "needs an agent" },
      { blockId: "", field: "role", message: "needs an agent" },
    ]);
  });

  it("adding an agent turns the ways in's scope into the agent's key", () => {
    const d: AutomationDefinition = {
      ...def([]),
      settings: {
        endSessionsOnFinish: false,
        instance: { keyTemplate: "${{ event.raw.data.id }}" },
      },
    };
    const next = withAgent(d);
    expect(next.agent?.scope).toEqual({ kind: "key", keyTemplate: "${{ event.raw.data.id }}" });
    expect(next.settings.instance).toBeUndefined();
    expect(withAgent(base()).agent?.scope).toEqual(defaultAgent(base().trigger).scope);
  });

  it("knows when a way in is a Slack event", () => {
    expect(hasSlackTrigger(base())).toBe(true);
    expect(hasSlackTrigger(def([]))).toBe(false);
    expect(
      hasSlackTrigger({
        ...def([]),
        entrypoints: [
          {
            id: "mention",
            trigger: { kind: "integration", provider: "slack", connectionId: "", eventKeys: [] },
            blocks: [],
          },
        ],
      }),
    ).toBe(true);
  });
});
