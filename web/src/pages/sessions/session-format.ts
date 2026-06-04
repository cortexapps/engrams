import type { SessionState } from '../../types';

export function shortId(id: string): string {
  return id.length <= 12 ? id : `${id.slice(0, 8)}…`;
}
export function stripImageHost(uri: string): string {
  const slash = uri.indexOf('/');
  const colon = uri.lastIndexOf(':');
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
  'active', 'created', 'guest_ready', 'pending', 'host_lost', 'evacuating', 'evicting',
]);
export type Lifecycle = 'ACTIVE' | 'IDLE — RESUMABLE' | 'ARCHIVED';
export function lifecycleOf(s: SessionState): Lifecycle {
  if (ACTIVEISH.has(s)) return 'ACTIVE';
  if (s === 'idle') return 'IDLE — RESUMABLE';
  return 'ARCHIVED';
}
const STATUS_VARIANT: Record<SessionState, 'default' | 'secondary' | 'outline' | 'destructive'> = {
  active: 'default', created: 'secondary', guest_ready: 'secondary', pending: 'secondary',
  host_lost: 'destructive', idle: 'outline', completed: 'outline', failed: 'destructive', dead: 'destructive',
  evacuating: 'secondary', evicting: 'secondary',
};
export const statusVariant = (s: SessionState) => STATUS_VARIANT[s];
