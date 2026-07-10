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
// Start-screen previews can use the same most-recently-active-first ordering as
// task-list responses when combining cached rows.
type Sortable = { last_active_at: string };
export function compareSessions(a: Sortable, b: Sortable): number {
  return new Date(b.last_active_at).getTime() - new Date(a.last_active_at).getTime();
}
// The status word shown beside the glyph. Wire states are snake_case machine
// values; humanize the underscores so the column reads as language, not code.
// The glyph (components/Glyph.tsx) carries tone/shape; this carries the word —
// together they are the product's status vocabulary (glyph + text label).
export function statusLabel(s: SessionState): string {
  return s.replace(/_/g, " ");
}
