import { describe, expect, test } from "bun:test";

import {
  addTicket,
  childrenOf,
  deleteTicket,
  descendantIds,
  mergeTickets,
  moveTicket,
  normalizeTree,
  orderedTree,
  splitTicket,
  SpecTicketTreeError,
  treeDepths,
  updateTicket,
  type SpecTicketNode,
} from "./ticket-tree.ts";
import { backlinkBody, backlinkHref, backlinkLabel, withBacklink } from "./ticket-backlink.ts";

function node(id: string, overrides: Partial<SpecTicketNode> = {}): SpecTicketNode {
  return {
    id,
    parentId: null,
    ordinal: 0,
    title: id,
    description: `body of ${id}`,
    sectionId: "data-model",
    dependsOn: [],
    syncState: "draft",
    linearId: null,
    syncError: null,
    ...overrides,
  };
}

/**
 * The mock 2l tree: four roots, one of them with a child.
 *
 *   quota-columns
 *   meter-rollup
 *     meter-events
 *   gateway-limiter
 *   payload-429
 */
function tree(): SpecTicketNode[] {
  return normalizeTree([
    node("quota-columns", { ordinal: 0 }),
    node("meter-rollup", { ordinal: 1 }),
    node("meter-events", { parentId: "meter-rollup", ordinal: 0, sectionId: "api" }),
    node("gateway-limiter", { ordinal: 2, sectionId: "proposed-design" }),
    node("payload-429", { ordinal: 3, sectionId: "api" }),
  ]);
}

/** Every sibling group is dense from 0, with no repeats. */
function expectDenseOrdinals(nodes: readonly SpecTicketNode[]): void {
  const parents = new Set<string | null>(nodes.map((entry) => entry.parentId));
  for (const parentId of parents) {
    const ordinals = childrenOf(nodes, parentId).map((entry) => entry.ordinal);
    expect(ordinals).toEqual(ordinals.map((_value, index) => index));
  }
}

describe("ticket tree order", () => {
  test("reads depth first, parent before subtree", () => {
    expect(orderedTree(tree()).map((entry) => entry.id)).toEqual([
      "quota-columns",
      "meter-rollup",
      "meter-events",
      "gateway-limiter",
      "payload-429",
    ]);
    expect(treeDepths(tree()).get("meter-events")).toBe(1);
    expect(treeDepths(tree()).get("meter-rollup")).toBe(0);
  });

  test("keeps a node whose parent is gone, at the root", () => {
    const orphaned = tree().filter((entry) => entry.id !== "meter-rollup");
    const ordered = orderedTree(orphaned);
    expect(ordered.find((entry) => entry.id === "meter-events")?.parentId).toBeNull();
  });

  test("normalizing drops a dependency on a ticket that is gone", () => {
    const withDependency = tree().map((entry) =>
      entry.id === "payload-429" ? { ...entry, dependsOn: ["gateway-limiter", "ghost"] } : entry,
    );
    const normalized = normalizeTree(withDependency);
    expect(normalized.find((entry) => entry.id === "payload-429")?.dependsOn).toEqual([
      "gateway-limiter",
    ]);
  });
});

describe("direct manipulation", () => {
  test("add places a ticket among its new siblings", () => {
    const next = addTicket(tree(), {
      id: "shadow-count",
      parentId: null,
      index: 1,
      title: "Shadow-count 7 days before enforcing",
      description: "body",
      sectionId: "failure-modes",
    });
    expect(childrenOf(next, null).map((entry) => entry.id)).toEqual([
      "quota-columns",
      "shadow-count",
      "meter-rollup",
      "gateway-limiter",
      "payload-429",
    ]);
    expectDenseOrdinals(next);
  });

  test("add past the end lands last", () => {
    const next = addTicket(tree(), {
      id: "late",
      parentId: null,
      index: 99,
      title: "Late",
      description: "body",
      sectionId: "api",
    });
    expect(childrenOf(next, null).at(-1)?.id).toBe("late");
  });

  test("retitle and re-point the backlink leave the shape alone", () => {
    const next = updateTicket(tree(), {
      id: "payload-429",
      title: "Quota-aware 429 payload",
      sectionId: "failure-modes",
    });
    const updated = next.find((entry) => entry.id === "payload-429");
    expect(updated?.title).toBe("Quota-aware 429 payload");
    expect(updated?.sectionId).toBe("failure-modes");
    expect(orderedTree(next).map((entry) => entry.id)).toEqual(
      orderedTree(tree()).map((entry) => entry.id),
    );
  });

  test("delete takes the whole subtree", () => {
    const next = deleteTicket(tree(), "meter-rollup");
    expect(next.map((entry) => entry.id).sort()).toEqual([
      "gateway-limiter",
      "payload-429",
      "quota-columns",
    ]);
    expectDenseOrdinals(next);
  });

  test("an unknown ticket is refused, not ignored", () => {
    expect(() => deleteTicket(tree(), "ghost")).toThrow(SpecTicketTreeError);
    expect(() => moveTicket(tree(), { id: "ghost", parentId: null })).toThrow("Unknown ticket");
  });
});

describe("drag to nest", () => {
  test("a drop under another row re-parents it and closes the gap it left", () => {
    const next = moveTicket(tree(), { id: "gateway-limiter", parentId: "quota-columns" });
    expect(next.find((entry) => entry.id === "gateway-limiter")?.parentId).toBe("quota-columns");
    expect(childrenOf(next, "quota-columns").map((entry) => entry.id)).toEqual([
      "gateway-limiter",
    ]);
    expect(childrenOf(next, null).map((entry) => entry.id)).toEqual([
      "quota-columns",
      "meter-rollup",
      "payload-429",
    ]);
    expectDenseOrdinals(next);
  });

  test("a drop at an index inside a parent orders the new siblings", () => {
    const next = moveTicket(tree(), { id: "payload-429", parentId: "meter-rollup", index: 0 });
    expect(childrenOf(next, "meter-rollup").map((entry) => entry.id)).toEqual([
      "payload-429",
      "meter-events",
    ]);
    expectDenseOrdinals(next);
  });

  test("a reorder among the same siblings keeps every parent", () => {
    const next = moveTicket(tree(), { id: "payload-429", parentId: null, index: 0 });
    expect(childrenOf(next, null).map((entry) => entry.id)).toEqual([
      "payload-429",
      "quota-columns",
      "meter-rollup",
      "gateway-limiter",
    ]);
    expect(next.find((entry) => entry.id === "meter-events")?.parentId).toBe("meter-rollup");
    expectDenseOrdinals(next);
  });

  test("a drop back to the root takes the subtree with it", () => {
    const nested = moveTicket(tree(), { id: "gateway-limiter", parentId: "meter-events" });
    const next = moveTicket(nested, { id: "meter-rollup", parentId: null, index: 0 });
    expect(childrenOf(next, "meter-events").map((entry) => entry.id)).toEqual(["gateway-limiter"]);
    expect(treeDepths(next).get("gateway-limiter")).toBe(2);
  });

  test("a ticket cannot be nested under its own subtree", () => {
    expect(() => moveTicket(tree(), { id: "meter-rollup", parentId: "meter-events" })).toThrow(
      "own subtree",
    );
    expect(() => moveTicket(tree(), { id: "meter-rollup", parentId: "meter-rollup" })).toThrow(
      "own subtree",
    );
  });
});

describe("merge", () => {
  test("the survivor keeps its place and adopts the folded children", () => {
    const next = mergeTickets(tree(), {
      targetId: "payload-429",
      sourceIds: ["meter-rollup"],
      title: "Quota-aware 429 payload",
    });
    expect(next.map((entry) => entry.id)).not.toContain("meter-rollup");
    expect(childrenOf(next, "payload-429").map((entry) => entry.id)).toEqual(["meter-events"]);
    expect(childrenOf(next, null).map((entry) => entry.id)).toEqual([
      "quota-columns",
      "gateway-limiter",
      "payload-429",
    ]);
    expect(next.find((entry) => entry.id === "payload-429")?.title).toBe(
      "Quota-aware 429 payload",
    );
    expectDenseOrdinals(next);
  });

  test("folded descriptions join, and the survivor keeps its backlink", () => {
    const next = mergeTickets(tree(), {
      targetId: "payload-429",
      sourceIds: ["gateway-limiter"],
    });
    const merged = next.find((entry) => entry.id === "payload-429");
    expect(merged?.description).toBe("body of payload-429\n\nbody of gateway-limiter");
    expect(merged?.sectionId).toBe("api");
  });

  test("folded children land after the survivor's own children", () => {
    const nested = moveTicket(tree(), { id: "quota-columns", parentId: "payload-429" });
    const next = mergeTickets(nested, { targetId: "payload-429", sourceIds: ["meter-rollup"] });
    expect(childrenOf(next, "payload-429").map((entry) => entry.id)).toEqual([
      "quota-columns",
      "meter-events",
    ]);
    expectDenseOrdinals(next);
  });

  test("a dependency on a folded ticket does not survive as a dangling id", () => {
    const linked = tree().map((entry) =>
      entry.id === "quota-columns" ? { ...entry, dependsOn: ["gateway-limiter"] } : entry,
    );
    const next = mergeTickets(linked, {
      targetId: "payload-429",
      sourceIds: ["gateway-limiter"],
    });
    expect(next.find((entry) => entry.id === "quota-columns")?.dependsOn).toEqual([]);
  });

  test("a merge needs a ticket to fold in, and refuses a descendant target", () => {
    expect(() => mergeTickets(tree(), { targetId: "payload-429", sourceIds: [] })).toThrow(
      "needs a ticket",
    );
    expect(() =>
      mergeTickets(tree(), { targetId: "meter-events", sourceIds: ["meter-rollup"] }),
    ).toThrow("own descendants");
  });
});

describe("split", () => {
  test("the parts become siblings at the original place, children stay with the first", () => {
    const next = splitTicket(tree(), "meter-rollup", [
      { id: "meter-rollup", title: "Hourly meter rollup job", description: "the job" },
      { id: "meter-backfill", title: "Backfill the rollup", description: "the backfill" },
    ]);
    expect(childrenOf(next, null).map((entry) => entry.id)).toEqual([
      "quota-columns",
      "meter-rollup",
      "meter-backfill",
      "gateway-limiter",
      "payload-429",
    ]);
    expect(childrenOf(next, "meter-rollup").map((entry) => entry.id)).toEqual(["meter-events"]);
    expectDenseOrdinals(next);
  });

  test("a split part inherits the backlink it does not name", () => {
    const next = splitTicket(tree(), "gateway-limiter", [
      { id: "gateway-limiter", title: "Enforce", description: "a" },
      { id: "gateway-shadow", title: "Shadow", description: "b", sectionId: "failure-modes" },
    ]);
    expect(next.find((entry) => entry.id === "gateway-limiter")?.sectionId).toBe("proposed-design");
    expect(next.find((entry) => entry.id === "gateway-shadow")?.sectionId).toBe("failure-modes");
  });

  test("a split of one is refused", () => {
    expect(() =>
      splitTicket(tree(), "payload-429", [{ id: "payload-429", title: "a", description: "b" }]),
    ).toThrow("at least two parts");
  });

  test("split parts start as drafts, because they are new work", () => {
    const synced = tree().map((entry) =>
      entry.id === "payload-429"
        ? { ...entry, syncState: "synced" as const, linearId: "ENG-412" }
        : entry,
    );
    const next = splitTicket(synced, "payload-429", [
      { id: "payload-429", title: "a", description: "a" },
      { id: "payload-429-b", title: "b", description: "b" },
    ]);
    for (const id of ["payload-429", "payload-429-b"]) {
      const part = next.find((entry) => entry.id === id);
      expect(part?.syncState).toBe("draft");
      expect(part?.linearId).toBeNull();
    }
  });
});

describe("descendants", () => {
  test("names the whole subtree and never the node itself", () => {
    const deep = moveTicket(tree(), { id: "gateway-limiter", parentId: "meter-events" });
    expect([...descendantIds(deep, "meter-rollup")].sort()).toEqual([
      "gateway-limiter",
      "meter-events",
    ]);
    expect(descendantIds(deep, "payload-429").size).toBe(0);
  });
});

describe("backlinks", () => {
  const backlink = {
    sectionId: "data-model",
    sectionTitle: "Data model",
    href: backlinkHref("spec-1", "data-model"),
  };

  test("the label is the section sign and the section title", () => {
    expect(backlinkLabel("Data model")).toBe("§Data model");
  });

  test("the href points at the spec tab, anchored at the section", () => {
    expect(backlinkHref("spec 1", "data model")).toBe(
      "/specs/spec%201?view=spec&section=data%20model",
    );
  });

  test("a description opens with the link", () => {
    expect(withBacklink("Add the columns.", backlink)).toBe(
      "[§Data model](/specs/spec-1?view=spec&section=data-model)\n\nAdd the columns.",
    );
  });

  test("rewriting a description replaces the old link instead of stacking one", () => {
    const once = withBacklink("Add the columns.", backlink);
    const twice = withBacklink(once, { ...backlink, sectionTitle: "Data design" });
    expect(twice).toBe(
      "[§Data design](/specs/spec-1?view=spec&section=data-model)\n\nAdd the columns.",
    );
  });

  test("the body a person edits excludes the link line", () => {
    expect(backlinkBody(withBacklink("Add the columns.\n\nThen backfill.", backlink))).toBe(
      "Add the columns.\n\nThen backfill.",
    );
    expect(backlinkBody("No link here.")).toBe("No link here.");
  });
});
