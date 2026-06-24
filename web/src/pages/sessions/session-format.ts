import type { SessionState } from "../../lib/types";

export function shortId(id: string): string {
  return id.length <= 12 ? id : `${id.slice(0, 8)}…`;
}
export function stripImageHost(uri: string): string {
  const slash = uri.indexOf("/");
  const colon = uri.lastIndexOf(":");
  const start = slash >= 0 ? slash + 1 : 0;
  const end = colon > start ? colon : uri.length;
  return uri.slice(start, end);
}
export function relativeTime(iso: string): string {
  const t = new Date(iso).getTime();
  const dt = Math.max(0, (Date.now() - t) / 1000);
  if (dt < 60) return `${Math.floor(dt)}s`;
  if (dt < 3600) return `${Math.floor(dt / 60)}m`;
  if (dt < 86400) return `${Math.floor(dt / 3600)}h`;
  return `${Math.floor(dt / 86400)}d`;
}
// Transitional suspend/relocate (ADR 0018 / ADR 0034) still count as
// "happening now" — they re-bucket to idle/active within a couple minutes.
const ACTIVEISH = new Set<SessionState>([
  "active",
  "created",
  "guest_ready",
  "pending",
  "queued",
  "host_lost",
  "evacuating",
  "evicting",
]);
export type Lifecycle = "ACTIVE" | "IDLE — RESUMABLE" | "ARCHIVED";
export function lifecycleOf(s: SessionState): Lifecycle {
  if (ACTIVEISH.has(s)) return "ACTIVE";
  if (s === "idle") return "IDLE — RESUMABLE";
  return "ARCHIVED";
}

// Display order for the workspace list AND the rail: most-recently-active first,
// status-agnostic — a "Recent" list reads newest → oldest, the least surprising
// default. Running sessions keep bumping last_active_at as they work, so in-flight
// work naturally floats to the top without a separate status pin; idle/archived
// rows sink as their activity recedes. Array.prototype.sort is stable, so rows
// sharing a timestamp keep their source order (no jitter under the 1s poll). Both
// surfaces share this comparator, so the list reads as the full version of the rail.
type Sortable = { last_active_at: string };
export function compareSessions(a: Sortable, b: Sortable): number {
  return new Date(b.last_active_at).getTime() - new Date(a.last_active_at).getTime();
}

// The list's status filter. `live` is the working set (running + resumable);
// `archived` is terminal history (completed / failed / dead); `all` is both.
export type StatusFilter = "live" | "archived" | "all";
export function matchesFilter(s: SessionState, f: StatusFilter): boolean {
  if (f === "all") return true;
  const archived = lifecycleOf(s) === "ARCHIVED";
  return f === "archived" ? archived : !archived;
}
// The status word shown beside the glyph. Wire states are snake_case machine
// values; humanize the underscores so the column reads as language, not code.
// The glyph (components/Glyph.tsx) carries tone/shape; this carries the word —
// together they are the product's status vocabulary (glyph + text label).
export function statusLabel(s: SessionState): string {
  return s.replace(/_/g, " ");
}
