// Shared formatting helpers for the diagnostic surfaces (COW state,
// the Storage durability ledger). Kept tiny and dependency-free.

/** Human byte size with binary units, e.g. `1.5 MiB`. `0 B` for zero. */
export function fmtBytes(n: number): string {
  if (n === 0) return "0 B";
  const units = ["B", "KiB", "MiB", "GiB", "TiB"];
  let value = n;
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024;
    unit += 1;
  }
  return `${value.toFixed(value >= 100 ? 0 : 1)} ${units[unit]}`;
}

/** Seconds since `iso` (for thresholding); `Infinity` for null. */
export function secondsSince(iso: string | null): number {
  if (!iso) return Infinity;
  return Math.max(0, Math.floor((Date.now() - new Date(iso).getTime()) / 1000));
}

/** Render a long id as `prefix…`; short ids pass through. */
export function shortId(id: string): string {
  return id.length <= 12 ? id : `${id.slice(0, 8)}…`;
}
