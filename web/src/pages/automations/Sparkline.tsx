import type { DayRunCount } from "@/gen/engram/app/v1/automation_pb";

/** Every run the server counted for the day. `other` is the server's bucket
 * for pending/running/waiting/superseded/halted — a superseding automation
 * can have whole days that are other-only, and those days RAN. */
export function dayTotal(day: DayRunCount): number {
  return day.completed + day.failed + day.filtered + day.other;
}

/** Colour class for one day's bar. Failures win; every other run reads as
 * nominal activity, including a day that only has in-flight or superseded
 * entries. */
export function dayTone(day: DayRunCount): "nominal" | "critical" | "muted" {
  if (dayTotal(day) === 0) return "muted";
  if (day.failed > 0) return "critical";
  return "nominal";
}

const FILL: Record<ReturnType<typeof dayTone>, string> = {
  nominal: "fill-instrument-nominal",
  critical: "fill-instrument-critical",
  muted: "fill-border",
};

const W = 47;
const H = 18;
const BAR_WIDTH = 5;
const GAP = 2;

const UTC_DAY_MS = 86_400_000;

function utcDay(ms: number): string {
  return new Date(ms).toISOString().slice(0, 10);
}

/** The server reports only the days that had runs; the widget promises seven
 * calendar-aligned slots, newest on the right. Build that window here: the
 * last seven UTC dates ending today, each taking its reported bucket or a
 * zero bucket. Exported for the test. */
export function sevenDayWindow(
  days: readonly DayRunCount[],
  now: Date = new Date(),
): DayRunCount[] {
  const byDay = new Map(days.map((d) => [d.day, d]));
  const todayMs = Date.UTC(now.getUTCFullYear(), now.getUTCMonth(), now.getUTCDate());
  return Array.from({ length: 7 }, (_, i) => {
    const day = utcDay(todayMs - (6 - i) * UTC_DAY_MS);
    return (
      byDay.get(day) ?? {
        $typeName: "engram.app.v1.DayRunCount",
        day,
        completed: 0,
        failed: 0,
        filtered: 0,
        other: 0,
      }
    );
  });
}

/** Seven bars, newest on the right. Height ∝ that day's run count; a zero
 * day keeps a 2px stub so the week reads as seven slots. */
export function Sparkline({
  days,
  label,
  now,
}: {
  days: DayRunCount[];
  label?: string;
  /** Test seam; defaults to the wall clock. */
  now?: Date;
}) {
  const week = sevenDayWindow(days, now);
  const max = Math.max(1, ...week.map(dayTotal));
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
        const total = dayTotal(day);
        const h = total === 0 ? 2 : Math.max(3, Math.round((total / max) * H));
        return (
          <rect
            key={day.day || i}
            data-tone={dayTone(day)}
            x={i * (BAR_WIDTH + GAP)}
            y={H - h}
            width={BAR_WIDTH}
            height={h}
            rx={1}
            className={FILL[dayTone(day)]}
          />
        );
      })}
    </svg>
  );
}
