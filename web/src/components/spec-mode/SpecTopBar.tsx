import { SpecPublishControl } from "@/components/spec/SpecPublishControl";
import { Text } from "@/components/ui/text";
import type { SpecCheckpointSummary } from "@/hooks/useSpecRead";
import { CheckpointButton } from "./CheckpointButton";

export function SpecTopBar({
  specId,
  title,
  templateName,
  checkpoints,
  viewerIsOwner,
}: {
  specId: string;
  title: string;
  templateName: string;
  checkpoints: SpecCheckpointSummary[];
  viewerIsOwner: boolean;
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
        {/* F5 fills this region from the page-owned provider awareness. */}
        <div className="spec-mode-presence-region" aria-label="Presence" />
        <span className="spec-mode-top-separator" aria-hidden="true" />
        <CheckpointButton specId={specId} checkpoints={checkpoints} />
        {viewerIsOwner ? (
          <SpecPublishControl specId={specId} onReviewSection={() => undefined} />
        ) : null}
      </div>
    </header>
  );
}
