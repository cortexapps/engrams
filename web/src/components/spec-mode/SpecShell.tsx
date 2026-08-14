import type { ReactNode, RefObject, UIEventHandler } from "react";

import type { SpecCheckpointSummary } from "@/hooks/useSpecRead";
import { ConversationRail } from "./ConversationRail";
import { SectionList } from "./SectionList";
import type { SpecPresenceEntry } from "./section-presence";
import type { SpecSurface } from "./spec-surface";
import { SpecTopBar } from "./SpecTopBar";
import "./spec-mode.css";

export function SpecShell({
  specId,
  title,
  templateName,
  checkpoints,
  viewerIsOwner,
  surface,
  presence = [],
  onSelectSection = () => undefined,
  documentPaneRef,
  onDocumentScroll,
  showProvenance = true,
  onShowProvenanceChange = () => undefined,
  children,
}: {
  specId: string;
  title: string;
  templateName: string;
  checkpoints: SpecCheckpointSummary[];
  viewerIsOwner: boolean;
  surface?: SpecSurface;
  presence?: SpecPresenceEntry[];
  onSelectSection?: (sectionId: string) => void;
  documentPaneRef?: RefObject<HTMLElement | null>;
  onDocumentScroll?: UIEventHandler<HTMLElement>;
  showProvenance?: boolean;
  onShowProvenanceChange?: (visible: boolean) => void;
  children?: ReactNode;
}) {
  return (
    // Fills the app layout rather than the viewport: the sidebar lives beside
    // this, and a fixed-position shell would cover it.
    <div
      className="spec-mode-viewport"
      data-testid="spec-mode-viewport"
      style={{ height: "100%", overflowX: "auto", overflowY: "hidden" }}
    >
      <div
        className="spec-mode-shell"
        style={{
          display: "grid",
          gridTemplateColumns: "236px minmax(0, 1fr) 392px",
          minWidth: 1188,
        }}
      >
        <SpecTopBar
          specId={specId}
          title={title}
          templateName={templateName}
          checkpoints={checkpoints}
          viewerIsOwner={viewerIsOwner}
          presence={presence}
          sections={surface?.sections ?? []}
          showProvenance={showProvenance}
          onShowProvenanceChange={onShowProvenanceChange}
        />
        <aside className="spec-mode-sections" aria-label="Spec sections">
          {surface ? (
            <SectionList surface={surface} presence={presence} onSelectSection={onSelectSection} />
          ) : null}
        </aside>
        <section
          ref={documentPaneRef}
          className="spec-mode-document"
          aria-label="Spec document"
          data-scroll="doc"
          style={{ overflowY: "auto" }}
          onScroll={onDocumentScroll}
        >
          <div className="spec-mode-document-inner">{children}</div>
        </section>
        <aside className="spec-mode-conversation" aria-label="Conversation">
          {surface ? (
            <ConversationRail
              specId={specId}
              surface={surface}
              presence={presence}
              onSelectSection={onSelectSection}
            />
          ) : null}
        </aside>
      </div>
    </div>
  );
}
