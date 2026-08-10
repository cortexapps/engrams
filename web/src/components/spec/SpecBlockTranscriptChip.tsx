import type { ToolCallMessagePartProps } from "@assistant-ui/react";
import {
  CheckCircle2Icon,
  ChevronDownIcon,
  GitCommitHorizontalIcon,
  Loader2Icon,
  XCircleIcon,
} from "lucide-react";

interface SpecBlockUpdateArgs {
  block_id?: unknown;
  section_id?: unknown;
  source?: unknown;
}

export function SpecBlockTranscriptChip({
  args,
  result,
  status,
}: ToolCallMessagePartProps<SpecBlockUpdateArgs, unknown>) {
  const blockId = typeof args?.block_id === "string" ? args.block_id : "block";
  const sectionId = typeof args?.section_id === "string" ? args.section_id : null;
  const source = typeof args?.source === "string" ? args.source : null;
  const output = readResult(result);
  const checkpointId = readString(output, "checkpoint_id");
  const running = status?.type === "running";
  const failed =
    status?.type === "incomplete" ||
    readString(output, "error") !== null ||
    output?.["applied"] === false;

  return (
    <div
      className="w-full rounded-lg border border-border/70 bg-muted/35 px-3 py-2 text-sm"
      data-testid="spec-block-transcript-chip"
      role="status"
    >
      <div className="flex min-w-0 items-center gap-2">
        <GitCommitHorizontalIcon className="size-3.5 shrink-0 text-primary" />
        <span className="min-w-0 flex-1 truncate text-foreground">
          {running ? "Updating" : failed ? "Could not update" : "Updated"} block {blockId}
        </span>
        {sectionId ? (
          <span className="shrink-0 text-xs text-muted-foreground">§{sectionId}</span>
        ) : null}
        {running ? (
          <Loader2Icon
            aria-label="Block update running"
            className="size-3.5 shrink-0 animate-spin text-muted-foreground"
          />
        ) : failed ? (
          <XCircleIcon
            aria-label="Block update failed"
            className="size-3.5 shrink-0 text-destructive"
          />
        ) : (
          <CheckCircle2Icon
            aria-label="Block update checkpointed"
            className="size-3.5 shrink-0 text-instrument-nominal-ink"
          />
        )}
      </div>
      {!running && !failed && checkpointId ? (
        <div className="mt-1 text-xs text-muted-foreground">
          checkpoint {checkpointId.slice(0, 8)}
        </div>
      ) : null}
      {source ? (
        <details className="mt-2 border-t pt-2 text-xs">
          <summary className="flex cursor-pointer list-none items-center gap-1 text-muted-foreground">
            <ChevronDownIcon className="size-3" />
            Source change
          </summary>
          <pre className="mt-2 max-h-48 overflow-auto whitespace-pre-wrap text-foreground">
            {source}
          </pre>
        </details>
      ) : null}
    </div>
  );
}

function readResult(result: unknown): Record<string, unknown> | null {
  let value = result;
  if (typeof value === "string") {
    try {
      value = JSON.parse(value) as unknown;
    } catch {
      return null;
    }
  }
  if (value === null || typeof value !== "object" || Array.isArray(value)) return null;
  return value as Record<string, unknown>;
}

function readString(value: Record<string, unknown> | null, key: string): string | null {
  const field = value?.[key];
  return typeof field === "string" ? field : null;
}
