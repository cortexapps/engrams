/** A row's tags in a narrow space: the first few chips, then a "+N" control
 * whose tooltip and accessible name list every tag. */

import type { Tag } from "@/gen/engram/app/v1/tag_pb";
import { Tooltip, TooltipContent, TooltipProvider, TooltipTrigger } from "@/components/ui/tooltip";
import { orderTagsForDisplay } from "@/lib/tag-filter";
import { TagChip } from "./TagChip";

export function TagChipRow({
  tags,
  selected = [],
  max = 2,
}: {
  tags: readonly Pick<Tag, "id" | "name" | "color" | "description">[];
  /** Filter selection; these tags come first. */
  selected?: readonly string[];
  max?: number;
}) {
  if (tags.length === 0) return null;
  const ordered = orderTagsForDisplay(tags, selected);
  const hidden = ordered.length - max;
  const names = ordered.map((tag) => tag.name).join(", ");

  return (
    <span className="flex min-w-0 items-center gap-1">
      {/* The first chip keeps its width; the next ones truncate first, so a
          long neighbour never squeezes the leading tag to "…". */}
      {ordered.slice(0, max).map((tag, index) => (
        <TagChip key={tag.id} tag={tag} className={index === 0 ? "shrink-0" : undefined} />
      ))}
      {hidden > 0 && (
        <TooltipProvider delayDuration={200}>
          <Tooltip>
            <TooltipTrigger asChild>
              <button
                type="button"
                aria-label={`+${hidden} more. All tags: ${names}`}
                className="shrink-0 rounded-sm border px-1.5 py-0.5 text-xs leading-4 font-medium text-muted-foreground outline-none focus-visible:ring-2 focus-visible:ring-ring/50"
              >
                +{hidden}
              </button>
            </TooltipTrigger>
            <TooltipContent>{names}</TooltipContent>
          </Tooltip>
        </TooltipProvider>
      )}
    </span>
  );
}
