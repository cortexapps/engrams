/** A row's tags in a narrow space: the first few chips, then a "+N" control
 * whose tooltip and accessible name list every tag. The row is a size
 * container: below 11rem it shows only the first chip and a "+N" that counts
 * every other tag, so no tag shrinks to a stub. */

import type { Tag } from "@/gen/engram/app/v1/tag_pb";
import { Tooltip, TooltipContent, TooltipProvider, TooltipTrigger } from "@/components/ui/tooltip";
import { orderTagsForDisplay } from "@/lib/tag-filter";
import { cn } from "@/lib/utils";
import { TagChip } from "./TagChip";

/** Hidden while the row is narrower than 11rem. */
const WIDE_ONLY = "@max-[11rem]:hidden";
/** Shown only while the row is narrower than 11rem. */
const NARROW_ONLY = "hidden @max-[11rem]:inline-flex";

function MoreTags({
  count,
  names,
  className,
}: {
  count: number;
  names: string;
  className: string;
}) {
  return (
    <Tooltip>
      <TooltipTrigger asChild>
        <button
          type="button"
          aria-label={`+${count} more. All tags: ${names}`}
          className={cn(
            "shrink-0 rounded-sm border px-1.5 py-0.5 text-xs leading-4 font-medium text-muted-foreground outline-none focus-visible:ring-2 focus-visible:ring-ring/50",
            className,
          )}
        >
          +{count}
        </button>
      </TooltipTrigger>
      <TooltipContent>{names}</TooltipContent>
    </Tooltip>
  );
}

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
  const names = ordered.map((tag) => tag.name).join(", ");

  return (
    <span className="@container block min-w-0">
      <span className="flex min-w-0 items-center gap-1">
        <TooltipProvider delayDuration={200}>
          {/* The first chip keeps its width beside a second chip, which
              truncates first; alone in a narrow cell it shrinks, so the +N
              never spills out of the cell. */}
          {ordered.slice(0, max).map((tag, index) => (
            <TagChip
              key={tag.id}
              tag={tag}
              className={index === 0 ? "shrink-0 @max-[11rem]:shrink" : WIDE_ONLY}
            />
          ))}
          {ordered.length > max && (
            <MoreTags count={ordered.length - max} names={names} className={WIDE_ONLY} />
          )}
          {ordered.length > 1 && (
            <MoreTags count={ordered.length - 1} names={names} className={NARROW_ONLY} />
          )}
        </TooltipProvider>
      </span>
    </span>
  );
}
