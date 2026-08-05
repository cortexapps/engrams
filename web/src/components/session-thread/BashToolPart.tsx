import { ChevronDownIcon, ScrollTextIcon, TerminalIcon } from "lucide-react";
import type { ToolCallMessagePartProps } from "@assistant-ui/react";
import { Collapsible, CollapsibleContent, CollapsibleTrigger } from "@/components/ui/collapsible";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { cn } from "@/lib/utils";
import { tailFileId } from "@/lib/bash-tail";
import { useWorkPaneActions } from "./work-pane-actions";

// Renders the agent's `Bash` tool call in shell vocabulary (`$ command`,
// running badge, collapsible result) instead of the generic JSON card — and
// carries the Tail action: the harness tees every Bash call's output to a
// guest-side log (see lib/bash-tail.ts), so one click opens the Processes
// pane following that log live.

type BashArgs = {
  command?: string;
  description?: string;
};

export function BashToolPart({
  toolCallId,
  args,
  argsText,
  result,
  status,
}: ToolCallMessagePartProps<BashArgs, string>) {
  const paneActions = useWorkPaneActions();
  const running = status?.type === "running";
  const cancelled = status?.type === "incomplete" && status.reason === "cancelled";
  const failed = status?.type === "incomplete" && !cancelled;
  // args_summary is truncated at 1 KB on the wire; a truncated JSON parses to
  // undefined and the raw text is the honest fallback.
  const command = args?.command ?? argsText ?? "";
  const output = typeof result === "string" ? result.trimEnd() : "";
  const hasOutput = output.length > 0;
  const tailable = tailFileId(toolCallId) != null;

  return (
    <Collapsible
      className={cn("w-full rounded-lg border bg-card/40", cancelled && "opacity-60")}
      disabled={!hasOutput}
    >
      <div className="flex items-center gap-1 pr-2">
        <CollapsibleTrigger
          className={cn(
            "group flex min-w-0 flex-1 items-center gap-2 px-3 py-2 text-left font-mono text-sm",
            hasOutput ? "cursor-pointer" : "cursor-default",
          )}
        >
          <TerminalIcon className="size-3.5 shrink-0 text-muted-foreground" />
          <span className="text-muted-foreground">$</span>
          <span
            className={cn("min-w-0 flex-1 truncate text-foreground", cancelled && "line-through")}
            title={args?.description ?? command}
          >
            {command}
          </span>

          {running ? (
            <Badge variant="secondary" className="shrink-0 animate-pulse">
              running…
            </Badge>
          ) : cancelled ? (
            <Badge variant="outline" className="shrink-0">
              cancelled
            </Badge>
          ) : failed ? (
            <Badge variant="destructive" className="shrink-0">
              failed
            </Badge>
          ) : null}

          {hasOutput && (
            <ChevronDownIcon className="size-3.5 shrink-0 text-muted-foreground transition-transform group-data-[state=open]:rotate-180" />
          )}
        </CollapsibleTrigger>

        {/* Sibling of the trigger, never inside it (no nested buttons). */}
        {paneActions && tailable && (
          <Button
            type="button"
            variant="ghost"
            size="icon-xs"
            className="shrink-0 text-muted-foreground hover:text-foreground"
            onClick={() => paneActions.openProcesses(toolCallId)}
            aria-label="Tail command output"
            title={
              running
                ? "Follow this command's live output in the Processes pane"
                : "Open this command's full output log in the Processes pane"
            }
          >
            <ScrollTextIcon />
          </Button>
        )}
      </div>

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
