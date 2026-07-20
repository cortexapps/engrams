import { beforeEach, describe, it, expect, vi } from "vitest";
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
  finderSessionId: "finder-sess-1",
  verifierSessionId: "verifier-sess-1",
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
  events: [
    { id: "e1", reviewId: "review-1", kind: "queued" },
    { id: "e2", reviewId: "review-1", kind: "cloning", detail: "finder" },
    { id: "e3", reviewId: "review-1", kind: "reviewing" },
    { id: "e4", reviewId: "review-1", kind: "verifying", detail: "1 candidate finding" },
  ],
};

// A mutable status so a test can render a terminal review (retry is offered)
// without re-mocking; defaults to the active "verifying" the other cases use.
const view = { status: "verifying" };
const retryMutate = vi.fn();

vi.mock("../../hooks/useReviews", () => ({
  useReviews: () => ({
    data: { reviews: [{ ...review, status: view.status }] },
    isPending: false,
    error: null,
  }),
  useReview: () => ({
    data: { ...detail, review: { ...review, status: view.status } },
    isPending: false,
    error: null,
  }),
  useRetryReview: () => ({
    mutate: retryMutate,
    isPending: false,
    isError: false,
    error: null,
  }),
}));
vi.mock("../../hooks/useNow", () => ({ useNow: () => 0 }));

beforeEach(() => {
  view.status = "verifying";
  retryMutate.mockClear();
});

describe("Reviews page", () => {
  it("shows the workflow stage for each review", async () => {
    renderWithProviders(<Reviews />);
    expect(await screen.findByText("cortexapps/engrams")).toBeTruthy();
    // status "verifying" → the "Verifying" stage label.
    expect(screen.getByText("Verifying")).toBeTruthy();
  });

  it("offers a live watch link to the active phase's session without expanding", async () => {
    renderWithProviders(<Reviews />);
    // status "verifying" → the verifier session is the live one.
    const watch = await screen.findByRole("link", { name: /watch live/i });
    expect(watch.getAttribute("href")).toContain("/sessions/verifier-sess-1");
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

  it("offers a retry on a terminal review and dispatches a fresh pass", async () => {
    view.status = "failed";
    renderWithProviders(<Reviews />);
    const user = userEvent.setup();
    await user.click(await screen.findByText("cortexapps/engrams"));

    const retry = await screen.findByRole("button", { name: /retry review/i });
    await user.click(retry);
    expect(retryMutate).toHaveBeenCalledWith({ id: "review-1" });
  });

  it("does not offer a retry while a review is still running", async () => {
    // Default status is the active "verifying" — no retry button.
    renderWithProviders(<Reviews />);
    const user = userEvent.setup();
    await user.click(await screen.findByText("cortexapps/engrams"));

    await screen.findByText("Unchecked value reaches the caller");
    expect(screen.queryByRole("button", { name: /retry review/i })).toBeNull();
  });

  it("renders the review activity log with per-step milestones", async () => {
    renderWithProviders(<Reviews />);
    const user = userEvent.setup();
    await user.click(await screen.findByText("cortexapps/engrams"));

    // The durable log surfaces sub-phase steps the coarse status can't show.
    expect(await screen.findByText("Cloning repository")).toBeTruthy();
    expect(screen.getByText("Reviewing changes")).toBeTruthy();
    expect(screen.getByText("Verifying findings")).toBeTruthy();
    // status "verifying" → the last step ("Verifying findings") reads as live.
    expect(screen.getByText("in progress")).toBeTruthy();
  });
});
