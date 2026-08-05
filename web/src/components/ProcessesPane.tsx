import { useMemo } from "react";
import { ArrowLeft, TerminalIcon } from "lucide-react";

import { TerminalPane } from "./TerminalPane";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { bashTailCommand } from "@/lib/bash-tail";
import type { IndexedEvent } from "../lib/types";

// The Processes view: the agent's own Bash commands, derived from the event
// stream the page already holds — not a guest-wide `ps`. Clicking a command
// swaps in a terminal that is born tailing its output log (TerminalPane's
// bootstrap contract; the log exists because the harness hook tees every
// Bash call, see lib/bash-tail.ts).

export interface BashCommandRow {
  toolCallId: string;
  command: string;
  done: boolean;
  ok?: boolean;
  durationMs?: number;
}

/** Running commands first (newest first), then finished ones (newest first). */
export function deriveBashCommands(events: IndexedEvent[]): BashCommandRow[] {
  const byId = new Map<string, BashCommandRow>();
  for (const { event } of events) {
    if (event.type === "tool_call_started" && event.tool_name === "Bash") {
      // args_summary is JSON truncated at 1 KB on the wire; a truncated
      // summary fails to parse and the raw text is the honest fallback.
      let command = event.args_summary ?? "";
      try {
        const parsed = JSON.parse(command) as { command?: unknown };
        if (typeof parsed?.command === "string") command = parsed.command;
      } catch {
        /* keep raw */
      }
      byId.set(event.tool_call_id, { toolCallId: event.tool_call_id, command, done: false });
    } else if (event.type === "tool_call_completed") {
      const row = byId.get(event.tool_call_id);
      if (row) {
        row.done = true;
        row.ok = event.ok;
        row.durationMs = event.duration_ms;
      }
    }
  }
  const rows = [...byId.values()].reverse();
  return [...rows.filter((r) => !r.done), ...rows.filter((r) => r.done)];
}

function StatusBadge({ row }: { row: BashCommandRow }) {
  if (!row.done)
    return (
      <Badge variant="secondary" className="shrink-0 animate-pulse">
        running…
      </Badge>
    );
  if (row.ok === false)
    return (
      <Badge variant="destructive" className="shrink-0">
        failed
      </Badge>
    );
  return (
    <span className="shrink-0 font-mono text-xs text-muted-foreground">
      {row.durationMs != null ? `${(row.durationMs / 1000).toFixed(1)}s` : "done"}
    </span>
  );
}

export interface ProcessesPaneProps {
  sessionId: string;
  events: IndexedEvent[];
  /** Bash tool_call_id being tailed, or null for the command list. */
  tailId: string | null;
  onTail: (toolCallId: string | null) => void;
}

export function ProcessesPane({ sessionId, events, tailId, onTail }: ProcessesPaneProps) {
  const commands = useMemo(() => deriveBashCommands(events), [events]);
  const tailCommand = tailId ? bashTailCommand(tailId) : null;

  if (tailId && tailCommand) {
    const row = commands.find((c) => c.toolCallId === tailId);
    return (
      <div className="flex h-full min-h-0 flex-col">
        <div className="flex h-9 shrink-0 items-center gap-2 border-b pr-3 pl-1.5">
          <Button
            variant="ghost"
            size="icon-xs"
            className="shrink-0 text-muted-foreground hover:text-foreground"
            onClick={() => onTail(null)}
            aria-label="Back to command list"
            title="All commands"
          >
            <ArrowLeft />
          </Button>
          <span className="font-mono text-xs text-muted-foreground">$</span>
          <span className="min-w-0 flex-1 truncate font-mono text-xs" title={row?.command}>
            {row?.command ?? tailId}
          </span>
          {row && <StatusBadge row={row} />}
        </div>
        <div className="min-h-0 flex-1">
          <TerminalPane sessionId={sessionId} bootstrap={tailCommand} />
        </div>
      </div>
    );
  }

  if (commands.length === 0) {
    return (
      <div className="flex h-full flex-col items-center justify-center gap-2 p-6 text-center">
        <TerminalIcon className="size-5 text-muted-foreground" />
        <p className="text-sm text-muted-foreground">
          No commands yet. The agent's Bash commands appear here, and you can follow their output
          live.
        </p>
      </div>
    );
  }

  return (
    <div className="h-full overflow-y-auto p-2">
      <ul className="flex flex-col gap-1">
        {commands.map((c) => (
          <li key={c.toolCallId}>
            <button
              type="button"
              onClick={() => onTail(c.toolCallId)}
              className="flex w-full items-center gap-2 rounded-md border bg-card/40 px-3 py-2 text-left transition-colors hover:bg-accent/50"
              title="Follow this command's output"
            >
              <span className="shrink-0 font-mono text-sm text-muted-foreground">$</span>
              <span className="min-w-0 flex-1 truncate font-mono text-sm">{c.command}</span>
              <StatusBadge row={c} />
            </button>
          </li>
        ))}
      </ul>
    </div>
  );
}
