import type { ToolCallMessagePartProps } from "@assistant-ui/react";
import type { TrackedEditTranscriptChip as TrackedEditChipData } from "@engrams/spec-document";

import { ToolFallback } from "@/components/assistant-ui/tool-fallback";
import { TrackedEditTranscriptChip } from "@/components/spec";

export function isSpecUpdateSectionTool(name: string): boolean {
  return name === "spec_update_section" || name === "mcp__engrams__spec_update_section";
}

export function TrackedEditToolPart(props: ToolCallMessagePartProps) {
  const chip = trackedEditChip(props.result);
  if (!chip) return <ToolFallback {...props} />;
  return <TrackedEditTranscriptChip chip={chip} />;
}

function trackedEditChip(result: unknown): TrackedEditChipData | null {
  let value = result;
  if (typeof value === "string") {
    try {
      value = JSON.parse(value) as unknown;
    } catch {
      return null;
    }
  }
  if (!value || typeof value !== "object" || Array.isArray(value)) return null;
  const chip = Reflect.get(value, "transcript_chip");
  if (!chip || typeof chip !== "object" || Array.isArray(chip)) return null;
  if (
    Reflect.get(chip, "kind") !== "spec_tracked_edit" ||
    typeof Reflect.get(chip, "specId") !== "string" ||
    typeof Reflect.get(chip, "sectionId") !== "string" ||
    typeof Reflect.get(chip, "before") !== "string" ||
    typeof Reflect.get(chip, "after") !== "string"
  ) {
    return null;
  }
  return {
    kind: "spec_tracked_edit",
    specId: Reflect.get(chip, "specId"),
    sectionId: Reflect.get(chip, "sectionId"),
    before: Reflect.get(chip, "before"),
    after: Reflect.get(chip, "after"),
  };
}
