import { useStorageSummary } from "../hooks/useStorageSummary";
import { fmtBytes, secondsSince, shortId } from "../format";
import { PageHeading } from "../components/page-heading";
import { StatReadout } from "../components/stat-readout";
import { EmptyState } from "@/components/empty-state";
import { localityTone, Meter } from "@/components/meter";
import { SkeletonRows } from "@/components/skeleton-rows";
import { StatusDot } from "@/components/status-dot";
import { relativeTime } from "@/lib/relative-time";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";
import type { DurabilityRow } from "../lib/types";

/** The flush window: a sandbox whose last flush is older than this has dirty
 * chunks at risk, so its last-flush reading turns caution. Recency is not the
 * signal — a flush ten seconds ago is good news, not an alarm. */
const FLUSH_WINDOW_SECS = 60;

const LEDGER_COLUMNS = ["minmax(0, 1.6fr)", "120px", "90px", "110px", "190px", "100px"];

export function Storage() {
  const { data, isPending, error } = useStorageSummary();
  const rows = data?.rows ?? [];
  const rollups = [
    { label: "Snapshots", value: data?.snapshots ?? 0 },
    { label: "Snapshot bytes", value: fmtBytes(data?.snapshot_bytes ?? 0) },
    { label: "Tracked sandboxes", value: data?.tracked_sandboxes ?? 0 },
    { label: "Unflushed", value: fmtBytes(data?.unflushed_bytes ?? 0) },
    { label: "Avg locality", value: `${data?.avg_locality_pct ?? 0}%` },
    { label: "GC pending", value: data?.gc_pending ?? 0 },
  ];

  return (
    <div className="space-y-6">
      <PageHeading title="Storage" />

      <StatReadout className="sm:grid-cols-3 lg:grid-cols-6" items={rollups} />

      <div>
        <div className="mb-2 flex items-baseline justify-between">
          <h2 className="text-sm font-semibold">Durability ledger</h2>
          <span className="font-mono text-xs tabular-nums text-muted-foreground">
            {rows.length}
          </span>
        </div>
        {/* The three domain terms below used to be defined in a paragraph under
            the table. A definition belongs where the question is asked, so each
            one now rides its own column header. */}
        <Table>
          <TableHeader>
            <TableRow>
              <TableHead>Session</TableHead>
              <TableHead>Host</TableHead>
              <TableHead
                className="text-right"
                title="Chunks written since the last flush to the content-addressed store."
              >
                Dirty
              </TableHead>
              <TableHead className="text-right" title="Bytes not yet flushed to the chunk store.">
                Unflushed
              </TableHead>
              <TableHead title="Share of this sandbox's base chunks resident on its own host.">
                Base locality
              </TableHead>
              <TableHead className="text-right" title="Time since the last flush.">
                Last flush
              </TableHead>
            </TableRow>
          </TableHeader>
          <TableBody>
            {error ? (
              <TableRow>
                <TableCell colSpan={6}>
                  <EmptyState inline tone="error">
                    Couldn’t load the ledger. {(error as Error).message}
                  </EmptyState>
                </TableCell>
              </TableRow>
            ) : isPending ? (
              <TableRow>
                <TableCell colSpan={6}>
                  <SkeletonRows rows={4} columns={LEDGER_COLUMNS} />
                </TableCell>
              </TableRow>
            ) : rows.length === 0 ? (
              <TableRow>
                <TableCell colSpan={6}>
                  <EmptyState inline>No chunk-tracked sandboxes.</EmptyState>
                </TableCell>
              </TableRow>
            ) : (
              rows.map((r) => <LedgerRow key={r.sandbox_id} row={r} />)
            )}
          </TableBody>
        </Table>
      </div>
    </div>
  );
}

function LedgerRow({ row }: { row: DurabilityRow }) {
  const pct =
    row.base_chunks > 0 ? Math.round((row.base_chunks_local / row.base_chunks) * 100) : null;
  // Staleness, not recency: inside the window is nominal, past it is caution,
  // and a sandbox that has never flushed has nothing to judge yet.
  const flushTone =
    row.last_flush_at === null
      ? "muted"
      : secondsSince(row.last_flush_at) <= FLUSH_WINDOW_SECS
        ? "nominal"
        : "caution";
  return (
    <TableRow>
      <TableCell className="font-mono text-sm">
        {shortId(row.session_id ?? row.sandbox_id)}
      </TableCell>
      <TableCell className="font-mono text-xs text-muted-foreground">
        {shortId(row.host_id)}
      </TableCell>
      <TableCell className="text-right font-mono tabular-nums">{row.dirty_chunks}</TableCell>
      <TableCell className="text-right font-mono tabular-nums">
        {fmtBytes(row.dirty_bytes)}
      </TableCell>
      <TableCell>
        {pct === null ? (
          "—"
        ) : (
          <span className="flex items-center gap-2">
            <Meter value={pct} tone={localityTone(pct)} label="base locality" className="w-16" />
            <span className="font-mono text-xs tabular-nums">{pct}%</span>
          </span>
        )}
      </TableCell>
      <TableCell className="text-right">
        <span className="inline-flex items-center gap-1.5 font-mono text-xs tabular-nums text-muted-foreground">
          <StatusDot tone={flushTone} size={6} />
          {relativeTime(row.last_flush_at)}
        </span>
      </TableCell>
    </TableRow>
  );
}
