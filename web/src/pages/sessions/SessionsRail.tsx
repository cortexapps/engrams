import type { CSSProperties } from 'react';
import { Layers, ListChecks } from 'lucide-react';
import { Link, useNavigate, useRouterState } from '@tanstack/react-router';
import { useIsAdmin } from '../../auth/AuthProvider';
import { useSession, useSessions } from '../../hooks/useSessions';
import { StatusGlyph } from '../../components/Glyph';
import { NewSessionDialog } from '../../components/NewSessionDialog';
import {
  SidebarContent, SidebarFooter, SidebarGroup, SidebarGroupContent,
  SidebarGroupLabel, SidebarHeader, SidebarMenu, SidebarMenuBadge,
  SidebarMenuButton, SidebarMenuItem, SidebarMenuSkeleton,
} from '@/components/ui/sidebar';
import type { Session, SessionListItem, SessionState } from '../../types';
import { lifecycleOf, relativeTime, shortId, stripImageHost, type Lifecycle } from './session-format';

// The persistent sessions rail: a live switcher between recent sessions that
// stays mounted across the list views AND the transcript (the rail is the
// second sidebar of the whole /sessions section). Running sessions sort to the
// top; the open session is highlighted (the selected-session crumb). "See all"
// drops to the full table; admins get the fleet-wide list too. Replaces the
// old two-link rail — this is the navigation, not a menu pointing at it.

const RAIL_CAP = 10;
const ORDER: Lifecycle[] = ['ACTIVE', 'IDLE — RESUMABLE', 'ARCHIVED'];

/** One rail row. We normalise both the list shape (`last_active_at`) and the
 * single-session shape (`created_at`, used to pin an open session that isn't in
 * the recent window) to a single `at` timestamp. */
interface RailRow {
  id: string;
  status: SessionState;
  image: string;
  at: string;
}
const fromListItem = (s: SessionListItem): RailRow => ({ id: s.id, status: s.status, image: s.image, at: s.last_active_at });
const fromSession = (s: Session): RailRow => ({ id: s.id, status: s.status, image: s.image, at: s.created_at });

// Stable order so the 1s refetch never reorders rows under the cursor: by
// lifecycle bucket (active → idle → archived), then most-recently-active.
function sortForRail(rows: SessionListItem[]): SessionListItem[] {
  return [...rows].sort((a, b) => {
    const la = ORDER.indexOf(lifecycleOf(a.status));
    const lb = ORDER.indexOf(lifecycleOf(b.status));
    if (la !== lb) return la - lb;
    return new Date(b.last_active_at).getTime() - new Date(a.last_active_at).getTime();
  });
}

export function SessionsRail() {
  const isAdmin = useIsAdmin();
  const navigate = useNavigate();
  const pathname = useRouterState({ select: (s) => s.location.pathname });

  // `/sessions/<id>` → the open session; `/sessions/all` is the fleet list, not
  // a detail. `/sessions` and `/sessions/all` are the two list scopes.
  const seg = pathname.startsWith('/sessions/') ? pathname.split('/')[2] : undefined;
  const openId = seg && seg !== 'all' ? seg : undefined;
  const onMyList = pathname === '/sessions' || pathname === '/sessions/';
  const onAllList = pathname.startsWith('/sessions/all');

  const { data, isPending, error } = useSessions('mine');
  const all = data ?? [];
  const recent = sortForRail(all).slice(0, RAIL_CAP);

  // The open session always needs a row, even if it's older than the recent
  // window or (for an admin) isn't one of mine. This shares the query cache
  // with SessionDetail's own useSession, so it's not an extra fetch.
  const openSession = useSession(openId);
  const rows: RailRow[] = recent.map(fromListItem);
  if (openId && !rows.some((r) => r.id === openId)) {
    const inAll = all.find((s) => s.id === openId);
    if (inAll) rows.unshift(fromListItem(inAll));
    else if (openSession.data) rows.unshift(fromSession(openSession.data));
  }

  return (
    <>
      <SidebarHeader className="gap-3 px-3 pt-3">
        {/* "Recent" names what the list IS (my recent sessions, capped) so the
            footer's "My sessions" can mean the full list without colliding. */}
        <SidebarGroupLabel className="px-1">Recent</SidebarGroupLabel>
        {/* Lime primary — the rail's one "go" verb and the product's racecar
            action. Active rows use sidebar-accent (green), so no lime clash. */}
        <NewSessionDialog
          className="w-full"
          onCreated={(id) => navigate({ to: '/sessions/$id', params: { id } })}
        />
      </SidebarHeader>

      {/* SidebarContent is the scroll container (min-h-0 flex-1 overflow-auto
          by default), sitting between the pinned header and footer. */}
      <SidebarContent>
        <SidebarGroup className="py-1">
          <SidebarGroupContent>
            {/* StatusGlyph reads content-surface vars (--ring, --muted-foreground)
                which go near-invisible on the dark section rail. Remap them to
                the sidebar's own palette within the list: active → lime ring,
                idle/archived → a visible faded sage. The glyph stays correct
                everywhere else; only this context re-tones it. */}
            <SidebarMenu
              style={
                {
                  '--ring': 'var(--sidebar-ring)',
                  '--muted-foreground': 'color-mix(in oklch, var(--sidebar-foreground) 72%, transparent)',
                } as CSSProperties
              }
            >
              {isPending ? (
                Array.from({ length: 5 }).map((_, i) => (
                  <SidebarMenuItem key={i}><SidebarMenuSkeleton /></SidebarMenuItem>
                ))
              ) : error ? (
                <p className="px-2 py-1.5 text-xs text-destructive">Couldn’t load sessions.</p>
              ) : rows.length === 0 ? (
                <p className="px-2 py-2 text-xs text-sidebar-foreground/70">No sessions yet.</p>
              ) : (
                rows.map((r) => (
                  <SidebarMenuItem key={r.id}>
                    <SidebarMenuButton
                      asChild
                      isActive={r.id === openId}
                      className="h-auto items-start gap-2.5 py-1.5 data-[active=true]:font-medium"
                    >
                      <Link to="/sessions/$id" params={{ id: r.id }} title={r.id}>
                        <span className="mt-0.5 shrink-0 text-[0.7rem] leading-none">
                          <StatusGlyph status={r.status} />
                        </span>
                        <span className="flex min-w-0 flex-1 flex-col">
                          <span className="truncate font-mono text-[0.8rem] leading-tight">{shortId(r.id)}</span>
                          <span className="truncate text-[0.7rem] leading-tight text-sidebar-foreground/70">
                            {stripImageHost(r.image)}
                          </span>
                        </span>
                        <span className="mt-0.5 shrink-0 font-mono text-[0.65rem] tabular-nums text-sidebar-foreground/70">
                          {relativeTime(r.at)}
                        </span>
                      </Link>
                    </SidebarMenuButton>
                  </SidebarMenuItem>
                ))
              )}
            </SidebarMenu>
          </SidebarGroupContent>
        </SidebarGroup>
      </SidebarContent>

      {/* Jump to the full lists, in the app's established vocabulary: My
          sessions (mine, the full table) and All sessions (admin, fleet-wide),
          each with the icon it carries in the spine + mobile strip. The badge
          carries the true total, so the rail's 10-item cap stays honest. */}
      <SidebarFooter className="gap-1">
        <SidebarMenu>
          <SidebarMenuItem>
            <SidebarMenuButton asChild isActive={onMyList}>
              <Link to="/sessions"><Layers /><span>My sessions</span></Link>
            </SidebarMenuButton>
            {all.length > 0 && <SidebarMenuBadge>{all.length}</SidebarMenuBadge>}
          </SidebarMenuItem>
          {isAdmin && (
            <SidebarMenuItem>
              <SidebarMenuButton asChild isActive={onAllList}>
                <Link to="/sessions/all"><ListChecks /><span>All sessions</span></Link>
              </SidebarMenuButton>
            </SidebarMenuItem>
          )}
        </SidebarMenu>
      </SidebarFooter>
    </>
  );
}
