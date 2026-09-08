import { useEffect, useMemo, useRef, useState } from "react";
import { History } from "lucide-react";

import { CheckpointDiff } from "@/components/spec/CheckpointDiff";
import { EmptyState } from "@/components/empty-state";
import { SkeletonRows } from "@/components/skeleton-rows";
import { Button } from "@/components/ui/button";
import {
  Sheet,
  SheetContent,
  SheetDescription,
  SheetHeader,
  SheetTitle,
  SheetTrigger,
} from "@/components/ui/sheet";
import { Text } from "@/components/ui/text";
import { type SpecCheckpointSummary, useSpecCheckpoint } from "@/hooks/useSpecRead";
import { relativeTime } from "@/lib/relative-time";

export function CheckpointButton({
  specId,
  checkpoints,
}: {
  specId: string;
  checkpoints: SpecCheckpointSummary[];
}) {
  const ordered = useMemo(
    () => [...checkpoints].sort((a, b) => Date.parse(a.createdAt) - Date.parse(b.createdAt)),
    [checkpoints],
  );
  const latest = ordered.at(-1);
  const defaultIds = useMemo(() => ordered.slice(-2).map((checkpoint) => checkpoint.id), [ordered]);
  const [selectedIds, setSelectedIds] = useState<string[]>(defaultIds);
  const [open, setOpen] = useState(false);
  const defaultIdsRef = useRef(defaultIds);
  defaultIdsRef.current = defaultIds;

  useEffect(() => setSelectedIds(defaultIdsRef.current), [specId]);

  const before = useSpecCheckpoint(specId, selectedIds.at(-2) ?? null);
  const after = useSpecCheckpoint(specId, selectedIds.at(-1) ?? null);

  const selectCheckpoint = (checkpointId: string) => {
    setSelectedIds((current) => {
      if (current.includes(checkpointId)) return current.filter((id) => id !== checkpointId);
      return [...current, checkpointId].slice(-2);
    });
  };

  return (
    <Sheet
      open={open}
      onOpenChange={(nextOpen) => {
        setOpen(nextOpen);
        if (nextOpen) setSelectedIds(defaultIdsRef.current);
      }}
    >
      <SheetTrigger asChild>
        <Button variant="outline" size="sm" className="spec-mode-checkpoint-trigger">
          <History aria-hidden="true" />
          {latest ? `History · ${relativeTime(latest.createdAt)}` : "History"}
        </Button>
      </SheetTrigger>
      <SheetContent className="spec-mode-checkpoint-sheet sm:max-w-2xl">
        <SheetHeader className="border-b">
          <SheetTitle>Checkpoint history</SheetTitle>
          {/* Instructing someone to select two versions directly above "No
              versions yet" reads as a broken panel. Only ask once there is
              something to ask for. */}
          <SheetDescription>
            {ordered.length === 0
              ? "Versions appear here as the spec progresses."
              : "Select two saved versions to compare."}
          </SheetDescription>
        </SheetHeader>
        <div className="spec-mode-checkpoint-body">
          <ol className="spec-mode-checkpoint-list" aria-label="Checkpoints">
            {ordered.map((checkpoint) => {
              const selectedIndex = selectedIds.indexOf(checkpoint.id);
              return (
                <li key={checkpoint.id}>
                  <button
                    type="button"
                    aria-pressed={selectedIndex >= 0}
                    data-selected={selectedIndex >= 0 || undefined}
                    onClick={() => selectCheckpoint(checkpoint.id)}
                  >
                    <span className="spec-mode-checkpoint-order">
                      {selectedIndex >= 0 ? selectedIndex + 1 : ""}
                    </span>
                    <span>
                      <strong>{checkpoint.label}</strong>
                      <small>
                        {checkpoint.author?.name ?? "System"} · {relativeTime(checkpoint.createdAt)}
                      </small>
                    </span>
                  </button>
                </li>
              );
            })}
          </ol>
          {ordered.length === 0 ? (
            <EmptyState inline>
              No versions yet. A version is saved each time a section settles, and at publish.
            </EmptyState>
          ) : selectedIds.length < 2 ? (
            <Text tone="muted">Select one more checkpoint to compare.</Text>
          ) : before.isPending || after.isPending ? (
            <SkeletonRows rows={3} />
          ) : before.data && after.data ? (
            <CheckpointDiff before={before.data.markdown} after={after.data.markdown} />
          ) : (
            <EmptyState inline>The comparison is not available.</EmptyState>
          )}
        </div>
      </SheetContent>
    </Sheet>
  );
}
