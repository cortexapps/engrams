import { describe, it, expect, vi } from "vitest";
import { screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { renderWithProviders } from "../../test-utils";
import { Reviews } from "./Reviews";

const review = {
  id: "review-1",
  repo: "cortexapps/engrams",
  prNumber: 100,
  status: "verifying",
  githubReviewId: "",
  findingCounts: { critical: 0, high: 1, medium: 0, low: 0, total: 1 },
};

const detail = {
  review,
  findings: [
    {
      id: "f1",
      reviewId: "review-1",
      path: "src/index.ts",
      startLine: 10,
      endLine: 12,
      severity: "high",
      category: "functional-correctness",
      title: "Unchecked value reaches the caller",
      bodyMd: "The validated value is dropped.",
      state: "posted",
      sessionId: "finder-sess-1",
    },
  ],
  verdicts: [
    {
      id: "v1",
      findingId: "f1",
      verdict: "confirmed",
      confidence: "high",
      reasoning: "Reproduced from the diff.",
      sessionId: "verifier-sess-1",
    },
  ],
};

vi.mock("../../hooks/useReviews", () => ({
  useReviews: () => ({ data: { reviews: [review] }, isPending: false, error: null }),
  useReview: () => ({ data: detail, isPending: false, error: null }),
}));
vi.mock("../../hooks/useNow", () => ({ useNow: () => 0 }));

describe("Reviews page", () => {
  it("shows the workflow stage for each review", async () => {
    renderWithProviders(<Reviews />);
    expect(await screen.findByText("cortexapps/engrams")).toBeTruthy();
    // status "verifying" → the "Verifying" stage label.
    expect(screen.getByText("Verifying")).toBeTruthy();
  });

  it("expands a row to reveal findings, verdict, and the finder session link", async () => {
    renderWithProviders(<Reviews />);
    const user = userEvent.setup();
    await user.click(await screen.findByText("cortexapps/engrams"));

    expect(await screen.findByText("Unchecked value reaches the caller")).toBeTruthy();
    expect(screen.getByText(/src\/index\.ts:L10-L12/)).toBeTruthy();
    expect(screen.getByText(/Verifier: confirmed/)).toBeTruthy();
    const finderLink = screen.getByRole("link", { name: /finder session/i });
    expect(finderLink.getAttribute("href")).toContain("/sessions/finder-sess-1");
    const verifierLink = screen.getByRole("link", { name: /verifier session/i });
    expect(verifierLink.getAttribute("href")).toContain("/sessions/verifier-sess-1");
  });
});
