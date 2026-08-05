import { lazy, Suspense, useMemo } from "react";
import { ChevronDownIcon, FileDiffIcon } from "lucide-react";
import { Collapsible, CollapsibleContent, CollapsibleTrigger } from "@/components/ui/collapsible";
import { cn } from "@/lib/utils";
import type { IndexedEvent } from "../events";
import { beforeAfter, extractFileChanges, totalCounts } from "./session-thread/fileChanges";

const PierreDiff = lazy(() => import("./session-thread/PierreDiff"));

export function ChangesPane({ events }: { events: IndexedEvent[] }) {
  const files = useMemo(() => extractFileChanges(events), [events]);
  const totals = totalCounts(files);

  if (files.length === 0) {
    return (
      <div className="flex h-full flex-col items-center justify-center px-6 text-center">
        <p className="text-sm text-muted-foreground italic">No file changes yet.</p>
        <p className="mt-1 text-xs text-muted-foreground">
          Only edits made through the agent&apos;s file tools appear here.
        </p>
      </div>
    );
  }

  return (
    <div className="flex h-full min-h-0 flex-col">
      <div className="flex shrink-0 items-center justify-between border-b px-4 py-2 text-xs text-muted-foreground">
        <span>
          {files.length} {files.length === 1 ? "file" : "files"} changed
        </span>
        <span className="flex items-center gap-2 font-mono tabular-nums">
          <span className="text-emerald-600 dark:text-emerald-400">+{totals.additions}</span>
          <span className="text-red-600 dark:text-red-400">−{totals.deletions}</span>
        </span>
      </div>
      <div className="min-h-0 flex-1 space-y-2 overflow-auto p-3">
        {files.map((file) => (
          <Collapsible key={file.path} className="w-full rounded-lg border bg-card/40">
            <CollapsibleTrigger className="group flex w-full cursor-pointer items-center gap-2 px-3 py-2 text-left text-sm">
              <FileDiffIcon className="size-3.5 shrink-0 text-muted-foreground" />
              <span className="min-w-0 flex-1 truncate font-mono text-foreground">{file.path}</span>
              {file.additions > 0 && (
                <span className="shrink-0 font-mono text-xs tabular-nums text-emerald-600 dark:text-emerald-400">
                  +{file.additions}
                </span>
              )}
              {file.deletions > 0 && (
                <span className="shrink-0 font-mono text-xs tabular-nums text-red-600 dark:text-red-400">
                  −{file.deletions}
                </span>
              )}
              {file.changes.length > 1 && (
                <span className="shrink-0 text-xs text-muted-foreground">
                  ×{file.changes.length}
                </span>
              )}
              <ChevronDownIcon
                className={cn(
                  "size-3.5 shrink-0 text-muted-foreground transition-transform",
                  "group-data-[state=open]:rotate-180",
                )}
              />
            </CollapsibleTrigger>
            <CollapsibleContent>
              {file.changes.map((entry) => {
                const { before, after } = beforeAfter(entry.change);
                return (
                  <div
                    key={entry.toolCallId}
                    className="max-h-[32rem] overflow-auto border-t text-xs"
                  >
                    <Suspense
                      fallback={
                        <div className="px-3 py-2 text-xs text-muted-foreground italic">
                          Loading diff…
                        </div>
                      }
                    >
                      <PierreDiff path={file.path} before={before} after={after} />
                    </Suspense>
                  </div>
                );
              })}
            </CollapsibleContent>
          </Collapsible>
        ))}
      </div>
    </div>
  );
}
