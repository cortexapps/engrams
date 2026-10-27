/** The canvas's single SVG: the dashed lanes (a loop's body, a branch's two
 * legs), the edge paths, and each lane's label tab. Deliberately
 * non-interactive (`pointer-events-none`) — every interactive element (nodes,
 * "+") is HTML layered above, which sidesteps SVG focus and a11y entirely.
 *
 * Edges are 1.5px ink at 45%, orthogonal with 8px rounding; the tail run to
 * the "Add step" node is dashed, as is a loop's back edge. */

import { roundedOrthPath, type CanvasLayout, type LayoutGroup } from "./layout";

const EDGE_STROKE = "color-mix(in oklch, var(--color-foreground) 45%, transparent)";
const LANE_STROKE = "color-mix(in oklch, var(--color-foreground) 28%, transparent)";

function laneLabel(group: LayoutGroup): string {
  return group.kind === "loop" ? "repeat until …" : group.kind;
}

const TAKEN_STROKE = "var(--color-ring)";

export function EdgeLayer({
  layout,
  taken,
}: {
  layout: CanvasLayout;
  /** Replay: the edges the run travelled; every other edge fades. */
  taken?: (edgeId: string) => boolean;
}) {
  return (
    <>
      <svg
        width={layout.width}
        height={layout.height}
        className="pointer-events-none absolute inset-0"
        aria-hidden
      >
        {layout.groups.map((group) => (
          <rect
            key={`group-${group.id}`}
            x={group.x}
            y={group.y}
            width={group.w}
            height={group.h}
            rx={14}
            fill="none"
            stroke={LANE_STROKE}
            strokeWidth={1}
            strokeDasharray="4 4"
          />
        ))}
        {layout.edges.map((edge) => {
          const on = taken ? taken(edge.id) : undefined;
          return (
            <path
              key={edge.id}
              d={roundedOrthPath(edge.points)}
              stroke={on ? TAKEN_STROKE : EDGE_STROKE}
              strokeWidth={on ? 2.25 : 1.5}
              strokeDasharray={
                edge.kind === "tail" || edge.kind === "loop-back" ? "4 4" : undefined
              }
              opacity={on === false ? 0.35 : 1}
              fill="none"
              data-taken={on ? "true" : undefined}
            />
          );
        })}
      </svg>
      {/* The lane's label sits on a small `--background` tab over the lane's
          top-left corner, so it reads as the bracket's caption rather than as
          a node. HTML, so the type scale applies. */}
      {layout.groups.map((group) => (
        <span
          key={`label-${group.id}`}
          className="pointer-events-none absolute z-10 rounded-sm bg-background px-1.5 text-2xs font-semibold leading-4 text-muted-foreground"
          style={{ left: group.x + 8, top: group.y - 8 }}
          aria-hidden
        >
          {laneLabel(group)}
        </span>
      ))}
    </>
  );
}
