import { useState } from 'react';
import { sendPrompt } from '../api';
import type { SessionStatus } from '../types';

// PromptComposer pinned to the bottom of the transcript. The user's
// reply joins the same column rhythm as the assistant turns: a glyph
// in the left margin (▸ for "composing"), a paper-warm textarea in
// the body column, an amber send link to the right. On submit, the
// user message lands in the transcript through the existing SSE path
// (api/prompt.rs emits a User-role HarnessAgentMessage on success), so
// we don't optimistically insert.

export interface PromptComposerProps {
  sessionId: string;
  status: SessionStatus | undefined;
}

export function PromptComposer({ sessionId, status }: PromptComposerProps) {
  const [text, setText] = useState('');
  const [pending, setPending] = useState(false);
  const [error, setError] = useState<string | null>(null);

  // Dead sessions are terminal — `prompt.rs` returns 410 Gone, no
  // affordance beyond `engram session fork`. Render the dead-end
  // state instead of a textarea the user can shout into.
  if (status === 'dead') {
    return (
      <div
        className="mt-12 pt-6 font-display italic text-[0.92rem]"
        style={{
          color: 'var(--color-ink-quiet)',
          borderTop: '1px solid var(--color-rule)',
        }}
      >
        this session is dead — fork it to continue.
      </div>
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
    </div>
  );
}
