import { lazy, Suspense } from "react";

import { Skeleton } from "@/components/ui/skeleton";
import type { SpecSelectionActions } from "./SpecSelectionActions";

const SpecCanvas = lazy(() =>
  import("./SpecCanvas").then((module) => ({ default: module.SpecCanvas })),
);

/** Keep the editor, Yjs, and ProseMirror out of the main application bundle. */
export function LazySpecCanvas({
  specId,
  selectionActions,
}: {
  specId: string;
  selectionActions?: SpecSelectionActions;
}) {
  return (
    <Suspense
      fallback={
        <div className="space-y-3 p-8" aria-label="Loading spec editor">
          <Skeleton className="h-7 w-2/5" />
          <Skeleton className="h-4 w-full" />
          <Skeleton className="h-4 w-5/6" />
        </div>
      }
    >
      <SpecCanvas specId={specId} selectionActions={selectionActions} />
    </Suspense>
  );
}
