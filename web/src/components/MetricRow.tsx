import type { ReactNode } from "react";

// The instrument rail's key/value row: a quiet lowercase label on the left,
// the machine value (mono, tabular) hard-right against it. Shared so the
// session-identity block (image / created / events) and the durability
// readouts (dirty / size / locality / rpo) line up to the same baseline.
export function MetricRow({
  label,
  value,
  title,
}: {
  label: string;
  value: ReactNode;
  title?: string;
}) {
  return (
    <div className="flex items-baseline justify-between gap-3" title={title}>
      <dt className="text-muted-foreground">{label}</dt>
      <dd className="font-mono tabular-nums text-foreground">{value}</dd>
    </div>
  );
}
