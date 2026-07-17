import { ArrowRightIcon, ExternalLinkIcon } from "lucide-react";

import { usePrRefs } from "../hooks/usePrRefs";
import { errorMessage } from "../lib/errors";
import { Card, CardContent } from "@/components/ui/card";

export interface SideEffectsPanelProps {
  taskId: string | null;
  sessionId: string;
}

export function SideEffectsPanel({ taskId, sessionId }: SideEffectsPanelProps) {
  const { data, error, isPending } = usePrRefs(taskId, sessionId);
  const prRefs = data?.prRefs ?? [];

  return (
    <div className="h-full overflow-y-auto p-4">
      {isPending && <p className="text-sm text-muted-foreground">Loading…</p>}

      {!isPending && error && prRefs.length === 0 && (
        <p role="alert" className="text-sm text-destructive">
          Couldn’t load side effects. {errorMessage(error)}
        </p>
      )}

      {!isPending && !error && prRefs.length === 0 && (
        <div className="flex h-full items-center justify-center text-center">
          <p className="text-sm text-muted-foreground">No side effects recorded</p>
        </div>
      )}

      <div className="flex flex-col gap-3">
        {prRefs.map((prRef) => {
          const heading = (
            <span className="min-w-0">
              <span className="block truncate font-mono text-xs text-muted-foreground">
                {prRef.repo}#{prRef.prNumber}
              </span>
              <span className="mt-1 block text-sm font-medium group-hover:underline">
                {prRef.title || "Pull request"}
              </span>
            </span>
          );
          return (
            <Card key={prRef.id} className="py-0">
              <CardContent className="flex flex-col gap-2 p-4">
                {prRef.url ? (
                  <a
                    href={prRef.url}
                    target="_blank"
                    rel="noreferrer"
                    className="group flex min-w-0 items-start justify-between gap-3"
                  >
                    {heading}
                    <ExternalLinkIcon className="mt-0.5 size-3.5 shrink-0 text-muted-foreground" />
                  </a>
                ) : (
                  <div className="flex min-w-0 items-start justify-between gap-3">{heading}</div>
                )}

                {prRef.headBranch && prRef.baseBranch && (
                  <div className="flex min-w-0 items-center gap-1.5 font-mono text-xs text-muted-foreground">
                    <span className="truncate">{prRef.headBranch}</span>
                    <ArrowRightIcon className="size-3 shrink-0" aria-hidden />
                    <span className="truncate">{prRef.baseBranch}</span>
                  </div>
                )}
              </CardContent>
            </Card>
          );
        })}
      </div>
    </div>
  );
}
