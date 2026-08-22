/** Launch an editor dry run (ADR 0119 phase 3.5): a real run row flagged
 * dry_run, integration actions stubbed to "would have posted …". Navigates to
 * the run page so the timeline shows it live. */

import { FlaskConical } from "lucide-react";
import { useNavigate } from "@tanstack/react-router";
import { toast } from "sonner";

import { Button } from "@/components/ui/button";
import { useDryRun } from "@/hooks/useAutomationCode";

export interface DryRunButtonProps {
  automationId: string;
  /** Selected sample to drive the run (3.4); absent → the server picks the latest. */
  sampleId?: string;
  disabled?: boolean;
}

export function DryRunButton({ automationId, sampleId, disabled }: DryRunButtonProps) {
  const navigate = useNavigate();
  const dryRun = useDryRun();
  return (
    <Button
      type="button"
      variant="outline"
      disabled={disabled || dryRun.isPending}
      data-testid="dry-run-button"
      onClick={() =>
        dryRun.mutate(
          {
            automationId,
            ...(sampleId !== undefined ? { sample: { case: "sampleId", value: sampleId } } : {}),
          },
          {
            onSuccess: ({ runId }) => {
              toast.success("Dry run started");
              // 3.7's run page route; addressed by href so this compiles on a
              // base where that route is not registered yet.
              void navigate({ href: `/settings/automations/${automationId}/runs/${runId}` });
            },
            onError: (error) => toast.error(`Dry run failed: ${error.message}`),
          },
        )
      }
    >
      <FlaskConical className="size-4" aria-hidden />
      {dryRun.isPending ? "Starting…" : "Dry run"}
    </Button>
  );
}
