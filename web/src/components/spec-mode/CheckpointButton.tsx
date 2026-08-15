import { useEffect, useMemo, useRef, useState } from "react";
import { History } from "lucide-react";

import { CheckpointDiff } from "@/components/spec/CheckpointDiff";
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
          <SheetDescription>Select two saved versions to compare.</SheetDescription>
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
            <Text tone="muted">
              No versions yet. A version is saved each time a section settles, and at publish.
            </Text>
          ) : selectedIds.length < 2 ? (
            <Text tone="muted">Select one more checkpoint to compare.</Text>
          ) : before.isPending || after.isPending ? (
            <Text tone="muted">Loading comparison…</Text>
          ) : before.data && after.data ? (
            <CheckpointDiff before={before.data.markdown} after={after.data.markdown} />
          ) : (
            <Text tone="muted">The comparison is not available.</Text>
          )}
        </div>
      </SheetContent>
    </Sheet>
  );
}

function relativeTime(value: string): string {
  const elapsedMs = Date.now() - Date.parse(value);
  if (!Number.isFinite(elapsedMs)) return value;
  if (elapsedMs < 60_000) return "now";
  const minutes = Math.floor(elapsedMs / 60_000);
  if (minutes < 60) return `${minutes}m ago`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return `${hours}h ago`;
  return `${Math.floor(hours / 24)}d ago`;
}
