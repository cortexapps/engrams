import { render, screen } from "@testing-library/react";
import { describe, expect, test } from "vitest";

import { TrackedEditTranscriptChip } from "./TrackedEditTranscriptChip";

describe("TrackedEditTranscriptChip", () => {
  test("shows the exact before and after text", () => {
    render(
      <TrackedEditTranscriptChip
        chip={{
          kind: "spec_tracked_edit",
          specId: "00000000-0000-4000-8000-000000000112",
          sectionId: "failure-modes",
          before: "Retry forever.",
          after: "Retry three times.",
        }}
      />,
    );

    expect(screen.getByRole("status", { name: "Tracked spec edit" })).toBeTruthy();
    expect(screen.getByText("Retry forever.").tagName).toBe("DEL");
    expect(screen.getByText("Retry three times.").tagName).toBe("INS");
  });
});
