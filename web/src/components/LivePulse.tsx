import { cn } from "@/lib/utils";

/**
 * The "this is happening right now" dot — a soft ping behind a solid core.
 *
 * Extracted because it was hand-rolled three times across the reviews page, each
 * copy pinned to `emerald-500`. State colour belongs to the instrument palette,
 * so it reads as the same signal as every other nominal indicator and survives
 * a theme change. It is decoration only: every caller pairs it with a word.
 */
export function LivePulse({ className }: { className?: string }) {
  return (
    <span className={cn("relative flex size-1.5", className)} aria-hidden>
      <span
        className="absolute inline-flex size-full animate-ping rounded-full opacity-70 motion-reduce:animate-none"
        style={{ backgroundColor: "var(--instrument-nominal)" }}
      />
      <span
        className="relative inline-flex size-1.5 rounded-full"
        style={{ backgroundColor: "var(--instrument-nominal)" }}
      />
    </span>
  );
}
