import { Link, useParams } from 'react-router-dom';
import { useEffect, useState } from 'react';
import { useSession } from '../hooks/useSessions';
import { useSessionEvents } from '../hooks/useSessionEvents';
import { StatusGlyph } from '../components/Glyph';
import { Transcript } from '../components/Transcript';
import { PromptComposer } from '../components/PromptComposer';
import { TabRow } from '../components/TabRow';
import { TerminalPane } from '../components/TerminalPane';
import { relativeTime } from '../components/SessionManifest';
import type { WorkspaceSpec } from '../types';

function workspaceLabel(ws: WorkspaceSpec): string {
  switch (ws.kind) {
    case 'empty':
      return 'empty';
    case 'git':
      return ws.url;
    case 'local_mount':
      return `${ws.host_path} → ${ws.guest_path}`;
  }
}

type ViewTab = 'transcript' | 'shell' | 'raw';

const TABS = [
  { id: 'transcript' as const, label: 'TRANSCRIPT' },
  { id: 'shell' as const, label: 'SHELL' },
  { id: 'raw' as const, label: 'RAW' },
];

export function SessionDetail() {
  const { id } = useParams<{ id: string }>();
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

  return (
    <main className="book py-12">
      <Link
        to="/"
        className="font-mono smallcaps text-[0.7rem]"
        style={{ color: 'var(--color-ink-quiet)', letterSpacing: '0.18em' }}
      >
        ← back to overview
      </Link>

      <header className="mt-8 mb-12">
        <div
          className="font-mono smallcaps text-[0.7rem]"
          style={{ color: 'var(--color-ink-quiet)' }}
        >
          session
        </div>
        <h1
          className="font-mono"
          style={{
            fontSize: '1.4rem',
            color: 'var(--color-ink)',
            letterSpacing: '-0.01em',
          }}
        >
          {id}
        </h1>

        {session && (
          <div
            className="mt-3 flex items-baseline gap-3 font-display"
            style={{ color: 'var(--color-ink-faded)' }}
          >
            <StatusGlyph status={session.status} />
            <span
              className="smallcaps"
              style={{ fontSize: '0.7rem', color: 'var(--color-ink-quiet)' }}
            >
              {session.status}
            </span>
            <span style={{ color: 'var(--color-ink-faded)' }}>·</span>
            <span className="font-mono text-[0.85rem]">
              {workspaceLabel(session.workspace)}
            </span>
            {session.workspace.kind === 'git' && (
              <>
                <span style={{ color: 'var(--color-ink-quiet)' }}>·</span>
                <span className="font-mono text-[0.85rem]">
                  {session.workspace.branch}
                </span>
              </>
            )}
          </div>
        )}

        {session && (
          <div
            className="mt-1 font-mono text-[0.78rem]"
            style={{ color: 'var(--color-ink-quiet)' }}
          >
            image {session.image.repo}:{session.image.tag} · created{' '}
            {relativeTime(session.created_at)} ago · {events.length} events
          </div>
        )}

        <hr className="mt-6" />
      </header>

      <TabRow
        tabs={TABS}
        active={tab}
        onChange={setTab}
        right={
          tab === 'transcript' ? `${events.length} events` : undefined
        }
      />

      {tab === 'transcript' && (
        <>
          <Transcript events={events} />
          {id && <PromptComposer sessionId={id} status={session?.status} />}
        </>
      )}

      {/* Mount TerminalPane once and keep it mounted across tab
          switches. Display:none preserves canvas + WS + ghostty-web
          WASM grid; remounting would restart bash and (because
          ghostty-web's render loop has cross-mount quirks) ghost the
          previous session's output into the fresh canvas. */}
      {shellEverActive && id && (
        <div style={{ display: tab === 'shell' ? 'block' : 'none' }}>
          <TerminalPane sessionId={id} />
        </div>
      )}

      {tab === 'raw' && (
        <div
          className="font-mono text-[0.74rem] space-y-0.5"
          style={{ color: 'var(--color-ink-faded)' }}
        >
          {events.map((e) => (
            <div
              key={e.idx}
              className="grid items-baseline gap-3"
              style={{ gridTemplateColumns: '4ch min-content 1fr' }}
            >
              <span data-tabular style={{ color: 'var(--color-ink-quiet)' }}>
                {e.idx}
              </span>
              <span className="smallcaps" style={{ fontSize: '0.66rem' }}>
                {e.event.type}
              </span>
              <span
                className="truncate"
                title={JSON.stringify(e.event)}
              >
                {summarizeRaw(e.event)}
              </span>
            </div>
          ))}
        </div>
      )}
    </main>
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
