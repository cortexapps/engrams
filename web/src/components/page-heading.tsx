import type { ReactNode } from "react";
import { cn } from "@/lib/utils";
import { Text } from "@/components/ui/text";

// Every page opens the same way: the page's name, what there is of it, and
// what you can do here. Nothing else — no kicker over the title, no sentence
// explaining a page to someone already standing on it, no closing rule (the
// content below draws its own top edge).
//
// `count` is the one fact a list page owes you before you scroll.
// `titleVariant="mono"` swaps to the machine-data voice when the title IS an id
// or a digest rather than prose.
export function PageHeading({
  title,
  count,
  actions,
  titleVariant = "display",
  className,
}: {
  title: ReactNode;
  /** A short readout of what this page holds — "8 documents", "24 hosts". */
  count?: ReactNode;
  actions?: ReactNode;
  titleVariant?: "display" | "mono";
  className?: string;
}) {
  return (
    <div
      className={cn("flex flex-wrap items-center justify-between gap-x-6 gap-y-3", className)}
      data-slot="page-heading"
    >
      {/* `min-w-0` drops the min-content floor so a long title truncates
          instead of pushing its own controls off the edge; `basis-80 grow`
          keeps the actions on the title's baseline until the band itself is
          narrower than the title asks for (a phone). */}
      <div className="flex min-w-0 grow basis-80 items-center gap-3">
        <Text as="h1" variant={titleVariant === "mono" ? "displayMono" : "display"}>
          {title}
        </Text>
        {count && (
          <span className="shrink-0 rounded-full bg-secondary px-2 py-0.5 font-mono text-xs tabular-nums text-muted-foreground">
            {count}
          </span>
        )}
      </div>
      {actions && <div className="flex shrink-0 items-center gap-2">{actions}</div>}
    </div>
  );
}
