// THE relative-time module. Two voices from one clock, so no surface appends
// its own " ago" or invents a third format:
//
//   relativeTime — the sentence voice: "4m ago", "in 2h", "just now",
//                  "any moment", "never"; the date past a month.
//   relativeAge  — the terse voice for dense columns and rails: "12s", "3m",
//                  "2h", "5d"; "—" when there is no timestamp.
//
// `now` is epoch milliseconds (what `useNow` yields), never a Date.

const MINUTE = 60;
const HOUR = 60 * MINUTE;
const DAY = 24 * HOUR;
const MONTH = 31 * DAY;

function parse(iso: string | null | undefined): number | null {
  if (!iso) return null;
  const t = new Date(iso).getTime();
  return Number.isNaN(t) ? null : t;
}

/** Past or future, symmetric: "4m ago" / "in 4m". Within a minute either way
 * reads as "just now" / "any moment"; beyond a month, the date. */
export function relativeTime(iso: string | null | undefined, now: number = Date.now()): string {
  if (!iso) return "never";
  const then = parse(iso);
  if (then === null) return iso;
  const delta = Math.round((now - then) / 1000);
  const future = delta < 0;
  const s = Math.abs(delta);
  if (s < MINUTE) return future ? "any moment" : "just now";
  const unit = (n: number, suffix: string) => (future ? `in ${n}${suffix}` : `${n}${suffix} ago`);
  const m = Math.round(s / MINUTE);
  if (m < 60) return unit(m, "m");
  const h = Math.round(m / 60);
  if (h < 24) return unit(h, "h");
  const d = Math.round(h / 24);
  if (s < MONTH) return unit(d, "d");
  return new Date(then).toISOString().slice(0, 10);
}

/** The terse age of a past timestamp: "12s", "3m", "2h", "5d". A future or
 * missing timestamp reads "—" — a column of ages has no room for a sentence. */
export function relativeAge(iso: string | null | undefined, now: number = Date.now()): string {
  const then = parse(iso);
  if (then === null) return "—";
  const dt = Math.max(0, (now - then) / 1000);
  if (dt < MINUTE) return `${Math.floor(dt)}s`;
  if (dt < HOUR) return `${Math.floor(dt / MINUTE)}m`;
  if (dt < DAY) return `${Math.floor(dt / HOUR)}h`;
  return `${Math.floor(dt / DAY)}d`;
}
