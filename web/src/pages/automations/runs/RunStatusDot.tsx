import { StatusDot } from "@/components/status-dot";
import { runStatusTone } from "@/lib/automations";
import { runStatusLabel } from "./run-format";

/** A run or step status as the one StatusDot, named for a screen reader. */
export function RunStatusDot({
  status,
  size = 8,
  className,
}: {
  status: string;
  size?: 6 | 8 | 10;
  className?: string;
}) {
  return (
    <StatusDot
      tone={runStatusTone(status)}
      size={size}
      label={`status ${runStatusLabel(status)}`}
      className={className}
    />
  );
}
