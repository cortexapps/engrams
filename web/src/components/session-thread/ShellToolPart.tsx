import { ChevronDownIcon, TerminalIcon } from "lucide-react";
import type { ToolCallMessagePartProps } from "@assistant-ui/react";
import { Collapsible, CollapsibleContent, CollapsibleTrigger } from "@/components/ui/collapsible";
import { Badge } from "@/components/ui/badge";
import { cn } from "@/lib/utils";
import { fmtDur } from "../transcriptFmt";
import type { ShellArgs } from "./buildMessages";

// Renders an `engram.shell` tool part — a sandbox/operator shell exec, which
// is a distinct thing from an agent tool call (those fall through to
// ToolFallback). Shell vocabulary: `$ command`, an exit-status badge, the
// duration, and collapsible stdout/stderr. Built on the shadcn Collapsible +
// Badge so it shares the app's surface tokens.

export function ShellToolPart({
  args,
  result,
  status,
}: ToolCallMessagePartProps<ShellArgs, string>) {
  const running = status?.type === "running";
  const command = args?.command ?? "";
  const exit = args?.exit;
  const durationMs = args?.durationMs;
  const failed = !running && exit != null && exit !== 0;
  const output = typeof result === "string" ? result.trimEnd() : "";
  const hasOutput = output.length > 0;

  return (
    <Collapsible className="w-full rounded-lg border bg-card/40" disabled={!hasOutput}>
      <CollapsibleTrigger
        className={cn(
          "group flex w-full items-center gap-2 px-3 py-2 text-left font-mono text-sm",
          hasOutput ? "cursor-pointer" : "cursor-default",
        )}
      >
        <TerminalIcon className="size-3.5 shrink-0 text-muted-foreground" />
        <span className="text-muted-foreground">$</span>
        <span className="min-w-0 flex-1 truncate text-foreground">{command}</span>

        {running ? (
          <Badge variant="secondary" className="shrink-0 animate-pulse">
            running…
          </Badge>
        ) : exit != null ? (
          <Badge variant={failed ? "destructive" : "outline"} className="shrink-0 tabular-nums">
            exit {exit}
          </Badge>
        ) : null}

        {durationMs != null && (
          <span className="shrink-0 text-xs text-muted-foreground tabular-nums">
            {fmtDur(durationMs)}
          </span>
        )}

        {hasOutput && (
          <ChevronDownIcon className="size-3.5 shrink-0 text-muted-foreground transition-transform group-data-[state=open]:rotate-180" />
        )}
      </CollapsibleTrigger>

      {hasOutput && (
        <CollapsibleContent>
          <pre className="max-h-96 overflow-auto border-t bg-muted/40 px-3 py-2 font-mono text-xs leading-relaxed whitespace-pre-wrap text-foreground">
            {output}
          </pre>
        </CollapsibleContent>
      )}
    </Collapsible>
  );
}
