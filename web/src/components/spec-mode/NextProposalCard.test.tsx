import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, test, vi } from "vitest";

import { NextProposalCard, proposalContent } from "./NextProposalCard";
import type { NextProposal } from "./spec-surface";

const CASES: Array<{ next: Exclude<NextProposal, null>; copy: RegExp }> = [
  {
    next: { kind: "draft_section", sectionId: "api", sectionTitle: "API" },
    copy: /Draft §API/,
  },
  {
    next: { kind: "settle_section", sectionId: "design", sectionTitle: "Proposed design" },
    copy: /Review §Proposed design/,
  },
  { next: { kind: "look_for_breakage" }, copy: /Look for what breaks/ },
];

describe("NextProposalCard", () => {
  test.each(CASES)("renders and posts the $next.kind proposal", async ({ next, copy }) => {
    const user = userEvent.setup();
    const onSend = vi.fn(async () => undefined);
    const content = proposalContent(next)!;
    render(<NextProposalCard next={next} onSend={onSend} />);

    expect(screen.getByRole("heading", { name: "Next — I propose" })).toBeTruthy();
    expect(screen.getByText(copy, { selector: "p" })).toBeTruthy();
    await user.click(screen.getByRole("button", { name: content.primaryLabel }));

    await waitFor(() => expect(onSend).toHaveBeenCalledWith(content.primaryPrompt));
  });

  test("posts the alternate action through the same send seam", async () => {
    const user = userEvent.setup();
    const onSend = vi.fn(async () => undefined);
    const next: Exclude<NextProposal, null> = {
      kind: "draft_section",
      sectionId: "api",
      sectionTitle: "API",
    };
    const content = proposalContent(next)!;
    render(<NextProposalCard next={next} onSend={onSend} />);

    await user.click(screen.getByRole("button", { name: content.secondaryLabel }));
    await waitFor(() => expect(onSend).toHaveBeenCalledWith(content.secondaryPrompt));
  });
});
