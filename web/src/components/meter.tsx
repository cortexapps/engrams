import { cn } from "@/lib/utils";
import type { StatusTone } from "./status-dot";

// THE meter: a 6px bar whose fill is an instrument tone, never lime and never
// `--destructive`. The thresholds are the product's, not the caller's:
//
//   usage    (disk, memory, cpu, capacity — higher is worse):
//            nominal below 70%, caution from 70%, critical from 90%.
//   locality (base chunks resident on the host — higher is better):
//            nominal from 80%, caution from 50%, critical below.
//
// `value` is 0–100, or null when there is no reading; a null meter draws an
// empty track rather than a zero, because "no data" is not "0%".

export function usageTone(pct: number): StatusTone {
  if (pct >= 90) return "critical";
  if (pct >= 70) return "caution";
  return "nominal";
}

export function localityTone(pct: number): StatusTone {
  if (pct >= 80) return "nominal";
  if (pct >= 50) return "caution";
  return "critical";
}

const FILL: Record<StatusTone, string> = {
  nominal: "bg-instrument-nominal",
  caution: "bg-instrument-caution",
  critical: "bg-instrument-critical",
  active: "bg-ring",
  muted: "bg-muted-foreground/30",
};

export function Meter({
  value,
  tone,
  label,
  className,
}: {
  value: number | null;
  /** Defaults to the usage thresholds; pass `localityTone(v)` for locality. */
  tone?: StatusTone;
  /** Accessible name, e.g. "disk". */
  label?: string;
  className?: string;
}) {
  const pct = value === null ? null : Math.min(100, Math.max(0, Math.round(value)));
  const fill = pct === null ? "muted" : (tone ?? usageTone(pct));
  return (
    <span
      role="meter"
      aria-label={label}
      aria-valuemin={0}
      aria-valuemax={100}
      aria-valuenow={pct ?? undefined}
      data-slot="meter"
      data-tone={fill}
      className={cn("block h-1.5 w-full overflow-hidden rounded-[3px] bg-border", className)}
    >
      {pct !== null && (
        <span
          className={cn("block h-full rounded-[3px]", FILL[fill])}
          style={{ width: `${pct}%` }}
        />
      )}
    </span>
  );
}
