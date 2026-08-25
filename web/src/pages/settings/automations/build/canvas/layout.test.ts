import { describe, expect, it } from "vitest";

import type { BlockDef, ListPath } from "@/lib/automation-blocks";

import {
  BRANCH_GAP,
  CANVAS_PAD,
  COL_GAP,
  EMPTY_W,
  GROUP_PAD,
  JOIN_GAP,
  layoutCanvas,
  NODE_H,
  NODE_W,
  roundedOrthPath,
  TAIL_H,
  TRIGGER_ROW_ID,
  V_GAP,
  type CanvasLayout,
} from "./layout";

const leaf = (id: string): BlockDef => ({
  id,
  type: "run_command",
  config: { session: { blockId: "s" }, commandTemplate: "true" },
});

const branch = (id: string, then: BlockDef[], els?: BlockDef[]): BlockDef => ({
  id,
  type: "branch",
  config: { conditions: { mode: "all", conditions: [] } },
  then,
  ...(els !== undefined ? { else: els } : {}),
});

const loop = (id: string, body: BlockDef[]): BlockDef => ({
  id,
  type: "loop",
  config: { maxIterations: 3 },
  body,
});

function nodeById(layout: CanvasLayout, id: string) {
  const node = layout.nodes.find((n) => n.id === id);
  expect(node, `node ${id}`).toBeDefined();
  return node!;
}

function pathsOf(at: ListPath): string {
  return "root" in at ? "root" : `${at.parentId}/${at.slot}`;
}

/** Every list of length n must carry exactly n+1 insert points, 0..n. */
function assertInsertCompleteness(layout: CanvasLayout, blocks: readonly BlockDef[]) {
  const lists = new Map<string, number>([["root", blocks.length]]);
  const walk = (list: readonly BlockDef[]) => {
    for (const block of list) {
      if (block.then !== undefined || block.type === "branch") {
        lists.set(`${block.id}/then`, (block.then ?? []).length);
        lists.set(`${block.id}/else`, (block.else ?? []).length);
        walk(block.then ?? []);
        walk(block.else ?? []);
      }
      if (block.body !== undefined || block.type === "loop") {
        lists.set(`${block.id}/body`, (block.body ?? []).length);
        walk(block.body ?? []);
      }
    }
  };
  walk(blocks);

  const seen = new Map<string, Set<number>>();
  for (const edge of layout.edges) {
    if (!edge.insert) continue;
    const key = pathsOf(edge.insert.at);
    const set = seen.get(key) ?? new Set<number>();
    expect(set.has(edge.insert.index), `duplicate insert ${key}:${edge.insert.index}`).toBe(false);
    set.add(edge.insert.index);
    seen.set(key, set);
  }
  for (const [key, length] of lists) {
    const set = seen.get(key) ?? new Set<number>();
    expect(
      [...set].sort((a, b) => a - b),
      `insert indexes for ${key}`,
    ).toEqual(Array.from({ length: length + 1 }, (_, i) => i));
  }
  expect(seen.size).toBe(lists.size);
}

function assertNoOverlap(layout: CanvasLayout) {
  for (let i = 0; i < layout.nodes.length; i++) {
    for (let j = i + 1; j < layout.nodes.length; j++) {
      const a = layout.nodes[i]!;
      const b = layout.nodes[j]!;
      const overlap = a.x < b.x + b.w && b.x < a.x + a.w && a.y < b.y + b.h && b.y < a.y + a.h;
      expect(overlap, `${a.id} overlaps ${b.id}`).toBe(false);
    }
  }
}

describe("layoutCanvas", () => {
  it("empty automation: trigger + a single tail insert at index 0", () => {
    const layout = layoutCanvas([]);
    expect(layout.nodes.map((n) => n.id)).toEqual([TRIGGER_ROW_ID]);
    expect(layout.edges).toHaveLength(1);
    expect(layout.edges[0]!.kind).toBe("tail");
    expect(layout.edges[0]!.insert).toMatchObject({ at: { root: true }, index: 0 });
    expect(layout.width).toBe(NODE_W + 2 * CANVAS_PAD);
    expect(layout.height).toBe(CANVAS_PAD + NODE_H + TAIL_H + CANVAS_PAD);
    assertInsertCompleteness(layout, []);
  });

  it("linear a → b → c: exact trunk coordinates and 4 insert points", () => {
    const blocks = [leaf("a"), leaf("b"), leaf("c")];
    const layout = layoutCanvas(blocks);
    const axis = layout.width / 2;
    const listTop = CANVAS_PAD + NODE_H + V_GAP;
    expect(nodeById(layout, "a")).toMatchObject({ x: axis - NODE_W / 2, y: listTop });
    expect(nodeById(layout, "b").y).toBe(listTop + NODE_H + V_GAP);
    expect(nodeById(layout, "c").y).toBe(listTop + 2 * (NODE_H + V_GAP));
    // Document order: trigger, a, b, c.
    expect(layout.nodes.map((n) => n.id)).toEqual([TRIGGER_ROW_ID, "a", "b", "c"]);
    assertInsertCompleteness(layout, blocks);
    assertNoOverlap(layout);
  });

  it("branch fans out, labels both legs, and rejoins; empty else is one labeled edge", () => {
    const blocks = [branch("gate", [leaf("hot")], [])];
    const layout = layoutCanvas(blocks);
    const axis = layout.width / 2;
    const gate = nodeById(layout, "gate");
    const hot = nodeById(layout, "hot");
    expect(hot.y).toBe(gate.y + NODE_H + BRANCH_GAP);
    // then column sits left of the else lane.
    const childrenW = NODE_W + COL_GAP + EMPTY_W;
    expect(hot.x + NODE_W / 2).toBe(axis - childrenW / 2 + NODE_W / 2);
    const thenEdge = layout.edges.find((e) => e.kind === "then")!;
    const elseEdge = layout.edges.find((e) => e.kind === "else")!;
    expect(thenEdge.label).toBe("then");
    expect(elseEdge.label).toBe("else");
    // The empty else leg's edge carries its only insert.
    expect(elseEdge.insert).toMatchObject({ at: { parentId: "gate", slot: "else" }, index: 0 });
    // Join waypoint: the block after the branch chains from below both legs.
    assertInsertCompleteness(layout, blocks);
    assertNoOverlap(layout);
  });

  it("an else-less branch (undefined slot) still lays out both lanes", () => {
    const blocks = [branch("gate", [leaf("hot")])];
    const layout = layoutCanvas(blocks);
    expect(layout.edges.some((e) => e.kind === "else")).toBe(true);
    assertInsertCompleteness(layout, blocks);
  });

  it("loop draws a group box around its body with inserts inside", () => {
    const blocks = [loop("poll", [leaf("tick"), leaf("tock")])];
    const layout = layoutCanvas(blocks);
    const poll = nodeById(layout, "poll");
    const tick = nodeById(layout, "tick");
    expect(layout.groups).toHaveLength(1);
    const box = layout.groups[0]!;
    expect(box.id).toBe("poll");
    expect(box.y).toBe(poll.y + NODE_H + 12);
    expect(tick.y).toBe(box.y + GROUP_PAD);
    // Body nodes stay inside the box.
    expect(tick.x).toBeGreaterThanOrEqual(box.x);
    expect(tick.x + tick.w).toBeLessThanOrEqual(box.x + box.w);
    assertInsertCompleteness(layout, blocks);
    assertNoOverlap(layout);
  });

  it("empty loop body: box renders with one insert stub", () => {
    const blocks = [loop("poll", [])];
    const layout = layoutCanvas(blocks);
    expect(layout.groups[0]!.emptyBody).toBe(true);
    assertInsertCompleteness(layout, blocks);
  });

  it("branch as the last root block: the tail hangs below the join", () => {
    const blocks = [leaf("a"), branch("gate", [leaf("hot")], [leaf("cold")])];
    const layout = layoutCanvas(blocks);
    const tail = layout.edges.find((e) => e.kind === "tail")!;
    const gate = nodeById(layout, "gate");
    const joinY = gate.y + NODE_H + BRANCH_GAP + NODE_H + JOIN_GAP;
    expect(tail.points[0]!.y).toBe(joinY);
    expect(tail.insert).toMatchObject({ at: { root: true }, index: 2 });
    assertInsertCompleteness(layout, blocks);
    assertNoOverlap(layout);
  });

  it("a deep nested tree holds the invariants", () => {
    const blocks: BlockDef[] = [
      leaf("a"),
      branch(
        "b1",
        [branch("b2", [leaf("x")], [loop("l1", [leaf("y"), leaf("z")])]), leaf("m")],
        [],
      ),
      loop("l2", [branch("b3", [], [leaf("q")])]),
      leaf("end"),
    ];
    const layout = layoutCanvas(blocks);
    assertInsertCompleteness(layout, blocks);
    assertNoOverlap(layout);
    // Document order matches the walk order.
    expect(layout.nodes.map((n) => n.id)).toEqual([
      TRIGGER_ROW_ID,
      "a",
      "b1",
      "b2",
      "x",
      "l1",
      "y",
      "z",
      "m",
      "l2",
      "b3",
      "q",
      "end",
    ]);
    // Every insert anchor lies on its edge's polyline (x matches a vertical run).
    for (const edge of layout.edges) {
      if (!edge.insert) continue;
      const onSegment = edge.points.some((p, i) => {
        const next = edge.points[i + 1];
        if (!next) return false;
        if (p.x === next.x && p.x === edge.insert!.x) {
          const [lo, hi] = p.y < next.y ? [p.y, next.y] : [next.y, p.y];
          return edge.insert!.y >= lo && edge.insert!.y <= hi;
        }
        return false;
      });
      expect(onSegment, `insert anchor off-edge on ${edge.id}`).toBe(true);
    }
  });
});

describe("roundedOrthPath", () => {
  it("renders a straight line without curves", () => {
    expect(
      roundedOrthPath([
        { x: 0, y: 0 },
        { x: 0, y: 40 },
      ]),
    ).toBe("M 0 0 L 0 40");
  });

  it("rounds a corner with a quadratic", () => {
    const d = roundedOrthPath(
      [
        { x: 0, y: 0 },
        { x: 0, y: 20 },
        { x: 30, y: 20 },
      ],
      8,
    );
    expect(d).toContain("Q 0 20 8 20");
    expect(d).toContain("L 0 12");
  });

  it("clamps the radius on short segments", () => {
    const d = roundedOrthPath(
      [
        { x: 0, y: 0 },
        { x: 0, y: 6 },
        { x: 30, y: 6 },
      ],
      8,
    );
    // Half the 6px inbound segment, not the full 8.
    expect(d).toContain("L 0 3");
    expect(d).toContain("Q 0 6 3 6");
  });
});
