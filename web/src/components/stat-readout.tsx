import type { ReactNode } from "react";
import { cn } from "@/lib/utils";
import { Text } from "@/components/ui/text";

export interface Stat {
  label: string;
  value: ReactNode;
}

// An instrument readout, not a row of cards. Figures are mono and oversized,
// labels are small Saira caps beneath them (the instrument-label voice — the
// figure/label contrast is the point), and a single hairline grid (drawn
// with a 1px gap over the border color) separates the cells like a gauge
// cluster. Responsive by column count passed in `className`.
export function StatReadout({ items, className }: { items: Stat[]; className?: string }) {
  return (
    <dl
      className={cn(
        "grid grid-cols-2 gap-px overflow-hidden rounded-md border bg-border sm:grid-cols-4",
        className,
      )}
    >
      {items.map(({ label, value }) => (
        <div key={label} className="bg-card px-4 py-3">
          {/* Figure dominates (full ink, oversized mono); caption recedes
              beneath it (quiet Saira caps, muted). The variant/tone split is
              the hierarchy. */}
          <Text as="dd" variant="stat" tone="default">
            {value}
          </Text>
          <Text as="dt" variant="label" tone="muted" className="mt-1.5 text-[0.65rem]">
            {label}
          </Text>
        </div>
      ))}
    </dl>
  );
}
