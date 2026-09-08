import { lazy, Suspense } from "react";
import type { WebsocketProvider } from "y-websocket";
import type * as Y from "yjs";

import { SkeletonRows } from "@/components/skeleton-rows";
import type { SpecPresenceEntry } from "@/components/spec-mode/section-presence";
import type { SpecSurface } from "@/components/spec-mode/spec-surface";
import type { SpecSelectionActions } from "./SpecSelectionActions";

const SpecCanvas = lazy(() =>
  import("./SpecCanvas").then((module) => ({ default: module.SpecCanvas })),
);

/** Keep the editor, Yjs, and ProseMirror out of the main application bundle. */
export function LazySpecCanvas({
  doc,
  provider,
  specId,
  revision,
  selectionActions,
  surface,
  presence,
  showProvenance,
  readOnly = false,
}: {
  doc: Y.Doc;
  provider: WebsocketProvider;
  specId: string;
  revision: string;
  surface: SpecSurface;
  presence: SpecPresenceEntry[];
  showProvenance: boolean;
  selectionActions?: SpecSelectionActions;
  readOnly?: boolean;
}) {
  return (
    <Suspense
      fallback={
        <div className="p-8" aria-label="Loading spec editor">
          <SkeletonRows rows={3} />
        </div>
      }
    >
      <SpecCanvas
        doc={doc}
        provider={provider}
        specId={specId}
        revision={revision}
        selectionActions={selectionActions}
        surface={surface}
        presence={presence}
        showProvenance={showProvenance}
        readOnly={readOnly}
      />
    </Suspense>
  );
}
