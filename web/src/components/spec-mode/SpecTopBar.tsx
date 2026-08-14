import { SpecPublishControl } from "@/components/spec/SpecPublishControl";
import { Switch } from "@/components/ui/switch";
import { Text } from "@/components/ui/text";
import type { SpecCheckpointSummary } from "@/hooks/useSpecRead";
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
        <Text as="h1" variant="heading" className="spec-mode-title">
          {title}
        </Text>
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
          <SpecPublishControl specId={specId} onReviewSection={() => undefined} />
        ) : null}
      </div>
    </header>
  );
}
