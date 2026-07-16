import type { ToolCallMessagePartProps } from "@assistant-ui/react";
import { CheckCircle2Icon, Globe2Icon, Loader2Icon, XCircleIcon } from "lucide-react";
import type { BrowserActivityArgs } from "./buildMessages";

export function BrowserActivityPart({
  args,
  result,
  status,
}: ToolCallMessagePartProps<BrowserActivityArgs, string>) {
  const running = status?.type === "running";
  const failed = status?.type === "incomplete";
  const detail = failed && typeof result === "string" ? result.trim() : "";

  return (
    <div className="w-full rounded-lg border bg-card/40 px-3 py-2 text-sm">
      <div className="flex min-w-0 items-center gap-2">
        <Globe2Icon className="size-3.5 shrink-0 text-primary" />
        <span className="min-w-0 flex-1 truncate text-foreground">
          {args?.intent || "Using browser"}
        </span>
        {running ? (
          <Loader2Icon
            aria-label="Browser action running"
            className="size-3.5 shrink-0 animate-spin text-muted-foreground"
          />
        ) : failed ? (
          <XCircleIcon
            aria-label="Browser action failed"
            className="size-3.5 shrink-0 text-destructive"
          />
        ) : (
          <CheckCircle2Icon
            aria-label="Browser action completed"
            className="size-3.5 shrink-0 text-emerald-600 dark:text-emerald-400"
          />
        )}
      </div>
      {detail && (
        <pre className="mt-2 max-h-40 overflow-auto border-t pt-2 font-mono text-xs whitespace-pre-wrap text-destructive">
          {detail}
        </pre>
      )}
    </div>
  );
}
