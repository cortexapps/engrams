import { useAuiState } from '@assistant-ui/react';
import { CornerDownRightIcon } from 'lucide-react';
import { hms } from '../transcriptFmt';
import type { RunFooter as RunFooterData } from './buildMessages';

// The per-run receipt that closes an assistant turn (`↳ read 3 · edited 1 ·
// ran 2`), read from the assistant message's metadata.custom.run. Rendered in
// the assistant message footer so it stays attached to its run. Returns null
// for the trailing in-flight message (no footer until the run closes).

export function RunFooter() {
  const run = useAuiState(
    (s) => s.message.metadata.custom?.run as RunFooterData | undefined,
  );
  if (!run) return null;

  const parts: string[] = [];
  if (run.reads) parts.push(`read ${run.reads}`);
  if (run.edits) parts.push(`edited ${run.edits}`);
  if (run.ran) parts.push(`ran ${run.ran}`);
  if (run.other) parts.push(`${run.other} other`);

  return (
    <div className="flex items-center gap-1.5 py-0.5 font-mono text-xs text-muted-foreground">
      <CornerDownRightIcon className="size-3 shrink-0" />
      {run.interrupted ? (
        <span className="text-destructive">interrupted</span>
      ) : !run.ok ? (
        <span className="text-destructive">failed</span>
      ) : null}
      {parts.length > 0 && <span className="tabular-nums">{parts.join(' · ')}</span>}
      <span aria-hidden>·</span>
      <span className="tabular-nums">{hms(run.endAt)}</span>
    </div>
  );
}
