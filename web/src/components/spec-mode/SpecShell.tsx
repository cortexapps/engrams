import type { ReactNode } from "react";

import type { SpecCheckpointSummary } from "@/hooks/useSpecRead";
import { SpecSpine } from "./SpecSpine";
import { SpecTopBar } from "./SpecTopBar";
import "./spec-mode.css";

export function SpecShell({
  specId,
  title,
  templateName,
  checkpoints,
  viewerIsOwner,
  children,
}: {
  specId: string;
  title: string;
  templateName: string;
  checkpoints: SpecCheckpointSummary[];
  viewerIsOwner: boolean;
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
        />
        {/* F3 owns the section list. Keep the structural region empty until then. */}
        <aside className="spec-mode-sections" aria-label="Spec sections" />
        <section
          className="spec-mode-document"
          aria-label="Spec document"
          data-scroll="doc"
          style={{ overflowY: "auto" }}
        >
          <div className="spec-mode-document-inner">{children}</div>
        </section>
        {/* F4 owns the shared conversation rail. */}
        <aside className="spec-mode-conversation" aria-label="Conversation" />
      </div>
    </div>
  );
}
