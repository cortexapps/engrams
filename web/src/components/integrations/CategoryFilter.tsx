/**
 * Category filter for the Integrations marketplace (design handoff).
 *
 * Replaces the horizontal tab strip (which ran off the viewport once the
 * catalog grew past ~16 categories) with a compact dropdown that lives inline
 * with the search field: the active category + a live provider count, opening a
 * popover of every category with its count. Scales to any number of categories
 * without overflowing.
 *
 * Built on the shared shadcn `DropdownMenu` (Radix) so open/close, click-outside,
 * Escape, focus and positioning come from the design system — no hand-rolled
 * listeners. Styled entirely with theme tokens + `textVariants` (the same
 * instrument-label voice the old TabRow used); no raw CSS / inline styles.
 */

import { ChevronDownIcon, LayersIcon } from "lucide-react";

import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { textVariants } from "@/components/ui/text";
import { cn } from "@/lib/utils";

export interface CategoryFilterProps {
  /** Ordered category ids, `"all"` first (the parent's first-seen order). */
  cats: string[];
  /** Provider count per category id, including `counts.all` (the total). */
  counts: Record<string, number>;
  /** Selected category id; `"all"` is the default sentinel. */
  active: string;
  onChange: (id: string) => void;
}

const labelOf = (id: string) => (id === "all" ? "All categories" : id);

export function CategoryFilter({ cats, counts, active, onChange }: CategoryFilterProps) {
  return (
    <DropdownMenu>
      <DropdownMenuTrigger
        aria-label="Filter by category"
        className="group inline-flex h-9 shrink-0 items-center gap-2.5 rounded-md border bg-secondary px-3 text-foreground outline-none focus-visible:ring-2 focus-visible:ring-ring"
      >
        <LayersIcon className="size-3.5 text-muted-foreground" />
        <span className={cn(textVariants({ variant: "label" }), "whitespace-nowrap")}>
          {labelOf(active)}
        </span>
        <span className="font-mono text-[0.66rem] tabular-nums text-muted-foreground">
          {counts[active] ?? 0}
        </span>
        <ChevronDownIcon className="size-3.5 text-muted-foreground transition-transform group-data-[state=open]:rotate-180" />
      </DropdownMenuTrigger>

      <DropdownMenuContent align="end" className="max-h-[360px] min-w-[240px] p-1.5 shadow-lg">
        {cats.map((c) => {
          const isActive = c === active;
          return (
            <DropdownMenuItem
              key={c}
              onSelect={() => onChange(c)}
              className={cn(
                "cursor-pointer gap-2.5 px-2.5 py-2",
                isActive ? "bg-secondary text-foreground" : "text-muted-foreground",
              )}
            >
              {/* status dot: lime fill when active, hairline ring otherwise */}
              <span
                className={cn(
                  "size-1.5 shrink-0 rounded-full",
                  isActive ? "bg-primary" : "border border-border",
                )}
              />
              <span className={cn(textVariants({ variant: "label" }), "flex-1")}>{labelOf(c)}</span>
              <span className="font-mono text-[0.68rem] tabular-nums text-muted-foreground/75">
                {counts[c] ?? 0}
              </span>
            </DropdownMenuItem>
          );
        })}
      </DropdownMenuContent>
    </DropdownMenu>
  );
}
