import { useState } from 'react';
import { fmtDur } from './transcriptFmt';

// A process the harness/operator ran — shell vocabulary (`$ command`),
// an exit-status dot, duration, and expandable stdout/stderr (ADR 0030
// §2b). Surfaces the `exec_started`/`exec_completed`/`stdout`/`stderr`
// events the old transcript dropped on the floor.
//
// Deliberately distinct from the bracketed tool-call asides: Claude's
// own tool calls render as `[ … ]` (ToolCall); coordinator/operator
// shell execs render as `$ …` here. They're genuinely different things.
//
//   ● $ cargo nextest run … · exit 0 · 18s ▸
//
// dot: verdigris ● (exit 0) / amber ! (non-zero) / amber ◐ (running).

export interface ProcessProps {
  command: string;
  output: string;
  /** undefined while still running. */
  completion?: { exit: number | null; durationMs?: number | null };
}

export function Process({ command, output, completion }: ProcessProps) {
  const [open, setOpen] = useState(false);
  const running = !completion;
  const failed =
    completion && completion.exit !== 0 && completion.exit != null;
  const dot = running ? '◐' : failed ? '!' : '●';
  const dotColor =
    running || failed ? 'var(--accent-now)' : 'var(--accent-archived)';
  const hasOutput = output.trim().length > 0;

  return (
    <div className="process font-mono">
      <button
        type="button"
        className="process-line"
        onClick={() => hasOutput && setOpen((o) => !o)}
        style={{ cursor: hasOutput ? 'pointer' : 'default' }}
      >
        <span
          aria-hidden
          style={{ color: dotColor, fontSize: '0.8rem' }}
        >
          {dot}
        </span>
        <span className="process-dollar">$</span>
        <span className="process-cmd">{command}</span>
        {completion && completion.exit != null && (
          <span className="process-meta" data-tabular>
            exit {completion.exit}
            {completion.durationMs != null
              ? ` · ${fmtDur(completion.durationMs)}`
              : ''}
          </span>
        )}
        {running && (
          <span
            className="process-meta"
            style={{ color: 'var(--accent-now)' }}
          >
            running…
          </span>
        )}
        {hasOutput && (
          <span className="process-toggle">{open ? '▾' : '▸'}</span>
        )}
      </button>
      {open && hasOutput && (
        <pre className="process-output">{output.trimEnd()}</pre>
      )}
    </div>
  );
}
