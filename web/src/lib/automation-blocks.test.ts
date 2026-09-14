import { describe, expect, it } from "vitest";

import {
  blockKind,
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
      "resolve_user",
      "relay_session",
      "relay_close",
    ]) {
      const spec = blockKind(kind);
      expect(spec.description).not.toBe("Unknown block kind.");
      expect(insertableBlockKinds().some((s) => s.kind === kind)).toBe(true);
    }
  });

  it("defaults satisfy the summary renderers", () => {
    for (const spec of insertableBlockKinds()) {
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
