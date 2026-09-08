/** Launch an editor dry run (ADR 0119 phase 3.5): a real run row flagged
 * dry_run that boots no session, sends no prompt, runs no command and posts
 * nothing — every side-effecting block records what it would have done and
 * waits resolve at once. Navigates to the run page so the timeline shows it
 * live. */

import { FlaskConical } from "lucide-react";
import { useNavigate } from "@tanstack/react-router";
import { toast } from "sonner";

import { Button } from "@/components/ui/button";
import { useDryRun } from "@/hooks/useAutomationCode";

export interface DryRunButtonProps {
  automationId: string;
  /** Selected sample to drive the run (3.4); absent → the server picks the latest. */
  sampleId?: string;
  /** D9: which entrypoint to dry-run (default "main"). */
  entrypointId?: string;
  disabled?: boolean;
}

export function DryRunButton({
  automationId,
  sampleId,
  entrypointId,
  disabled,
}: DryRunButtonProps) {
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
            ...(entrypointId !== undefined && entrypointId !== "main" ? { entrypointId } : {}),
          },
          {
            onSuccess: ({ runId }) => {
              toast.success("Dry run started");
              // 3.7's run page route; addressed by href so this compiles on a
              // base where that route is not registered yet.
              void navigate({ href: `/automations/${automationId}/runs/${runId}` });
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
