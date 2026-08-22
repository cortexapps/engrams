import type { DayRunCount } from "@/gen/engram/app/v1/automation_pb";

/** Colour class for one day's bar: instrument tokens only (lime is a fill
 * reserved for the accent, never a status — web/DESIGN.md). */
export function dayTone(day: DayRunCount): "nominal" | "caution" | "critical" | "muted" {
  const total = day.completed + day.failed + day.filtered;
  if (total === 0) return "muted";
  if (day.failed > 0 && day.completed === 0) return "critical";
  if (day.failed > 0) return "caution";
  return "nominal";
}

const FILL: Record<ReturnType<typeof dayTone>, string> = {
  nominal: "fill-instrument-nominal",
  caution: "fill-instrument-caution",
  critical: "fill-instrument-critical",
  muted: "fill-muted-foreground/30",
};

const W = 84;
const H = 24;
const GAP = 2;

/** Seven bars, newest on the right. Height ∝ that day's run count; a zero
 * day keeps a 2px stub so the week reads as seven slots. */
export function Sparkline({ days, label }: { days: DayRunCount[]; label?: string }) {
  const week = days.slice(-7);
  const slot = (W - GAP * 6) / 7;
  const max = Math.max(1, ...week.map((d) => d.completed + d.failed + d.filtered));
  return (
    <svg
      width={W}
      height={H}
      viewBox={`0 0 ${W} ${H}`}
      role="img"
      aria-label={label ?? "Runs over the last 7 days"}
      className="shrink-0"
    >
      {week.map((day, i) => {
        const total = day.completed + day.failed + day.filtered;
        const h = total === 0 ? 2 : Math.max(3, Math.round((total / max) * H));
        return (
          <rect
            key={day.day || i}
            data-tone={dayTone(day)}
            x={i * (slot + GAP)}
            y={H - h}
            width={slot}
            height={h}
            rx={1}
            className={FILL[dayTone(day)]}
          />
        );
      })}
    </svg>
  );
}
