import type { SessionState } from "../../types";

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

// Display order for the workspace list AND the rail: running first, then
// resumable, then archived; ties broken by most-recently-active. Both surfaces
// share this so the list reads as the full version of the rail.
const LIFECYCLE_ORDER: Lifecycle[] = ["ACTIVE", "IDLE — RESUMABLE", "ARCHIVED"];
type Sortable = { status: SessionState; last_active_at: string };
export function compareSessions(a: Sortable, b: Sortable): number {
  const la = LIFECYCLE_ORDER.indexOf(lifecycleOf(a.status));
  const lb = LIFECYCLE_ORDER.indexOf(lifecycleOf(b.status));
  if (la !== lb) return la - lb;
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
