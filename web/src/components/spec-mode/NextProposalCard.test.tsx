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

  // A proposal comes from server state that only moves once the agent has done
  // the work, so an accepted card used to sit there still offering the same
  // instruction. Clicking twice sent it twice.
  test("stops offering a proposal it has already sent", async () => {
    const user = userEvent.setup();
    const onSend = vi.fn(async () => undefined);
    const next: Exclude<NextProposal, null> = {
      kind: "draft_section",
      sectionId: "api",
      sectionTitle: "API",
    };
    const content = proposalContent(next)!;
    const view = render(<NextProposalCard next={next} onSend={onSend} />);

    await user.click(screen.getByRole("button", { name: content.primaryLabel }));
    await waitFor(() => expect(onSend).toHaveBeenCalledTimes(1));

    expect(screen.queryByRole("button", { name: content.primaryLabel })).toBeNull();
    expect(screen.queryByRole("button", { name: content.secondaryLabel })).toBeNull();
    expect(screen.getByRole("status").textContent).toContain("Waiting for the agent");

    // The next proposal is a different ask, so it gets its own buttons.
    view.rerender(
      <NextProposalCard
        next={{ kind: "draft_section", sectionId: "design", sectionTitle: "Design" }}
        onSend={onSend}
      />,
    );
    expect(screen.getByRole("button", { name: "Do it" })).toBeTruthy();
  });

  test("keeps the buttons when the send fails, so it can be retried", async () => {
    const user = userEvent.setup();
    const onSend = vi.fn(async () => {
      throw new Error("offline");
    });
    const next: Exclude<NextProposal, null> = {
      kind: "draft_section",
      sectionId: "api",
      sectionTitle: "API",
    };
    render(<NextProposalCard next={next} onSend={onSend} />);

    await user.click(screen.getByRole("button", { name: "Do it" }));
    await waitFor(() => expect(screen.getByRole("alert")).toBeTruthy());
    expect(screen.getByRole("button", { name: "Do it" })).toBeTruthy();
  });
});
