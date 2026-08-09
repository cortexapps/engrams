import { Undo2Icon } from "lucide-react";
import type {
  RestoreSectionStateUndo,
  SectionState,
  SectionStateTranscriptChip as SectionStateChipData,
  SectionStateValue,
} from "@engrams/spec-document";

import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Text } from "@/components/ui/text";

export type SpecSectionState = SectionState;
export type SpecSectionStateValue = SectionStateValue;
export type SpecSectionStateUndo = RestoreSectionStateUndo;
export type SpecSectionStateChipData = SectionStateChipData;

const STATE_LABELS: Readonly<Record<SpecSectionState, string>> = {
  empty: "empty",
  drafted: "drafted",
  confirmed: "confirmed",
  "n/a": "not applicable",
};

export function SectionStateTranscriptChip({
  chip,
  onUndo,
  undoPending = false,
}: {
  chip: SpecSectionStateChipData;
  onUndo: (undo: SpecSectionStateUndo) => void;
  undoPending?: boolean;
}) {
  return (
    <div
      role="status"
      aria-label={`${chip.sectionTitle} state changed`}
      className="flex flex-wrap items-center gap-2 rounded-lg border border-border/70 bg-muted/35 px-3 py-2 text-xs text-muted-foreground"
    >
      <Text as="span" variant="label" className="text-foreground">
        {chip.sectionTitle}
      </Text>
      <span aria-hidden>·</span>
      <span>
        {STATE_LABELS[chip.before.state]} → {STATE_LABELS[chip.after.state]}
      </span>
      {chip.provisional && (
        <Badge variant="outline" className="border-amber-500/40 bg-amber-500/10 text-amber-700">
          provisional
        </Badge>
      )}
      {chip.after.state === "n/a" && chip.after.naReason != null && (
        <span className="basis-full pl-0.5">{chip.after.naReason}</span>
      )}
      <Button
        type="button"
        variant="ghost"
        size="xs"
        className="ml-auto"
        disabled={undoPending}
        onClick={() => onUndo(chip.undo)}
      >
        <Undo2Icon />
        {undoPending ? "Undoing…" : "Undo"}
      </Button>
    </div>
  );
}
