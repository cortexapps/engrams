import { useState } from "react";
import { toast } from "sonner";

import { useStorageSummary } from "../hooks/useStorageSummary";
import { useChunkGcDryRun } from "../hooks/useChunkGc";
import { useTasksAsSessionList } from "../hooks/useTasks";
import { fmtBytes, secondsSince, shortId } from "../format";
import { FLUSH_WINDOW_S } from "../operator-health";
import { PageHeading } from "../components/page-heading";
import { StatusGlyph } from "../components/Glyph";
import { EmptyState } from "@/components/empty-state";
import { localityTone, Meter } from "@/components/meter";
import { ReadoutCell, ReadoutStrip } from "@/components/readout-strip";
import { SkeletonRows } from "@/components/skeleton-rows";
import { StatusDot, type StatusTone } from "@/components/status-dot";
import { Button } from "@/components/ui/button";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";
import { errorMessage } from "../lib/errors";
import { relativeAge } from "@/lib/relative-time";
import { cn } from "@/lib/utils";
import type { DurabilityRow, SessionListItem } from "../lib/types";

// Settings › Storage: the durability ledger. The rollup says how much is at
// risk right now (unflushed bytes, sandboxes past the flush window); the ledger
// names the sandboxes that carry it. A row is judged by STALENESS — a flush
// four seconds ago is good news — and a sandbox with nothing unflushed has
// nothing to say, so those rows fold into one trailing line.

const LEDGER_COLUMNS = ["minmax(0, 1.6fr)", "120px", "90px", "110px", "190px", "100px"];

/** Staleness, not recency: inside the window is nominal, past it is caution,
 * and a sandbox that has never flushed has nothing to judge yet. */
export function flushTone(lastFlushAt: string | null): StatusTone {
  if (lastFlushAt === null) return "muted";
  return secondsSince(lastFlushAt) <= FLUSH_WINDOW_S ? "nominal" : "caution";
}

/** Rows that carry unflushed work, most at risk first; the rest fold away. */
export function splitLedger(rows: DurabilityRow[]): {
  active: DurabilityRow[];
  quiet: DurabilityRow[];
} {
  const active = rows
    .filter((r) => r.dirty_chunks > 0 || r.dirty_bytes > 0)
    .sort((a, b) => b.dirty_bytes - a.dirty_bytes || b.dirty_chunks - a.dirty_chunks);
  const quiet = rows.filter((r) => !(r.dirty_chunks > 0 || r.dirty_bytes > 0));
  return { active, quiet };
}

export function Storage() {
  const { data, isPending, error } = useStorageSummary();
  // The ledger row carries a session id, not a name. Best effort: name it from
  // the fleet-wide task list when the task is on the first page; fall back to
  // the id. Never a count from this list — those come from storage truth.
  const { data: tasks } = useTasksAsSessionList({ scope: "all", pageSize: 100 });
  const gc = useChunkGcDryRun();
  const [showQuiet, setShowQuiet] = useState(false);

  const rows = data?.rows ?? [];
  const { active, quiet } = splitLedger(rows);
  const collapsible = active.length > 0 && quiet.length > 0;
  const visible = collapsible && !showQuiet ? active : [...active, ...quiet];
  const bySession = new Map((tasks ?? []).map((t) => [t.id, t]));
  const unflushedSandboxes = rows.filter((r) => r.dirty_bytes > 0).length;
  const pastWindow = rows.filter((r) => secondsSince(r.last_flush_at) > FLUSH_WINDOW_S).length;
  const tracked = data?.tracked_sandboxes ?? 0;

  const onDryRun = async () => {
    try {
      const r = await gc.mutateAsync({ dryRun: true });
      if (r.markError) {
        toast.error(`GC dry run could not mark: ${r.markError}`);
        return;
      }
      toast.success(
        `GC dry run: ${r.candidatesMarked} of ${r.listedChunks} chunks unpinned · grace ${r.graceSecs}s`,
      );
    } catch (e) {
      toast.error(errorMessage(e));
    }
  };

  return (
    <div className="space-y-6">
      <PageHeading
        title="Storage"
        count={data ? `${tracked} tracked` : undefined}
        actions={
          <Button
            variant="outline"
            size="sm"
            onClick={() => void onDryRun()}
            disabled={gc.isPending}
          >
            GC dry run
          </Button>
        }
      />

      <ReadoutStrip columns="repeat(5, minmax(0, 1fr))">
        <ReadoutCell
          label="Snapshots"
          value={data?.snapshots ?? "—"}
          sub={data ? fmtBytes(data.snapshot_bytes) : undefined}
        />
        <ReadoutCell
          label="Unflushed"
          value={data ? fmtBytes(data.unflushed_bytes) : "—"}
          sub={
            data
              ? `across ${unflushedSandboxes} sandbox${unflushedSandboxes === 1 ? "" : "es"}`
              : undefined
          }
        />
        <ReadoutCell
          label="Base locality"
          value={data && tracked > 0 ? `${data.avg_locality_pct}%` : "—"}
          sub={data ? `${tracked} tracked` : undefined}
        >
          <Meter
            value={data && tracked > 0 ? data.avg_locality_pct : null}
            tone={data && tracked > 0 ? localityTone(data.avg_locality_pct) : undefined}
            label="base locality"
          />
        </ReadoutCell>
        <ReadoutCell
          label="Past flush window"
          value={data ? pastWindow : "—"}
          sub={`window ${FLUSH_WINDOW_S}s`}
        />
        <ReadoutCell label="GC pending" value={data?.gc_pending ?? "—"} sub="candidates" />
      </ReadoutStrip>

      <div className="rounded-lg border bg-card">
        <Table>
          <TableHeader>
            <TableRow className="hover:bg-transparent">
              <TableHead>Task</TableHead>
              <TableHead className="w-[120px]">Host</TableHead>
              <TableHead
                className="w-[90px] text-right"
                title="Chunks written since the last flush to the content-addressed store."
              >
                Dirty
              </TableHead>
              <TableHead
                className="w-[110px] text-right"
                title="Bytes not yet flushed to the chunk store."
              >
                Unflushed
              </TableHead>
              <TableHead
                className="w-[190px]"
                title="Share of this sandbox's base chunks resident on its own host."
              >
                Base locality
              </TableHead>
              <TableHead
                className="w-[100px] text-right"
                title={`Time since the last flush; caution past the ${FLUSH_WINDOW_S}s window.`}
              >
                Last flush
              </TableHead>
            </TableRow>
          </TableHeader>
          <TableBody>
            {error ? (
              <TableRow className="hover:bg-transparent">
                <TableCell colSpan={6}>
                  <EmptyState inline tone="error">
                    Couldn’t load the ledger. {errorMessage(error)}
                  </EmptyState>
                </TableCell>
              </TableRow>
            ) : isPending ? (
              <TableRow className="hover:bg-transparent">
                <TableCell colSpan={6}>
                  <SkeletonRows rows={4} columns={LEDGER_COLUMNS} />
                </TableCell>
              </TableRow>
            ) : rows.length === 0 ? (
              <TableRow className="hover:bg-transparent">
                <TableCell colSpan={6}>
                  <EmptyState inline>No chunk-tracked sandboxes.</EmptyState>
                </TableCell>
              </TableRow>
            ) : (
              <>
                {visible.map((r) => (
                  <LedgerRow
                    key={r.sandbox_id}
                    row={r}
                    task={r.session_id ? bySession.get(r.session_id) : undefined}
                  />
                ))}
                {collapsible && (
                  <TableRow className="hover:bg-transparent">
                    <TableCell colSpan={6} className="py-2">
                      <button
                        type="button"
                        onClick={() => setShowQuiet((v) => !v)}
                        aria-expanded={showQuiet}
                        className="text-xs text-muted-foreground underline-offset-4 hover:text-foreground hover:underline"
                      >
                        {showQuiet
                          ? "Hide the sandboxes with nothing unflushed"
                          : `${quiet.length} more · nothing unflushed`}
                      </button>
                    </TableCell>
                  </TableRow>
                )}
              </>
            )}
          </TableBody>
        </Table>
      </div>
    </div>
  );
}

function LedgerRow({ row, task }: { row: DurabilityRow; task: SessionListItem | undefined }) {
  const pct =
    row.base_chunks > 0 ? Math.round((row.base_chunks_local / row.base_chunks) * 100) : null;
  const tone = flushTone(row.last_flush_at);
  const id = shortId(row.session_id ?? row.sandbox_id);
  return (
    <TableRow className="hover:bg-transparent">
      <TableCell className="align-top">
        <div className="flex items-start gap-2">
          {task && (
            <span className="mt-0.5 inline-flex w-3 shrink-0 justify-center text-2xs leading-none">
              <StatusGlyph status={task.status} beat={false} />
            </span>
          )}
          <div className="min-w-0">
            {task?.title ? (
              <>
                <div className="truncate text-sm font-medium">{task.title}</div>
                <div className="font-mono text-2xs text-muted-foreground">{id}</div>
              </>
            ) : (
              <div className="font-mono text-sm">{id}</div>
            )}
          </div>
        </div>
      </TableCell>
      <TableCell className="align-top font-mono text-xs text-muted-foreground">
        {shortId(row.host_id)}
      </TableCell>
      <TableCell className="align-top text-right font-mono text-xs tabular-nums">
        {row.dirty_chunks}
      </TableCell>
      <TableCell className="align-top text-right font-mono text-xs tabular-nums">
        {fmtBytes(row.dirty_bytes)}
      </TableCell>
      <TableCell className="align-top">
        {pct === null ? (
          <span className="text-muted-foreground">—</span>
        ) : (
          <span className="flex items-center gap-2">
            <Meter
              value={pct}
              tone={localityTone(pct)}
              label="base locality"
              className="max-w-[90px]"
            />
            <span className="font-mono text-xs tabular-nums">{pct}%</span>
          </span>
        )}
      </TableCell>
      <TableCell className="align-top text-right">
        <span
          className={cn(
            "inline-flex items-center gap-1.5 font-mono text-xs tabular-nums",
            tone === "muted" ? "text-muted-foreground" : "text-foreground",
          )}
        >
          <StatusDot tone={tone} size={6} />
          {row.last_flush_at === null ? "never" : relativeAge(row.last_flush_at)}
        </span>
      </TableCell>
    </TableRow>
  );
}
