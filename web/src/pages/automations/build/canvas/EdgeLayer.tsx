/** The canvas's single SVG: loop group boxes, edge paths, then/else labels,
 * and the loop Repeat glyph. Deliberately non-interactive
 * (`pointer-events-none`) — every interactive element (nodes, "+") is HTML
 * layered above, which sidesteps SVG focus and a11y entirely. */

import { Repeat } from "lucide-react";

import { roundedOrthPath, type CanvasLayout } from "./layout";

export function EdgeLayer({ layout }: { layout: CanvasLayout }) {
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
            rx={10}
            className="fill-muted/30 stroke-border"
            strokeDasharray="4 4"
          />
        ))}
        {layout.edges.map((edge) => (
          <path
            key={edge.id}
            d={roundedOrthPath(edge.points)}
            className={edge.kind === "tail" ? "stroke-border/70" : "stroke-border"}
            strokeWidth={1.25}
            strokeDasharray={edge.kind === "loop-back" ? "4 4" : undefined}
            fill="none"
          />
        ))}
        {layout.edges
          .filter((edge) => edge.label)
          .map((edge) => {
            // The label sits above the fan-out's horizontal segment.
            const fan = edge.points[1]!;
            const target = edge.points[2] ?? fan;
            const x = (fan.x + target.x) / 2;
            return (
              <text
                key={`label-${edge.id}`}
                x={x}
                y={fan.y - 5}
                textAnchor="middle"
                className="fill-muted-foreground font-mono text-2xs"
              >
                {edge.label}
              </text>
            );
          })}
      </svg>
      {layout.groups.map((group) => (
        <span
          key={`repeat-${group.id}`}
          className="text-muted-foreground pointer-events-none absolute z-10"
          style={{ left: group.x + 6, top: group.y + group.h - 20 }}
          aria-hidden
        >
          <Repeat className="size-3.5" />
        </span>
      ))}
    </>
  );
}
