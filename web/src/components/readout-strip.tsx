import type { CSSProperties, ReactNode } from "react";

import { cn } from "@/lib/utils";
import type { StatusTone } from "./status-dot";
import { TickNumber } from "./tick-number";

// THE rollup: one card, hairline-separated cells, each a caption over a mono
// figure with an optional sub-line or meter. Fleet's health strip and Storage's
// rollup are the same object with different cells; neither is a row of cards.
//
// Cells stack on a phone (hairlines between rows) and sit side by side from
// `md` (hairlines between columns). The grid tracks are the caller's, so a
// verdict cell can take more room than a figure.

export function ReadoutStrip({
  columns,
  children,
  className,
}: {
  /** `grid-template-columns` from `md` up; below that, one column. */
  columns: string;
  children: ReactNode;
  className?: string;
}) {
  return (
    <section
      data-slot="readout-strip"
      className={cn(
        "grid grid-cols-1 divide-y divide-border rounded-lg border bg-card md:grid-cols-(--strip-columns) md:divide-x md:divide-y-0",
        className,
      )}
      style={{ "--strip-columns": columns } as CSSProperties}
    >
      {children}
    </section>
  );
}

/** A figure under a caption: label 12/600 muted · mono 20 · mono 12 muted
 * sub-line, or a meter. `tint` washes the cell in an instrument tone at 8% —
 * for the one cell that carries a verdict. */
export function ReadoutCell({
  label,
  value,
  sub,
  tint,
  className,
  children,
}: {
  label: ReactNode;
  value?: ReactNode;
  sub?: ReactNode;
  tint?: Extract<StatusTone, "caution" | "critical">;
  className?: string;
  children?: ReactNode;
}) {
  return (
    <div
      data-slot="readout-cell"
      className={cn("flex min-w-0 flex-col gap-1.5 px-[18px] py-4", className)}
      style={
        tint
          ? {
              backgroundColor: `color-mix(in oklch, var(--color-instrument-${tint}) 8%, transparent)`,
            }
          : undefined
      }
    >
      <div className="text-xs font-semibold text-muted-foreground">{label}</div>
      {value !== undefined && (
        <div className="font-mono text-lg leading-none tabular-nums text-foreground">
          <TickNumber value={value} />
        </div>
      )}
      {children}
      {sub !== undefined && (
        <div className="truncate font-mono text-xs tabular-nums text-muted-foreground">{sub}</div>
      )}
    </div>
  );
}
