import { useState, type KeyboardEvent } from "react";
import { toast } from "sonner";

import { SpecPublishConfirm } from "./SpecPublishConfirm";
import { Switch } from "@/components/ui/switch";
import { Text } from "@/components/ui/text";
import { useRenameSpec, type SpecCheckpointSummary } from "@/hooks/useSpecRead";
import { CheckpointButton } from "./CheckpointButton";
import { PresenceGroup } from "./PresenceGroup";
import type { SpecPresenceEntry } from "./section-presence";
import type { SpecSurfaceSection } from "./spec-surface";

export function SpecTopBar({
  specId,
  title,
  templateName,
  checkpoints,
  viewerIsOwner,
  presence,
  sections,
  showProvenance,
  onShowProvenanceChange,
}: {
  specId: string;
  title: string;
  templateName: string;
  checkpoints: SpecCheckpointSummary[];
  viewerIsOwner: boolean;
  presence: SpecPresenceEntry[];
  sections: SpecSurfaceSection[];
  showProvenance: boolean;
  onShowProvenanceChange: (visible: boolean) => void;
}) {
  return (
    <header className="spec-mode-top-bar">
      <div className="spec-mode-title-group">
        {viewerIsOwner ? (
          <EditableTitle specId={specId} title={title} />
        ) : (
          <Text as="h1" variant="heading" className="spec-mode-title">
            {title}
          </Text>
        )}
        <Text as="span" variant="code" tone="muted" className="spec-mode-template-name">
          {templateName}
        </Text>
      </div>
      <div className="spec-mode-top-actions">
        <PresenceGroup presence={presence} sections={sections} />
        {presence.length > 0 ? (
          <span className="spec-mode-top-separator" aria-hidden="true" />
        ) : null}
        <label className="spec-mode-provenance-toggle">
          <Text as="span" variant="label" tone="muted">
            Sources
          </Text>
          <Switch
            aria-label="Show provenance"
            checked={showProvenance}
            onCheckedChange={onShowProvenanceChange}
          />
        </label>
        <CheckpointButton specId={specId} checkpoints={checkpoints} />
        {viewerIsOwner ? (
          <SpecPublishConfirm specId={specId} viewerIsOwner={viewerIsOwner} />
        ) : null}
      </div>
    </header>
  );
}

/** The owner renames in place. Without this, a spec keeps whatever sentence
 *  created it as its name in every list, tab, and header. */
function EditableTitle({ specId, title }: { specId: string; title: string }) {
  const rename = useRenameSpec(specId);
  const [draft, setDraft] = useState<string | null>(null);

  const save = () => {
    const next = draft?.trim() ?? "";
    setDraft(null);
    if (next.length === 0 || next === title) return;
    rename.mutate(next, {
      onError: (error) =>
        toast.error("The rename did not save.", {
          description: error instanceof Error ? error.message : undefined,
        }),
    });
  };
  const keyDown = (event: KeyboardEvent<HTMLInputElement>) => {
    if (event.key === "Enter") save();
    if (event.key === "Escape") setDraft(null);
  };

  if (draft === null) {
    return (
      <button
        type="button"
        className="spec-mode-title-edit"
        title="Rename this spec"
        onClick={() => setDraft(title)}
      >
        <Text as="h1" variant="heading" className="spec-mode-title">
          {title}
        </Text>
      </button>
    );
  }
  return (
    <input
      className="spec-mode-title-input"
      value={draft}
      maxLength={200}
      autoFocus
      aria-label="Spec title"
      onChange={(event) => setDraft(event.target.value)}
      onBlur={save}
      onKeyDown={keyDown}
    />
  );
}
