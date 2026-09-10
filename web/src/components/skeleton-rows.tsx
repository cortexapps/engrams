import { cn } from "@/lib/utils";

// THE loading state for a list or a table: placeholder bars in the column
// geometry of the data that is coming, so the page does not reflow when it
// lands. Replaces every "Loading…" sentence. Pass the coming table's
// `grid-template-columns` tracks as `columns`; a single track makes a list.
//
// Motion: the hairlines between rows draw in first (900ms, 150ms apart), then
// the bars shimmer at 1.8s. Bar widths vary by a fixed pattern (not at
// random) so a re-render never makes the placeholder twitch.
const WIDTHS = ["72%", "48%", "61%", "39%", "55%"];

export function SkeletonRows({
  rows = 3,
  columns = ["minmax(0, 1fr)"],
  className,
}: {
  rows?: number;
  columns?: string[];
  className?: string;
}) {
  return (
    <div
      role="status"
      aria-busy="true"
      aria-label="Loading"
      data-slot="skeleton-rows"
      className={cn("shimmer-container flex flex-col [--shimmer-duration:1800ms]", className)}
    >
      {Array.from({ length: rows }, (_, r) => (
        <div
          key={r}
          className="relative grid items-center gap-4 py-3.5"
          style={{ gridTemplateColumns: columns.join(" "), "--i": r } as React.CSSProperties}
        >
          {r > 0 && (
            <span aria-hidden className="hairline-draw absolute inset-x-0 top-0 h-px bg-border" />
          )}
          {columns.map((_, c) => (
            <span
              key={c}
              className="shimmer-bg block h-2.5 rounded-[3px] bg-accent"
              style={{ width: WIDTHS[(r * columns.length + c) % WIDTHS.length] }}
            />
          ))}
        </div>
      ))}
    </div>
  );
}
