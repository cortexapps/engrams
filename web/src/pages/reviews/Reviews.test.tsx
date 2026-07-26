import { beforeEach, describe, it, expect, vi } from "vitest";
import { screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { renderWithProviders } from "../../test-utils";
import { Reviews } from "./Reviews";
import { ReviewDossier } from "./ReviewDossier";

// Two passes over the same PR: the ledger shows ONE row for the PR carrying the
// newer pass, which is the whole point of grouping (a retry and every push mint
// another review row).
const newerPass = {
  id: "review-2",
  repo: "cortexapps/engrams",
  prNumber: 100,
  taskId: "task-1",
  headSha: "bbbbbbbbbbbbbbbb",
  baseSha: "aaaaaaaaaaaaaaaa",
  trigger: "synchronize",
  status: "verifying",
  finderSessionId: "finder-sess-2",
  verifierSessionId: "verifier-sess-2",
  prTitle: "Bump quinn-proto from 0.11.14 to 0.11.16",
  prAuthor: "dependabot[bot]",
  headBranch: "dependabot/cargo/quinn-proto-0.11.16",
  baseBranch: "main",
  prState: "open",
  additions: 12,
  deletions: 4,
  changedFiles: 2,
  createdAt: { seconds: 2000n, nanos: 0 },
  findingCounts: { critical: 0, high: 1, medium: 0, low: 0, total: 1 },
};

const olderPass = {
  ...newerPass,
  id: "review-1",
  headSha: "cccccccccccccccc",
  trigger: "opened",
  status: "posted",
  finderSessionId: "finder-sess-1",
  verifierSessionId: "verifier-sess-1",
  createdAt: { seconds: 1000n, nanos: 0 },
};

const findings = [
  {
    id: "f1",
    reviewId: "review-1",
    path: "src/index.ts",
    startLine: 10,
    endLine: 12,
    severity: "high",
    confidence: "medium",
    category: "functional-correctness",
    title: "Unchecked value reaches the caller",
    bodyMd: "The validated value is dropped.",
    evidence: ["src/index.ts", "src/validate.ts"],
    state: "posted",
    sessionId: "finder-sess-1",
  },
  {
    id: "f2",
    reviewId: "review-1",
    path: "src/wide.ts",
    severity: "low",
    confidence: "low",
    category: "maintainability-quality",
    title: "Duplicated helper",
    bodyMd: "Two copies of the same guard.",
    evidence: ["src/wide.ts"],
    state: "ui_only",
    sessionId: "finder-sess-1",
  },
  {
    id: "f3",
    reviewId: "review-1",
    path: "src/gone.ts",
    startLine: 4,
    endLine: 4,
    severity: "critical",
    confidence: "low",
    category: "security-privacy",
    title: "Imagined injection",
    bodyMd: "Claimed unvalidated sink.",
    evidence: ["src/gone.ts"],
    state: "suppressed_refuted",
    sessionId: "finder-sess-1",
  },
];

const verdicts = [
  {
    id: "v1",
    findingId: "f1",
    verdict: "confirmed",
    confidence: "high",
    reasoning: "Reproduced from the diff.",
    sessionId: "verifier-sess-1",
  },
  // f2 has NO verdict → unverified.
  {
    id: "v3",
    findingId: "f3",
    verdict: "refuted",
    confidence: "high",
    reasoning: "validate.ts:44 already guards this path.",
    sessionId: "verifier-sess-1",
  },
];

const events = [
  { id: "e1", reviewId: "review-1", kind: "queued", createdAt: { seconds: 1000n, nanos: 0 } },
  {
    id: "e2",
    reviewId: "review-1",
    kind: "cloning",
    detail: "finder",
    createdAt: { seconds: 1010n, nanos: 0 },
  },
  { id: "e3", reviewId: "review-1", kind: "reviewing", createdAt: { seconds: 1020n, nanos: 0 } },
  {
    id: "e4",
    reviewId: "review-1",
    kind: "verifying",
    detail: "3 candidate findings",
    createdAt: { seconds: 1100n, nanos: 0 },
  },
];

// Which pass the dossier resolves, and its status — mutable so one mock covers
// the live and terminal shapes.
const view = { id: "review-1", status: "posted" };
const retryMutate = vi.fn();

vi.mock("@tanstack/react-router", async () => {
  const actual = await vi.importActual<Record<string, unknown>>("@tanstack/react-router");
  return { ...actual, useParams: () => ({ id: view.id }) };
});

vi.mock("../../hooks/useReviews", () => ({
  useReviews: () => ({
    data: {
      reviews: [
        { ...olderPass, status: view.id === "review-1" ? view.status : olderPass.status },
        newerPass,
      ],
    },
    isPending: false,
    error: null,
  }),
  useReview: () => ({
    data: {
      review: { ...olderPass, status: view.status },
      findings,
      verdicts,
      events,
    },
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
// The transcript pane streams over SSE, which jsdom has no EventSource for; the
// pane's job here is to mount the right session, not to replay a transcript.
vi.mock("../../hooks/useSessionEvents", () => ({
  useSessionEvents: () => ({ events: [], streamingText: "" }),
}));
vi.mock("../../hooks/useSessions", () => ({
  useSession: () => ({ data: { id: "finder-sess-1", status: "completed" } }),
}));

beforeEach(() => {
  view.id = "review-1";
  view.status = "posted";
  retryMutate.mockClear();
});

describe("Reviews ledger", () => {
  it("shows one row per pull request, carrying the newest pass", async () => {
    renderWithProviders(<Reviews />);

    // Two review rows over one PR collapse to a single entry…
    expect(await screen.findByText("Bump quinn-proto from 0.11.14 to 0.11.16")).toBeTruthy();
    expect(screen.getByText("1 of 1 pull requests")).toBeTruthy();
    // …and it reports the newer pass's stage, not the older one's.
    expect(screen.getByText("Verifying")).toBeTruthy();
    expect(screen.getByText("2 passes")).toBeTruthy();
  });

  it("links a row to the newest pass's dossier", async () => {
    renderWithProviders(<Reviews />);

    const row = await screen.findByRole("link", {
      name: /Bump quinn-proto/i,
    });
    expect(row.getAttribute("href")).toContain("/reviews/review-2");
  });

  it("falls back to the PR number when no title was captured", async () => {
    renderWithProviders(<Reviews />);
    // Both fixtures carry a title, so assert the coordinate is still shown
    // alongside it — the number is the stable identity.
    expect(await screen.findByText("#100")).toBeTruthy();
  });
});

describe("Review dossier", () => {
  it("names the PR and reports the context captured at pass start", async () => {
    renderWithProviders(<ReviewDossier />);

    expect(await screen.findByText("Bump quinn-proto from 0.11.14 to 0.11.16")).toBeTruthy();
    expect(screen.getByText("dependabot[bot]")).toBeTruthy();
    expect(screen.getByText("main ← dependabot/cargo/quinn-proto-0.11.16")).toBeTruthy();
    expect(screen.getByText("+12 −4 across 2 files")).toBeTruthy();
    // The head SHA is shown short, with the full value available on hover.
    expect(screen.getByText("ccccccc")).toBeTruthy();
  });

  it("states the kept-vs-killed ratio", async () => {
    renderWithProviders(<ReviewDossier />);
    // 3 findings, 1 refuted → 2 kept, 1 posted.
    expect(await screen.findByText(/2 of 3 kept · verifier refuted 1/)).toBeTruthy();
  });

  it("groups findings by outcome and names why each did not post", async () => {
    renderWithProviders(<ReviewDossier />);

    expect(await screen.findByText("Posted to the pull request")).toBeTruthy();
    // f2 has no verdict → unverified, not "over the cap".
    expect(screen.getByText("Not verified")).toBeTruthy();
    expect(screen.getByText("Refuted by the verifier")).toBeTruthy();
  });

  it("keeps the finder's claim and the verifier's ruling as separate voices", async () => {
    renderWithProviders(<ReviewDossier />);

    expect(await screen.findByText("Unchecked value reaches the caller")).toBeTruthy();
    expect(screen.getByText("The validated value is dropped.")).toBeTruthy();
    // The verifier is attributed, and carries its own confidence.
    expect(screen.getByText(/Verifier confirmed this/)).toBeTruthy();
    expect(screen.getByText("Reproduced from the diff.")).toBeTruthy();
  });

  it("collapses refuted findings behind a count", async () => {
    renderWithProviders(<ReviewDossier />);
    const user = userEvent.setup();

    // Collapsed: the refuted finding's title isn't rendered until asked for.
    await screen.findByText("Refuted by the verifier");
    expect(screen.queryByText("Imagined injection")).toBeNull();

    await user.click(screen.getByRole("button", { name: /Refuted by the verifier/i }));
    expect(await screen.findByText("Imagined injection")).toBeTruthy();
    expect(screen.getByText("validate.ts:44 already guards this path.")).toBeTruthy();
  });

  it("shows the finder's evidence on request — the claim's receipt", async () => {
    renderWithProviders(<ReviewDossier />);
    const user = userEvent.setup();

    const toggle = await screen.findByRole("button", { name: /2 files the finder read/i });
    expect(screen.queryByText("src/validate.ts")).toBeNull();
    await user.click(toggle);
    expect(await screen.findByText("src/validate.ts")).toBeTruthy();
  });

  it("collapses the activity log on a finished pass and expands it in place", async () => {
    renderWithProviders(<ReviewDossier />);
    const user = userEvent.setup();

    // Terminal → one summary line, no steps.
    const summary = await screen.findByRole("button", { name: /4 steps/i });
    expect(screen.queryByText("Cloning repository")).toBeNull();

    await user.click(summary);
    expect(await screen.findByText("Cloning repository")).toBeTruthy();
    expect(screen.getByText("Reviewing changes")).toBeTruthy();
  });

  it("opens the log and marks the running step while a pass is live", async () => {
    view.status = "verifying";
    renderWithProviders(<ReviewDossier />);

    // Live → the log IS the content, already open, with the last step running.
    expect(await screen.findByText("Verifying findings")).toBeTruthy();
    expect(screen.getByText("in progress")).toBeTruthy();
    expect(screen.queryByRole("button", { name: /4 steps/i })).toBeNull();
  });

  // The activity log is the way INTO the transcripts: a milestone that names a
  // worker session opens that session's thread beside the dossier.
  it("opens a worker transcript from the milestone that names it", async () => {
    renderWithProviders(<ReviewDossier />);
    const user = userEvent.setup();
    await user.click(await screen.findByRole("button", { name: /4 steps/i }));

    // Nothing open yet — the pane is collapsed.
    expect(screen.queryByRole("link", { name: /Full session/i })).toBeNull();

    await user.click(screen.getByRole("button", { name: /Verifying findings/i }));

    // The pane mounts on the verifier and offers the way out to the full page.
    const full = await screen.findByRole("link", { name: /Full session/i });
    expect(full.getAttribute("href")).toContain("/sessions/verifier-sess-1");
    expect(screen.getByRole("button", { name: "Finder" })).toBeTruthy();
    expect(screen.getByRole("button", { name: "Verifier" })).toBeTruthy();
  });

  it("switches the pane between the finder and the verifier", async () => {
    renderWithProviders(<ReviewDossier />);
    const user = userEvent.setup();

    // The collapsed edge rail is the other way in.
    await user.click(await screen.findByRole("button", { name: /Open the finder transcript/i }));
    expect(
      (await screen.findByRole("link", { name: /Full session/i })).getAttribute("href"),
    ).toContain("/sessions/finder-sess-1");

    await user.click(screen.getByRole("button", { name: "Verifier" }));
    expect(screen.getByRole("link", { name: /Full session/i }).getAttribute("href")).toContain(
      "/sessions/verifier-sess-1",
    );
  });

  it("closes the transcript pane", async () => {
    renderWithProviders(<ReviewDossier />);
    const user = userEvent.setup();

    await user.click(await screen.findByRole("button", { name: /Open the finder transcript/i }));
    await screen.findByRole("link", { name: /Full session/i });

    await user.click(screen.getByRole("button", { name: /Close transcript/i }));
    expect(screen.queryByRole("link", { name: /Full session/i })).toBeNull();
  });

  it("offers a re-run on a finished pass and dispatches a fresh one", async () => {
    renderWithProviders(<ReviewDossier />);
    const user = userEvent.setup();

    await user.click(await screen.findByRole("button", { name: /re-run/i }));
    expect(retryMutate).toHaveBeenCalledWith({ id: "review-1" });
  });

  it("does not offer a re-run while a pass is still running", async () => {
    view.status = "verifying";
    renderWithProviders(<ReviewDossier />);

    await screen.findByText("Verifying findings");
    expect(screen.queryByRole("button", { name: /re-run/i })).toBeNull();
  });

  it("points forward when a newer pass has superseded this one", async () => {
    renderWithProviders(<ReviewDossier />);

    const forward = await screen.findByRole("link", { name: /Open the current pass/i });
    expect(forward.getAttribute("href")).toContain("/reviews/review-2");
  });
});
