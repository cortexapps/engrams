import { ChevronDownIcon, FileDiffIcon } from "lucide-react";
import { lazy, Suspense, useMemo } from "react";
import type { ToolCallMessagePartProps } from "@assistant-ui/react";
import { Collapsible, CollapsibleContent, CollapsibleTrigger } from "@/components/ui/collapsible";
import { cn } from "@/lib/utils";
import type { FileChangeArgs } from "./buildMessages";
import { beforeAfter, countChange } from "./fileChanges";

// ADR 0054 Flavor A: renders an `engram.fileChange` tool part — a
// Write/Edit/MultiEdit the harness confirmed succeeded (`file_changed`),
// shown in place of the generic tool card. Collapsed it's a one-line row:
// path + `+N −M` counts; expanded it shows the full diff. The counts are
// computed eagerly with jsdiff (cheap, no Shiki) so the file list reads
// without loading anything; the rich renderer (Pierre + Shiki) is lazy and
// only loads on expand.

const PierreDiff = lazy(() => import("./PierreDiff"));

export function FileChangePart({ args }: ToolCallMessagePartProps<FileChangeArgs, unknown>) {
  const path = args?.path ?? "";
  const { before, after } = useMemo(
    () => (args ? beforeAfter(args.change) : { before: "", after: "" }),
    [args],
  );
  const { additions, deletions } = useMemo(
    () => (args ? countChange(args.change) : { additions: 0, deletions: 0 }),
    [args],
  );

  return (
    <Collapsible className="w-full rounded-lg border bg-card/40">
      <CollapsibleTrigger className="group flex w-full cursor-pointer items-center gap-2 px-3 py-2 text-left text-sm">
        <FileDiffIcon className="size-3.5 shrink-0 text-muted-foreground" />
        <span className="min-w-0 flex-1 truncate font-mono text-foreground">{path}</span>
        {additions > 0 && (
          <span className="shrink-0 font-mono text-xs tabular-nums text-instrument-nominal-ink">
            +{additions}
          </span>
        )}
        {deletions > 0 && (
          <span className="shrink-0 font-mono text-xs tabular-nums text-instrument-critical-ink">
            −{deletions}
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
        <div className="max-h-[32rem] overflow-auto border-t text-xs">
          <Suspense
            fallback={
              <div className="px-3 py-2 text-xs text-muted-foreground italic">Loading diff…</div>
            }
          >
            <PierreDiff path={path} before={before} after={after} />
          </Suspense>
        </div>
      </CollapsibleContent>
    </Collapsible>
  );
}
