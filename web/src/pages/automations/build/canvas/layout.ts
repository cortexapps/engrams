/** Pure canvas layout for the Builder (no React).
 *
 * The block model is a strict ordered tree (branch then/else, loop body), so
 * layout is deterministic two-pass math: `measure` sizes every subtree
 * bottom-up, `place` walks top-down emitting absolutely-positioned nodes,
 * orthogonal edges, and loop group boxes. Fixed node size keeps the whole
 * function golden-testable; text truncates in the node component instead.
 *
 * Invariant the tests pin: every list of length n emits exactly n+1
 * insert-bearing edges with indexes 0..n — the canvas's "+" affordances are
 * complete iff that holds. Layout knows nothing of selection, lock state,
 * ghosts, or errors; those are render-time concerns resolved by node id.
 */

import { blockKind, type BlockDef, type ListPath } from "@/lib/automation-blocks";

export const NODE_W = 230;
export const NODE_H = 56;
/** Vertical run between sequential nodes; hosts the "+" affordance. */
export const V_GAP = 36;
/** Branch node to the top of its child columns (room for then/else labels). */
export const BRANCH_GAP = 44;
/** Bottom of a branch's children to the join waypoint. */
export const JOIN_GAP = 36;
export const COL_GAP = 32;
export const GROUP_PAD = 14;
export const LOOP_GAP = 12;
/** Lane width reserved for an empty then/else leg. */
export const EMPTY_W = 80;
/** Minimum inner height of an empty loop body. */
export const EMPTY_H = 28;
/** Append stub below the last root block. */
export const TAIL_H = 28;
export const CANVAS_PAD = 24;
export const MIN_SCALE = 0.5;
export const MAX_SCALE = 1;

export interface LayoutNode {
  id: string;
  x: number;
  y: number;
  w: number;
  h: number;
  /** The list this block lives in — the move menu's coordinates. Absent on
   * the synthesized trigger node. */
  at?: ListPath;
  index?: number;
  listLength?: number;
}

/** A loop's dashed body box; id = the loop block's id. */
export interface LayoutGroup {
  id: string;
  x: number;
  y: number;
  w: number;
  h: number;
  emptyBody: boolean;
}

export type LayoutEdgeKind = "seq" | "then" | "else" | "body" | "loop-back" | "tail";

export interface LayoutEdge {
  id: string;
  from: string;
  to: string | null;
  kind: LayoutEdgeKind;
  label?: "then" | "else";
  /** Orthogonal polyline in absolute canvas coordinates. */
  points: Array<{ x: number; y: number }>;
  /** Where a "+" inserts a block, when this edge carries one. */
  insert?: { at: ListPath; index: number; x: number; y: number };
}

export interface CanvasLayout {
  /** Document order; the first entry is the trigger node. */
  nodes: LayoutNode[];
  edges: LayoutEdge[];
  groups: LayoutGroup[];
  width: number;
  height: number;
}

export const TRIGGER_ROW_ID = "__trigger__";

interface Extent {
  w: number;
  h: number;
}

function nests(block: BlockDef): "branch" | "loop" | undefined {
  return blockKind(block.type).nests;
}

function measureBlock(block: BlockDef): Extent {
  const kind = nests(block);
  if (kind === "branch") {
    const tm = measureList(block.then ?? [], { emptyW: EMPTY_W, emptyH: 0 });
    const em = measureList(block.else ?? [], { emptyW: EMPTY_W, emptyH: 0 });
    const childrenW = tm.w + COL_GAP + em.w;
    return {
      w: Math.max(NODE_W, childrenW),
      h: NODE_H + BRANCH_GAP + Math.max(tm.h, em.h) + JOIN_GAP,
    };
  }
  if (kind === "loop") {
    const bm = measureList(block.body ?? [], { emptyW: NODE_W, emptyH: EMPTY_H });
    const groupW = Math.max(bm.w, NODE_W) + 2 * GROUP_PAD;
    const groupH = bm.h + 2 * GROUP_PAD;
    return { w: Math.max(NODE_W, groupW), h: NODE_H + LOOP_GAP + groupH };
  }
  return { w: NODE_W, h: NODE_H };
}

function measureList(list: readonly BlockDef[], empty: { emptyW: number; emptyH: number }): Extent {
  if (list.length === 0) return { w: empty.emptyW, h: empty.emptyH };
  let w = NODE_W;
  let h = 0;
  for (const block of list) {
    const m = measureBlock(block);
    w = Math.max(w, m.w);
    h += m.h;
  }
  return { w, h: h + V_GAP * (list.length - 1) };
}

interface Anchor {
  x: number;
  y: number;
  fromId: string;
}

interface Sink {
  nodes: LayoutNode[];
  edges: LayoutEdge[];
  groups: LayoutGroup[];
}

function pathKey(at: ListPath): string {
  return "root" in at ? "root" : `${at.parentId}/${at.slot}`;
}

/** A single vertical edge between two anchors on the same axis. */
function seqEdge(
  out: Sink,
  from: Anchor,
  toId: string | null,
  toY: number,
  at: ListPath,
  index: number,
  kind: LayoutEdgeKind = "seq",
): void {
  const midY = (from.y + toY) / 2;
  out.edges.push({
    id: `${kind}:${pathKey(at)}:${index}`,
    from: from.fromId,
    to: toId,
    kind,
    points: [
      { x: from.x, y: from.y },
      { x: from.x, y: toY },
    ],
    insert: { at, index, x: from.x, y: midY },
  });
}

/** Place one list along `axis` starting at `yTop`; returns the bottom
 * anchor the caller chains from (the last item's anchor), or null for an
 * empty list. Emits n-1 in-list seq edges (indexes 1..n-1); the enclosing
 * structure supplies the index-0 and index-n edges. */
function placeList(
  out: Sink,
  list: readonly BlockDef[],
  at: ListPath,
  axis: number,
  yTop: number,
): Anchor | null {
  let y = yTop;
  let prev: Anchor | null = null;
  list.forEach((block, index) => {
    if (prev !== null) {
      seqEdge(out, prev, block.id, y, at, index);
    }
    prev = placeBlock(out, block, at, index, list.length, axis, y);
    y = prev.y + V_GAP;
  });
  return prev;
}

function placeBlock(
  out: Sink,
  block: BlockDef,
  at: ListPath,
  index: number,
  listLength: number,
  axis: number,
  y: number,
): Anchor {
  out.nodes.push({
    id: block.id,
    x: axis - NODE_W / 2,
    y,
    w: NODE_W,
    h: NODE_H,
    at,
    index,
    listLength,
  });
  const kind = nests(block);
  const extent = measureBlock(block);

  if (kind === "branch") {
    const thenList = block.then ?? [];
    const elseList = block.else ?? [];
    const tm = measureList(thenList, { emptyW: EMPTY_W, emptyH: 0 });
    const em = measureList(elseList, { emptyW: EMPTY_W, emptyH: 0 });
    const childrenW = tm.w + COL_GAP + em.w;
    const left = axis - childrenW / 2;
    const thenAxis = left + tm.w / 2;
    const elseAxis = left + tm.w + COL_GAP + em.w / 2;
    const childTop = y + NODE_H + BRANCH_GAP;
    const joinY = y + extent.h;
    const fanY = y + NODE_H + BRANCH_GAP / 2;
    const joinFanY = joinY - JOIN_GAP / 2;

    const leg = (legList: readonly BlockDef[], legAxis: number, slot: "then" | "else"): void => {
      const legAt: ListPath = { parentId: block.id, slot };
      if (legList.length === 0) {
        // One fan-through edge IS the empty leg: it carries the label and
        // the leg's only insert point (index 0).
        out.edges.push({
          id: `${slot}:${block.id}:empty`,
          from: block.id,
          to: null,
          kind: slot,
          label: slot,
          points: [
            { x: axis, y: y + NODE_H },
            { x: axis, y: fanY },
            { x: legAxis, y: fanY },
            { x: legAxis, y: joinFanY },
            { x: axis, y: joinFanY },
            { x: axis, y: joinY },
          ],
          insert: { at: legAt, index: 0, x: legAxis, y: (fanY + joinFanY) / 2 },
        });
        return;
      }
      // Fan-out into the leg (insert index 0)…
      out.edges.push({
        id: `${slot}:${block.id}:0`,
        from: block.id,
        to: legList[0]!.id,
        kind: slot,
        label: slot,
        points: [
          { x: axis, y: y + NODE_H },
          { x: axis, y: fanY },
          { x: legAxis, y: fanY },
          { x: legAxis, y: childTop },
        ],
        insert: { at: legAt, index: 0, x: legAxis, y: (fanY + childTop) / 2 },
      });
      // …the leg itself (indexes 1..n-1)…
      const last = placeList(out, legList, legAt, legAxis, childTop)!;
      // …and the fan-in to the join waypoint (insert index n).
      out.edges.push({
        id: `${slot}:${block.id}:join`,
        from: last.fromId,
        to: null,
        kind: "seq",
        points: [
          { x: legAxis, y: last.y },
          { x: legAxis, y: joinFanY },
          { x: axis, y: joinFanY },
          { x: axis, y: joinY },
        ],
        insert: {
          at: legAt,
          index: legList.length,
          x: legAxis,
          y: (last.y + joinFanY) / 2,
        },
      });
    };

    leg(thenList, thenAxis, "then");
    leg(elseList, elseAxis, "else");
    return { x: axis, y: joinY, fromId: block.id };
  }

  if (kind === "loop") {
    const bodyList = block.body ?? [];
    const bodyAt: ListPath = { parentId: block.id, slot: "body" };
    const bm = measureList(bodyList, { emptyW: NODE_W, emptyH: EMPTY_H });
    const groupW = Math.max(bm.w, NODE_W) + 2 * GROUP_PAD;
    const groupH = bm.h + 2 * GROUP_PAD;
    const rect = {
      id: block.id,
      x: axis - groupW / 2,
      y: y + NODE_H + LOOP_GAP,
      w: groupW,
      h: groupH,
      emptyBody: bodyList.length === 0,
    };
    out.groups.push(rect);
    const bodyTop = rect.y + GROUP_PAD;
    const boxBottom = rect.y + rect.h;

    if (bodyList.length === 0) {
      // A stub inside the box carries the body's only insert point.
      seqEdge(
        out,
        { x: axis, y: y + NODE_H, fromId: block.id },
        null,
        bodyTop + EMPTY_H / 2,
        bodyAt,
        0,
        "body",
      );
    } else {
      seqEdge(
        out,
        { x: axis, y: y + NODE_H, fromId: block.id },
        bodyList[0]!.id,
        bodyTop,
        bodyAt,
        0,
        "body",
      );
      const last = placeList(out, bodyList, bodyAt, axis, bodyTop)!;
      // Append stub from the last body node to the box's bottom edge.
      seqEdge(out, last, null, boxBottom, bodyAt, bodyList.length);
    }
    return { x: axis, y: boxBottom, fromId: block.id };
  }

  return { x: axis, y: y + NODE_H, fromId: block.id };
}

export function layoutCanvas(blocks: readonly BlockDef[]): CanvasLayout {
  const out: Sink = { nodes: [], edges: [], groups: [] };
  const rootAt: ListPath = { root: true };
  const rootMeasure = measureList(blocks, { emptyW: NODE_W, emptyH: 0 });
  const width = Math.max(rootMeasure.w, NODE_W) + 2 * CANVAS_PAD;
  const axis = width / 2;

  const triggerY = CANVAS_PAD;
  out.nodes.push({
    id: TRIGGER_ROW_ID,
    x: axis - NODE_W / 2,
    y: triggerY,
    w: NODE_W,
    h: NODE_H,
  });
  const triggerAnchor: Anchor = { x: axis, y: triggerY + NODE_H, fromId: TRIGGER_ROW_ID };

  let bottom: Anchor;
  if (blocks.length === 0) {
    bottom = triggerAnchor;
  } else {
    const listTop = triggerY + NODE_H + V_GAP;
    seqEdge(out, triggerAnchor, blocks[0]!.id, listTop, rootAt, 0);
    bottom = placeList(out, blocks, rootAt, axis, listTop)!;
  }
  // The append affordance: a tail stub ending in nothing.
  seqEdge(out, bottom, null, bottom.y + TAIL_H, rootAt, blocks.length, "tail");

  return {
    nodes: out.nodes,
    edges: out.edges,
    groups: out.groups,
    width,
    height: bottom.y + TAIL_H + CANVAS_PAD,
  };
}

/** An orthogonal polyline as an SVG path with rounded corners. The radius
 * clamps to half the shorter adjacent segment so short runs stay valid. */
export function roundedOrthPath(points: ReadonlyArray<{ x: number; y: number }>, r = 8): string {
  if (points.length === 0) return "";
  if (points.length === 1) return `M ${points[0]!.x} ${points[0]!.y}`;
  const parts = [`M ${points[0]!.x} ${points[0]!.y}`];
  for (let i = 1; i < points.length - 1; i++) {
    const prev = points[i - 1]!;
    const corner = points[i]!;
    const next = points[i + 1]!;
    const inLen = Math.abs(corner.x - prev.x) + Math.abs(corner.y - prev.y);
    const outLen = Math.abs(next.x - corner.x) + Math.abs(next.y - corner.y);
    const radius = Math.min(r, inLen / 2, outLen / 2);
    if (radius <= 0) {
      parts.push(`L ${corner.x} ${corner.y}`);
      continue;
    }
    const inX = corner.x - Math.sign(corner.x - prev.x) * radius;
    const inY = corner.y - Math.sign(corner.y - prev.y) * radius;
    const outX = corner.x + Math.sign(next.x - corner.x) * radius;
    const outY = corner.y + Math.sign(next.y - corner.y) * radius;
    parts.push(`L ${inX} ${inY}`);
    parts.push(`Q ${corner.x} ${corner.y} ${outX} ${outY}`);
  }
  const last = points[points.length - 1]!;
  parts.push(`L ${last.x} ${last.y}`);
  return parts.join(" ");
}
