import { useParams } from '@tanstack/react-router';
import { useEffect, useState, type CSSProperties } from 'react';
import { useSession } from '../hooks/useSessions';
import { useSessionEvents } from '../hooks/useSessionEvents';
import { StatusGlyph } from '../components/Glyph';
import { SessionThread } from '../components/session-thread/SessionThread';
import { TabRow } from '../components/TabRow';
import { PageHeading } from '../components/page-heading';
import { TerminalPane } from '../components/TerminalPane';
import { SessionCowState } from '../components/CowState';
import { DurabilityTimeline } from '../components/DurabilityTimeline';
import { MetricRow } from '../components/MetricRow';
import { relativeTime } from './sessions/session-format';
import { Sidebar, SidebarContent, SidebarProvider } from '@/components/ui/sidebar';
import { Text } from '@/components/ui/text';
import type { Session } from '../types';

type ViewTab = 'transcript' | 'shell' | 'raw';

const TABS = [
  { id: 'transcript' as const, label: 'TRANSCRIPT' },
  { id: 'shell' as const, label: 'SHELL' },
  { id: 'raw' as const, label: 'RAW' },
];

export function SessionDetail() {
  const { id } = useParams({ from: '/sessions/$id' });
  const { data: session } = useSession(id);
  const events = useSessionEvents(id);
  const [tab, setTab] = useState<ViewTab>('transcript');
  // Once the user opens the SHELL tab, keep TerminalPane mounted for
  // the lifetime of this page. Switching back to TRANSCRIPT/RAW just
  // hides it via CSS — no remount, no fresh canvas, no replayed
  // ghostty-web WASM state. Lazy-mount avoids opening a ttyd
  // connection for users who never visit the SHELL tab.
  const [shellEverActive, setShellEverActive] = useState(false);
  useEffect(() => {
    if (tab === 'shell') setShellEverActive(true);
  }, [tab]);

  // App-like layout: the page fills the inset as a row — a content column
  // (masthead + tabs + active tab) beside a full-height instrument rail that
  // carries the session's metadata + durability across every tab. The rail is
  // the shadcn Sidebar primitive (same as the /sessions + /settings section
  // rails), wrapped in its own provider and re-toned off the dark spine onto
  // content tokens so the dense data stays dark-on-light legible. Shown on
  // wide screens; collapsible="none" means no toggle (and so no cmd+B clash
  // with the primary spine's provider).
  return (
    // Nested inside SessionsLayout's provider (the persistent sessions rail),
    // so we fill the section's height rather than the viewport — `min-h-0
    // flex-1` overrides the provider's built-in `min-h-svh`. `--sidebar-width`
    // here scopes the RIGHT metadata rail; the left rail uses the layout's.
    <SidebarProvider
      defaultOpen
      className="min-h-0 flex-1 overflow-hidden"
      style={{ '--sidebar-width': '18rem' } as CSSProperties}
    >
      <div className="flex min-w-0 flex-1 flex-col overflow-hidden">
        <div className="shrink-0 px-6 pt-6">
          {/* No `← back` link — the persistent sessions rail (left) keeps the
              full list in view and highlights this session, so location is
              never lost (ADR 0029). The masthead is the shared PageHeading:
              `session` eyebrow over the mono session id. */}
          <PageHeading eyebrow="session" title={id} titleVariant="mono" />

          <div className="mt-4">
            <TabRow tabs={TABS} active={tab} onChange={setTab} />
          </div>
        </div>

        <div className="min-h-0 flex-1 overflow-hidden">
          {tab === 'transcript' && (
            <SessionThread sessionId={id} events={events} status={session?.status} />
          )}

          {/* Mount TerminalPane once and keep it mounted across tab
              switches. Display:none preserves canvas + WS + ghostty-web
              WASM grid; remounting would restart bash and (because
              ghostty-web's render loop has cross-mount quirks) ghost the
              previous session's output into the fresh canvas. */}
          {shellEverActive && (
            <div
              className="h-full"
              style={{ display: tab === 'shell' ? 'block' : 'none' }}
            >
              <TerminalPane sessionId={id} />
            </div>
          )}

          {tab === 'raw' && <RawEvents events={events} />}
        </div>
      </div>

      {session && (
        <Sidebar
          side="right"
          collapsible="none"
          className="hidden border-l md:flex [--sidebar:var(--card)] [--sidebar-foreground:var(--card-foreground)] [--sidebar-border:var(--border)]"
        >
          <SidebarContent className="gap-0 px-5 py-6">
            <SessionMeta session={session} eventCount={events.length} sessionId={id} />
          </SidebarContent>
        </Sidebar>
      )}
    </SidebarProvider>
  );
}

function SessionMeta({
  session,
  eventCount,
  sessionId,
}: {
  session: Session;
  eventCount: number;
  sessionId: string;
}) {
  return (
    <div className="space-y-5">
      <div className="flex items-center gap-2">
        <StatusGlyph status={session.status} />
        <span className="text-sm font-medium text-foreground">
          {session.status.replace(/_/g, ' ')}
        </span>
      </div>

      <dl className="space-y-2.5 text-sm">
        <div>
          <dt className="text-muted-foreground">image</dt>
          <dd className="mt-0.5 font-mono text-[0.8rem] break-all text-foreground">
            {session.image}
          </dd>
        </div>
        <MetricRow label="created" value={`${relativeTime(session.created_at)} ago`} />
        <MetricRow label="events" value={eventCount} />
      </dl>

      {/* ADR 0016 Phase A + ADR 0028 A.log: per-session durability — "is my
          work durable yet" (CowState) and "what coherent points can I
          recover/fork to" (the checkpoint chain), visible across every tab. */}
      <div className="border-t pt-4">
        <Text variant="label" tone="muted" className="mb-2.5 block text-[0.65rem]">
          durability
        </Text>
        <SessionCowState sessionId={sessionId} />
        <div className="mt-3">
          <DurabilityTimeline sessionId={sessionId} />
        </div>
      </div>
    </div>
  );
}

function RawEvents({
  events,
}: {
  events: ReturnType<typeof useSessionEvents>;
}) {
  return (
    <div className="h-full space-y-0.5 overflow-auto px-6 py-4 font-mono text-[0.74rem] text-muted-foreground">
      {events.map((e) => (
        <div
          key={e.idx}
          className="grid items-baseline gap-3"
          style={{ gridTemplateColumns: '4ch min-content 1fr' }}
        >
          <span className="tabular-nums text-muted-foreground/70">{e.idx}</span>
          <span className="text-[0.66rem] uppercase tracking-[0.12em]">
            {e.event.type}
          </span>
          <span className="truncate text-foreground/80" title={JSON.stringify(e.event)}>
            {summarizeRaw(e.event)}
          </span>
        </div>
      ))}
    </div>
  );
}

function summarizeRaw(ev: unknown): string {
  // Compact one-liner of any event body — used only in the raw tab.
  const obj = ev as Record<string, unknown>;
  const fields = Object.entries(obj)
    .filter(([k]) => k !== 'type' && k !== 'at')
    .map(([k, v]) => `${k}=${JSON.stringify(v)}`)
    .join(' ');
  return fields;
}
