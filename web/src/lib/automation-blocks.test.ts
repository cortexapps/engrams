import { describe, expect, it } from "vitest";

import {
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
    { id: "gate", type: "system.review_policy_gate", config: { reviewId: "x" } },
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
