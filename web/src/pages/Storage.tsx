import { toast } from "sonner";

import { useStorageSummary } from "../hooks/useStorageSummary";
import { useChunkGcDryRun } from "../hooks/useChunkGc";
import { fmtBytes } from "../format";
import { PageHeading } from "../components/page-heading";
import { EmptyState } from "@/components/empty-state";
import { ReadoutCell, ReadoutStrip } from "@/components/readout-strip";
import { Button } from "@/components/ui/button";
import { errorMessage } from "../lib/errors";

// Settings › Storage: what the snapshot store holds and what the chunk GC has
// queued. The dry run says how many chunks a sweep would unpin without
// deleting any.

const COMPACT = new Intl.NumberFormat("en", { notation: "compact", maximumFractionDigits: 1 });

/** An exact backlog reads as its digits; an estimate reads as "≈22.7M". */
export function formatBacklog(count: number, exact: boolean): string {
  return exact ? String(count) : `≈${COMPACT.format(count)}`;
}

export function Storage() {
  const { data, error } = useStorageSummary();
  const gc = useChunkGcDryRun();

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

      {error ? (
        <EmptyState tone="error">Couldn’t load storage. {errorMessage(error)}</EmptyState>
      ) : (
        <ReadoutStrip columns="repeat(2, minmax(0, 1fr))">
          <ReadoutCell
            label="Snapshots"
            value={data?.snapshots ?? "—"}
            sub={data ? fmtBytes(data.snapshot_bytes) : undefined}
          />
          <ReadoutCell
            label="GC pending"
            value={data ? formatBacklog(data.gc_pending, data.gc_pending_exact) : "—"}
            sub={data && !data.gc_pending_exact ? "candidates · estimate" : "candidates"}
          />
        </ReadoutStrip>
      )}
    </div>
  );
}
