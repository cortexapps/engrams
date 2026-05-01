import { motion } from 'framer-motion';
import { useState } from 'react';

// Tool calls render as bracketed asides — never the chat-bubble shape
// that LLM UIs default to. The amber accent is reserved for the most
// recent paragraph of the transcript; tool calls always stay in faded
// ink, regardless of recency.

export interface ToolCallProps {
  toolName: string;
  argsSummary: string | null;
  /** undefined while still in flight. */
  completion?: {
    ok: boolean;
    durationMs: number;
    resultSummary: string | null;
  };
}

export function ToolCall({ toolName, argsSummary, completion }: ToolCallProps) {
  const [expanded, setExpanded] = useState(false);
  const status = completion ? (completion.ok ? 'ok' : 'err') : '…';

  return (
    <motion.div
      layout
      initial={{ opacity: 0, x: -4 }}
      animate={{ opacity: 1, x: 0 }}
      transition={{ duration: 0.35 }}
      className="my-3 ml-4 tool-rule font-mono text-[0.84rem]"
      style={{ color: 'var(--color-ink-faded)' }}
    >
      <button
        type="button"
        onClick={() => setExpanded((e) => !e)}
        className="block text-left"
        style={{ color: 'inherit' }}
      >
        <span style={{ color: 'var(--color-ink-quiet)' }}>[ </span>
        <span style={{ color: 'var(--color-ink)' }}>{toolName}</span>
        {argsSummary && (
          <>
            <span style={{ color: 'var(--color-ink-quiet)' }}> → </span>
            <span>{shortArgs(argsSummary, expanded)}</span>
          </>
        )}
        <span style={{ color: 'var(--color-ink-quiet)' }}> · </span>
        <span
          style={{
            color: completion
              ? completion.ok
                ? 'var(--color-verdigris)'
                : 'var(--color-amber)'
              : 'var(--color-amber)',
          }}
        >
          {status}
        </span>
        {completion && (
          <span
            style={{ color: 'var(--color-ink-quiet)' }}
            data-tabular
          >
            {' '}· {completion.durationMs}ms
          </span>
        )}
        <span style={{ color: 'var(--color-ink-quiet)' }}> ]</span>
      </button>

      {expanded && (
        <motion.div
          initial={{ opacity: 0, height: 0 }}
          animate={{ opacity: 1, height: 'auto' }}
          transition={{ duration: 0.25 }}
          className="mt-1.5 pl-4"
          style={{ color: 'var(--color-ink-faded)' }}
        >
          {argsSummary && (
            <Detail label="args" value={argsSummary} />
          )}
          {completion?.resultSummary && (
            <Detail label="ok " value={completion.resultSummary} />
          )}
        </motion.div>
      )}
    </motion.div>
  );
}

function Detail({ label, value }: { label: string; value: string }) {
  return (
    <div className="grid gap-x-3" style={{ gridTemplateColumns: 'min-content 1fr' }}>
      <span
        className="smallcaps"
        style={{ color: 'var(--color-ink-quiet)', fontSize: '0.7rem' }}
      >
        {label}
      </span>
      <pre
        className="whitespace-pre-wrap break-words"
        style={{ color: 'var(--color-ink)', fontSize: '0.8rem' }}
      >
        {value}
      </pre>
    </div>
  );
}

function shortArgs(args: string, expanded: boolean): string {
  if (expanded) return args;
  if (args.length <= 64) return args;
  return args.slice(0, 64) + '…';
}
