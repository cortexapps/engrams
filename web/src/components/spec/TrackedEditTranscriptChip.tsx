import type { TrackedEditTranscriptChip as TrackedEditChipData } from "@engrams/spec-document";

import { Badge } from "@/components/ui/badge";

export function TrackedEditTranscriptChip({ chip }: { chip: TrackedEditChipData }) {
  return (
    <div
      role="status"
      aria-label="Tracked spec edit"
      className="grid gap-2 rounded-lg border border-border/70 bg-muted/35 px-3 py-2 text-xs"
    >
      <div className="flex items-center gap-2 text-muted-foreground">
        <Badge variant="outline">Tracked edit</Badge>
        <span>{chip.sectionId}</span>
      </div>
      <div className="grid gap-1 font-mono leading-relaxed">
        <del className="rounded bg-destructive/10 px-2 py-1 text-destructive">{chip.before}</del>
        <ins className="rounded bg-emerald-500/10 px-2 py-1 text-emerald-700 no-underline dark:text-emerald-300">
          {chip.after || "Removed"}
        </ins>
      </div>
    </div>
  );
}
