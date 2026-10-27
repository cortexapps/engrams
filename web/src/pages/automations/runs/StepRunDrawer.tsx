import { Sheet, SheetContent, SheetDescription, SheetTitle } from "@/components/ui/sheet";

import { StepDetails } from "./StepDetails";
import type { TimelineStep } from "./run-format";

export interface StepRunDrawerProps {
  step: TimelineStep | null;
  blockType?: string;
  onClose(): void;
}

/** The step's details as a side sheet, for the activity trace. */
export function StepRunDrawer({ step, blockType, onClose }: StepRunDrawerProps) {
  return (
    <Sheet open={step !== null} onOpenChange={(open) => !open && onClose()}>
      <SheetContent className="flex w-full flex-col gap-4 overflow-y-auto sm:max-w-xl">
        <SheetTitle className="sr-only">{step ? `Step ${step.blockId}` : "Step"}</SheetTitle>
        <SheetDescription className="sr-only">
          What this step did: its inputs, outputs and output text.
        </SheetDescription>
        {step && <StepDetails step={step} {...(blockType !== undefined ? { blockType } : {})} />}
      </SheetContent>
    </Sheet>
  );
}
