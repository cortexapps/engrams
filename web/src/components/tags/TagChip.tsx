/** One tag, drawn the same way on every page: a descriptor chip tinted from
 * the tag's color (see `.tag-chip`). It is never a button or a link. */

import type { CSSProperties } from "react";

import type { Tag } from "@/gen/engram/app/v1/tag_pb";
import { cn } from "@/lib/utils";

export function TagChip({
  tag,
  className,
}: {
  tag: Pick<Tag, "name" | "color" | "description">;
  className?: string;
}) {
  const style: CSSProperties & { "--tag-color": string } = { "--tag-color": tag.color };
  return (
    <span
      className={cn(
        "tag-chip inline-block max-w-32 min-w-0 truncate rounded-sm px-1.5 py-0.5 align-middle text-xs leading-4 font-medium",
        className,
      )}
      style={style}
      // A long name truncates, so the tooltip always carries the full name.
      title={tag.description ? `${tag.name}: ${tag.description}` : tag.name}
    >
      {tag.name}
    </span>
  );
}

/** A tag's color as a small square, drawn by the same rule as its chip, for
 * places that list tags by name (the filter menu). */
export function TagSwatch({ color }: { color: string }) {
  const style: CSSProperties & { "--tag-color": string } = { "--tag-color": color };
  return <span aria-hidden className="tag-chip inline-block size-3 rounded-[3px]" style={style} />;
}
