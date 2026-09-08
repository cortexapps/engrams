import type { ReactNode } from "react";
import { TriangleAlert } from "lucide-react";

import { cn } from "@/lib/utils";

// THE empty state. A dashed card holding one quiet sentence and, at most, one
// action — the same object whether the list is empty, the filter matched
// nothing, or the load failed. An error keeps the shape and adds the alert
// glyph in ink: status is glyph + words, never a coloured sentence.
//
// `inline` drops the card for surfaces that already are one (a rail, a table
// cell), where a dashed box inside a box would read as a second object.
export function EmptyState({
  children,
  action,
  tone = "default",
  inline = false,
  className,
}: {
  /** One sentence. */
  children: ReactNode;
  /** A single control, when there is one obvious next thing to do. */
  action?: ReactNode;
  tone?: "default" | "error";
  inline?: boolean;
  className?: string;
}) {
  const error = tone === "error";
  return (
    <div
      role={error ? "alert" : undefined}
      data-slot="empty-state"
      data-tone={tone}
      className={cn(
        "flex flex-col items-center gap-3 text-center",
        !inline && "rounded-lg border border-dashed px-6 py-8",
        inline && "items-start px-2 py-1.5 text-left",
        className,
      )}
    >
      <p
        className={cn(
          "flex max-w-prose items-center gap-1.5 text-xs",
          error ? "text-foreground" : "text-muted-foreground",
        )}
      >
        {error && <TriangleAlert className="size-3.5 shrink-0" aria-hidden />}
        <span>{children}</span>
      </p>
      {action}
    </div>
  );
}
