import { fmtDur } from './transcriptFmt';

// A run's "receipt" — a faint one-line tally of what the agent did this
// run (read N · edited N · ran N · duration), rendered at the run's
// close (ADR 0030 §2d). Lets operators scan long sessions without
// reading every block. Shows `interrupted` if the run was stopped.

export interface RunTally {
  reads: number;
  edits: number;
  ran: number;
  other: number;
  /** run_started.at — start of the run, for the duration. */
  at?: string;
}

export function RunSummary({
  summary,
  endAt,
  ok,
  interrupted,
}: {
  summary: RunTally;
  endAt?: string;
  ok: boolean;
  interrupted?: boolean;
}) {
  const parts: string[] = [];
  if (summary.reads) parts.push(`read ${summary.reads}`);
  if (summary.edits) parts.push(`edited ${summary.edits}`);
  if (summary.ran) parts.push(`ran ${summary.ran}`);
  if (summary.other)
    parts.push(`${summary.other} ${summary.other === 1 ? 'tool' : 'tools'}`);
  if (summary.at && endAt) {
    const ms = new Date(endAt).getTime() - new Date(summary.at).getTime();
    if (ms > 0) parts.push(fmtDur(ms));
  }

  // Nothing happened and the run completed cleanly → no receipt worth
  // showing. An interrupted/failed run always gets a receipt.
  if (parts.length === 0 && ok && !interrupted) return null;

  return (
    <div className="run-summary">
      <span className="run-summary-mark" aria-hidden>
        ↳
      </span>
      <span className="run-summary-text font-mono">
        {interrupted
          ? `interrupted${parts.length ? ` · ${parts.join(' · ')}` : ''}`
          : ok
            ? parts.join(' · ')
            : `failed${parts.length ? ` · ${parts.join(' · ')}` : ''}`}
      </span>
    </div>
  );
}
