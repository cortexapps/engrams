import { Link, useParams } from 'react-router-dom';
import { useState } from 'react';
import { useSession } from '../hooks/useSessions';
import { useSessionEvents } from '../hooks/useSessionEvents';
import { StatusGlyph } from '../components/Glyph';
import { Transcript } from '../components/Transcript';
import { relativeTime } from '../components/SessionManifest';

export function SessionDetail() {
  const { id } = useParams<{ id: string }>();
  const { data: session } = useSession(id);
  const events = useSessionEvents(id);
  const [showRaw, setShowRaw] = useState(false);

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
        <div className="font-mono smallcaps text-[0.7rem]" style={{ color: 'var(--color-ink-quiet)' }}>
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
          <div className="mt-3 flex items-baseline gap-3 font-display" style={{ color: 'var(--color-ink-faded)' }}>
            <StatusGlyph status={session.status} />
            <span className="smallcaps" style={{ fontSize: '0.7rem', color: 'var(--color-ink-quiet)' }}>
              {session.status}
            </span>
            <span style={{ color: 'var(--color-ink-faded)' }}>·</span>
            <span className="font-mono text-[0.85rem]">{session.repo}</span>
            <span style={{ color: 'var(--color-ink-quiet)' }}>·</span>
            <span className="font-mono text-[0.85rem]">{session.branch}</span>
          </div>
        )}

        {session && (
          <div className="mt-1 font-mono text-[0.78rem]" style={{ color: 'var(--color-ink-quiet)' }}>
            image {session.image_version} · created {relativeTime(session.created_at)} ago · {events.length} events
          </div>
        )}

        <hr className="mt-6" />
      </header>

      <Transcript events={events} />

      <div className="mt-16">
        <button
          type="button"
          onClick={() => setShowRaw((v) => !v)}
          className="font-mono smallcaps text-[0.7rem]"
          style={{ color: 'var(--color-ink-quiet)', letterSpacing: '0.18em' }}
        >
          {showRaw ? '▾' : '▸'} raw event log
        </button>
        {showRaw && (
          <div className="mt-3 font-mono text-[0.74rem] space-y-0.5" style={{ color: 'var(--color-ink-faded)' }}>
            {events.map((e) => (
              <div key={e.idx} className="grid items-baseline gap-3" style={{ gridTemplateColumns: '4ch min-content 1fr' }}>
                <span data-tabular style={{ color: 'var(--color-ink-quiet)' }}>{e.idx}</span>
                <span className="smallcaps" style={{ fontSize: '0.66rem' }}>{e.event.type}</span>
                <span className="truncate" title={JSON.stringify(e.event)}>{summarizeRaw(e.event)}</span>
              </div>
            ))}
          </div>
        )}
      </div>
    </main>
  );
}

function summarizeRaw(ev: unknown): string {
  // Compact one-liner of any event body — used only in the raw sidebar.
  const obj = ev as Record<string, unknown>;
  const fields = Object.entries(obj)
    .filter(([k]) => k !== 'type' && k !== 'at')
    .map(([k, v]) => `${k}=${JSON.stringify(v)}`)
    .join(' ');
  return fields;
}
