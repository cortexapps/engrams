import { Skeleton } from "@/components/ui/skeleton";
import { cn } from "@/lib/utils";

// THE loading state for a list or a table: placeholder bars in the column
// geometry of the data that is coming, so the page does not reflow when it
// lands. Replaces every "Loading…" sentence. Pass the coming table's
// `grid-template-columns` tracks as `columns`; a single track makes a list.
//
// Bar widths vary by a fixed pattern (not at random) so a re-render never
// makes the placeholder twitch.
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
      className={cn("flex flex-col divide-y divide-border", className)}
    >
      {Array.from({ length: rows }, (_, r) => (
        <div
          key={r}
          className="grid items-center gap-4 py-3.5"
          style={{ gridTemplateColumns: columns.join(" ") }}
        >
          {columns.map((_, c) => (
            <Skeleton
              key={c}
              className="h-2.5 rounded-[3px]"
              style={{ width: WIDTHS[(r * columns.length + c) % WIDTHS.length] }}
            />
          ))}
        </div>
      ))}
    </div>
  );
}
