import type { ReactNode, RefObject, UIEventHandler } from "react";

import type { SpecCheckpointSummary } from "@/hooks/useSpecRead";
import { SectionList } from "./SectionList";
import type { SpecSurface } from "./spec-surface";
import { SpecSpine } from "./SpecSpine";
import { SpecTopBar } from "./SpecTopBar";
import "./spec-mode.css";

export function SpecShell({
  specId,
  title,
  templateName,
  checkpoints,
  viewerIsOwner,
  surface,
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
  onSelectSection?: (sectionId: string) => void;
  documentPaneRef?: RefObject<HTMLElement | null>;
  onDocumentScroll?: UIEventHandler<HTMLElement>;
  showProvenance?: boolean;
  onShowProvenanceChange?: (visible: boolean) => void;
  children?: ReactNode;
}) {
  return (
    <div
      className="spec-mode-viewport"
      data-testid="spec-mode-viewport"
      style={{ position: "fixed", inset: 0, overflowX: "auto", overflowY: "hidden" }}
    >
      <div
        className="spec-mode-shell"
        style={{
          display: "grid",
          gridTemplateColumns: "52px 236px minmax(0, 1fr) 392px",
          minWidth: 1240,
        }}
      >
        <SpecSpine />
        <SpecTopBar
          specId={specId}
          title={title}
          templateName={templateName}
          checkpoints={checkpoints}
          viewerIsOwner={viewerIsOwner}
          showProvenance={showProvenance}
          onShowProvenanceChange={onShowProvenanceChange}
        />
        <aside className="spec-mode-sections" aria-label="Spec sections">
          {surface ? <SectionList surface={surface} onSelectSection={onSelectSection} /> : null}
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
        {/* F4 owns the shared conversation rail. */}
        <aside className="spec-mode-conversation" aria-label="Conversation" />
      </div>
    </div>
  );
}
