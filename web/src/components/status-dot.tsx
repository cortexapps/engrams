import { cn } from "@/lib/utils";

// THE status dot. One shape, one size ladder, one colour vocabulary — the
// instrument tones for state, `--ring` for "running" (the one sanctioned
// exception to lime-is-action, and only at night does ring resolve to lime),
// faded ink for "nothing to report". Never a glow: a shadow with no offset
// reads as focus, not as height.
//
// A dot is never alone: the word beside it carries the meaning for anyone
// who cannot read the colour. Pass `label` when there is no adjacent text.
export type StatusTone = "nominal" | "caution" | "critical" | "active" | "muted";

const FILL: Record<StatusTone, string> = {
  nominal: "bg-instrument-nominal",
  caution: "bg-instrument-caution",
  critical: "bg-instrument-critical",
  active: "bg-ring animate-pulse motion-reduce:animate-none",
  muted: "bg-muted-foreground/30",
};

const SIZE = {
  6: "size-1.5",
  8: "size-2",
  10: "size-2.5",
} as const;

export function StatusDot({
  tone,
  size = 8,
  label,
  className,
}: {
  tone: StatusTone;
  /** Diameter in px: 6 in dense rows and rails, 8 in lists, 10 on a masthead. */
  size?: keyof typeof SIZE;
  /** Accessible name when no visible word sits beside the dot. */
  label?: string;
  className?: string;
}) {
  return (
    <span
      role={label ? "img" : undefined}
      aria-label={label}
      aria-hidden={label ? undefined : true}
      data-slot="status-dot"
      data-tone={tone}
      className={cn("inline-block shrink-0 rounded-full", SIZE[size], FILL[tone], className)}
    />
  );
}
