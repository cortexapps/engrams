import { useStorageSummary } from "../hooks/useStorageSummary";
import { fmtAgo, fmtBytes, secondsSince, shortId } from "../format";
import { PageHeading } from "../components/page-heading";
import { StatReadout } from "../components/stat-readout";
import { Text } from "@/components/ui/text";
import { Progress } from "@/components/ui/progress";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";
import type { DurabilityRow } from "../lib/types";

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
          <Text as="h2" variant="label" tone="muted">
            Durability ledger
          </Text>
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
                RPO
              </TableHead>
            </TableRow>
          </TableHeader>
          <TableBody>
            {error ? (
              <TableRow>
                <TableCell colSpan={6} className="text-sm text-destructive">
                  {(error as Error).message}
                </TableCell>
              </TableRow>
            ) : rows.length === 0 ? (
              <TableRow>
                <TableCell colSpan={6} className="text-sm text-muted-foreground">
                  {isPending ? "Loading…" : "No chunk-tracked sandboxes"}
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
  const rpoHot = secondsSince(row.last_flush_at) <= 10;
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
            <Progress value={pct} className="h-1.5 w-16" />
            <span className="font-mono text-xs tabular-nums">{pct}%</span>
          </span>
        )}
      </TableCell>
      <TableCell
        className={`text-right font-mono text-xs tabular-nums ${rpoHot ? "text-destructive" : "text-muted-foreground"}`}
      >
        {fmtAgo(row.last_flush_at)}
      </TableCell>
    </TableRow>
  );
}
