import { useState } from 'react';
import { sendPrompt } from '../api';
import type { SessionState } from '../types';

// PromptComposer pinned to the bottom of the transcript. The user's
// reply joins the same column rhythm as the assistant turns: a glyph
// in the left margin (▸ for "composing"), a paper-warm textarea in
// the body column, an amber send link to the right. On submit, the
// user message lands in the transcript through the existing SSE path
// (api/prompt.rs emits a User-role HarnessAgentMessage on success), so
// we don't optimistically insert.

export interface PromptComposerProps {
  sessionId: string;
  status: SessionState | undefined;
}

export function PromptComposer({ sessionId, status }: PromptComposerProps) {
  const [text, setText] = useState('');
  const [pending, setPending] = useState(false);
  const [error, setError] = useState<string | null>(null);

  // Terminal states — render a dead-end card instead of a textarea
  // the user can shout into. `prompt.rs` returns 410/409 for each;
  // there's no affordance beyond `engram session fork`.
  if (status === 'dead') {
    return <TerminalBanner text="this session is dead — fork it to continue." />;
  }
  if (status === 'completed') {
    return <TerminalBanner text="this session is completed — fork it to continue." />;
  }
  if (status === 'failed') {
    return <TerminalBanner text="this session failed during create — start a new one." />;
  }
  // ADR 0015 M2: HostLost is a non-terminal failure — the host went
  // away but the reconciler will resolve to Idle (if a snapshot
  // exists) or Dead soon. Either way the user can't send a prompt
  // right now. Tell them what's happening instead of failing silently.
  if (status === 'host_lost') {
    return (
      <TerminalBanner text="the host running this session went away — waiting for the reconciler to resolve to idle (resumable) or dead." />
    );
  }

  const submit = async () => {
    const trimmed = text.trim();
    if (!trimmed || pending) return;
    setPending(true);
    setError(null);
    try {
      await sendPrompt(sessionId, trimmed);
      setText('');
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setPending(false);
    }
  };

  const onKeyDown = (e: React.KeyboardEvent<HTMLTextAreaElement>) => {
    // Cmd/Ctrl+Enter submits. Plain Enter inserts newline so the
    // composer behaves like a writing surface, not a chat box.
    if ((e.metaKey || e.ctrlKey) && e.key === 'Enter') {
      e.preventDefault();
      submit();
    }
  };

  return (
    <div
      className="mt-12 pt-6 relative"
      style={{ borderTop: '1px solid var(--color-rule)' }}
    >
      <span
        className="margin-note margin-left smallcaps"
        style={{
          color: pending ? 'var(--color-amber)' : 'var(--color-ink-quiet)',
        }}
      >
        you.
      </span>

      <div className="flex items-start gap-3">
        <span
          className={`glyph ${pending ? 'glyph-heartbeat' : ''} mt-1`}
          style={{ color: pending ? 'var(--color-amber)' : 'var(--color-ink-faded)' }}
        >
          ▸
        </span>
        <textarea
          value={text}
          onChange={(e) => setText(e.target.value)}
          onKeyDown={onKeyDown}
          rows={2}
          disabled={pending}
          placeholder="type a reply — ⌘↵ to send"
          className="font-display flex-1"
          style={{
            background: 'transparent',
            border: 0,
            outline: 'none',
            color: 'var(--color-ink)',
            fontSize: '1rem',
            lineHeight: 1.55,
            resize: 'vertical',
          }}
        />
        <button
          type="button"
          onClick={submit}
          disabled={pending || !text.trim()}
          className="font-mono smallcaps text-[0.78rem] mt-2 transition-colors"
          style={{
            color:
              pending || !text.trim()
                ? 'var(--color-ink-quiet)'
                : 'var(--color-amber)',
            letterSpacing: '0.12em',
            cursor: pending || !text.trim() ? 'not-allowed' : 'pointer',
          }}
        >
          {pending ? 'sending…' : 'send →'}
        </button>
      </div>

      {error && (
        <p
          className="mt-2 ml-7 font-mono text-[0.78rem]"
          style={{ color: 'var(--color-amber)' }}
        >
          {error}
        </p>
      )}

      {status === 'idle' && (
        <p
          className="mt-2 ml-7 font-display italic text-[0.82rem]"
          style={{ color: 'var(--color-ink-quiet)' }}
        >
          session is idle — sending will resume it.
        </p>
      )}

      {(status === 'created' || status === 'guest_ready') && (
        <p
          className="mt-2 ml-7 font-display italic text-[0.82rem]"
          style={{ color: 'var(--color-ink-quiet)' }}
        >
          session is still starting up — agentd will be ready in a moment.
        </p>
      )}
    </div>
  );
}

function TerminalBanner({ text }: { text: string }) {
  return (
    <div
      className="mt-12 pt-6 font-display italic text-[0.92rem]"
      style={{
        color: 'var(--color-ink-quiet)',
        borderTop: '1px solid var(--color-rule)',
      }}
    >
      {text}
    </div>
  );
}
