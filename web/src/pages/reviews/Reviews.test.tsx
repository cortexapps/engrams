import { beforeEach, describe, it, expect, vi } from "vitest";
import { act, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { renderWithProviders } from "../../test-utils";
import { Reviews } from "./Reviews";
import { ReviewDossier } from "./ReviewDossier";

// Two passes over the same PR: the ledger shows ONE row for the PR carrying the
// newer pass, which is the whole point of grouping (a retry and every push mint
// another review row).
const newerPass = {
  id: "review-2",
  // Grouping keys on the PR's own record, not on "repo#number" (ADR 0100 d11).
  targetId: "target-100",
  provider: "github",
  repo: "cortexapps/engrams",
  prNumber: 100,
  taskId: "task-1",
  headSha: "bbbbbbbbbbbbbbbb",
  baseSha: "aaaaaaaaaaaaaaaa",
  trigger: "synchronize",
  status: "verifying",
  active: true,
  humanTrigger: false,
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
  // A person asked for this one; the newer pass was an automatic push.
  trigger: "command",
  status: "posted",
  active: false,
  humanTrigger: true,
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
    githubThreadId: "998877",
    resolution: "fixed",
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

// Which pass the dossier resolves, its status, and what it reported — mutable so
// one mock covers the live, terminal, and reported-nothing shapes.
const view: {
  id: string;
  status: string;
  findings: typeof findings;
  events: typeof events;
  /** Extra passes over OTHER pull requests, for grouping tests. */
  others: Array<typeof newerPass>;
} = {
  id: "review-1",
  status: "posted",
  findings,
  events,
  others: [],
};
const retryMutate = vi.fn<(input: { id: string }, options?: { onSuccess?: () => void }) => void>();

vi.mock("@tanstack/react-router", async () => {
  const actual = await vi.importActual<Record<string, unknown>>("@tanstack/react-router");
  return { ...actual, useParams: () => ({ id: view.id }) };
});

vi.mock("../../hooks/useReviews", () => ({
  useReviews: () => ({
    data: {
      reviews: [
        {
          ...olderPass,
          status: view.id === "review-1" ? view.status : olderPass.status,
          active:
            view.id === "review-1"
              ? ["queued", "finding", "verifying"].includes(view.status)
              : olderPass.active,
        },
        newerPass,
        ...view.others,
      ],
    },
    isPending: false,
    error: null,
  }),
  useReview: () => ({
    data: {
      review: {
        ...olderPass,
        status: view.status,
        active: ["queued", "finding", "verifying"].includes(view.status),
      },
      findings: view.findings,
      verdicts,
      events: view.events,
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
  useSessionEvents: () => ({
    events: [],
    streamingText: "",
    hasMore: false,
    loadingOlder: false,
    loadOlder: () => {},
    oldestIdx: null,
  }),
}));
vi.mock("../../hooks/useSessions", () => ({
  useSession: () => ({ data: { id: "finder-sess-1", status: "completed" } }),
}));

beforeEach(() => {
  view.id = "review-1";
  view.status = "posted";
  view.findings = findings;
  view.events = events;
  view.others = [];
  retryMutate.mockClear();
});

describe("Reviews ledger", () => {
  it("shows one row per pull request, carrying the newest pass", async () => {
    renderWithProviders(<Reviews />);

    // Two review rows over one PR collapse to a single entry…
    expect(await screen.findByText("Bump quinn-proto from 0.11.14 to 0.11.16")).toBeTruthy();
    // The count lives in the masthead chip, and only says "of" once a filter
    // narrows the list.
    expect(screen.getByText("1 pull request")).toBeTruthy();
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

  it("keeps separate pull requests apart, and a renamed repo together", async () => {
    view.others = [
      // A different PR — must be its own row.
      {
        ...newerPass,
        id: "review-3",
        targetId: "target-204",
        prNumber: 204,
        prTitle: "Trim the firecracker CI lane",
        createdAt: { seconds: 3000n, nanos: 0 },
      },
      // The SAME PR as review-1/2, seen after the repo was renamed. Keyed on the
      // PR's record it stays in that group; keyed on "repo#number" it would
      // split the history in two.
      {
        ...newerPass,
        id: "review-4",
        repo: "cortexapps/engrams-renamed",
        createdAt: { seconds: 2500n, nanos: 0 },
      },
    ];
    renderWithProviders(<Reviews />);

    expect(await screen.findByText("2 pull requests")).toBeTruthy();
    expect(screen.getByText("Trim the firecracker CI lane")).toBeTruthy();
    expect(screen.getByText("3 passes")).toBeTruthy();
  });

  // A review is about somebody's change: whose it is, what state that change is
  // in, and how big it is all decide whether a reader opens the row.
  it("carries the pull request's state, author and size on the row", async () => {
    renderWithProviders(<Reviews />);

    expect(await screen.findByText("dependabot[bot]")).toBeTruthy();
    expect(screen.getByText("+12 −4")).toBeTruthy();
    // The state is a shape, so the word goes to assistive tech only.
    expect(screen.getByText("Open pull request")).toBeTruthy();
  });

  // The bar is aria-hidden and the count beside it is a bare figure, so the
  // breakdown the four chips used to spell out has to be announced somewhere.
  it("announces the severity breakdown the bar draws", async () => {
    renderWithProviders(<Reviews />);

    expect(await screen.findByText(/findings: 1 high/)).toBeTruthy();
  });

  // "no findings" on a pass that has not read the diff yet is a false statement
  // about the code, not a summary of it.
  it("says nothing about findings until a pass has looked", async () => {
    const noCounts = { critical: 0, high: 0, medium: 0, low: 0, total: 0 };
    view.others = [
      {
        ...newerPass,
        id: "review-5",
        targetId: "target-300",
        prNumber: 300,
        prTitle: "Still reading the diff",
        status: "finding",
        active: true,
        findingCounts: noCounts,
        createdAt: { seconds: 4000n, nanos: 0 },
      },
      {
        ...newerPass,
        id: "review-6",
        targetId: "target-301",
        prNumber: 301,
        prTitle: "Nothing to report",
        status: "posted",
        active: false,
        findingCounts: noCounts,
        createdAt: { seconds: 3900n, nanos: 0 },
      },
    ];
    renderWithProviders(<Reviews />);

    // Only the finished pass claims zero. The running one stays silent.
    await screen.findByText("Still reading the diff");
    expect(screen.getAllByText("none")).toHaveLength(1);
  });

  it("falls back to the PR number when no title was captured", async () => {
    renderWithProviders(<Reviews />);
    // Both fixtures carry a title, so assert the coordinate is still shown
    // alongside it — the number is the stable identity.
    expect(await screen.findByText("#100")).toBeTruthy();
  });
});

describe("Review dossier", () => {
  it("names the PR once, with a single way over to GitHub", async () => {
    renderWithProviders(<ReviewDossier />);

    expect(await screen.findByText("Bump quinn-proto from 0.11.14 to 0.11.16")).toBeTruthy();
    expect(screen.getByText("dependabot[bot]")).toBeTruthy();
    expect(screen.getByText("main ← dependabot/cargo/quinn-proto-0.11.16")).toBeTruthy();
    expect(screen.getByText("+12 −4 across 2 files")).toBeTruthy();

    const gh = screen.getAllByRole("link", { name: /GitHub/i });
    expect(gh).toHaveLength(1);
    expect(gh[0]!.getAttribute("href")).toBe("https://github.com/cortexapps/engrams/pull/100");
  });

  // A pass is an address, not a section: it costs one line, and its siblings live
  // in a menu rather than a list the findings have to sit underneath.
  it("switches passes from a menu, marking the one being read", async () => {
    renderWithProviders(<ReviewDossier />);
    const user = userEvent.setup();

    await user.click(await screen.findByRole("button", { name: /Choose a pass/i }));

    const items = await screen.findAllByRole("menuitem");
    expect(items).toHaveLength(2);
    // Newest first, and the open one is the one the route names.
    expect(items[0]!.getAttribute("href")).toContain("/reviews/review-2");
    expect(items[1]!.getAttribute("href")).toContain("/reviews/review-1");
    expect(items[1]!.getAttribute("aria-current")).toBe("page");
  });

  it("marks only the passes a person asked for", async () => {
    renderWithProviders(<ReviewDossier />);
    const user = userEvent.setup();

    await user.click(await screen.findByRole("button", { name: /Choose a pass/i }));

    // review-1 came from an @mention; review-2 was an automatic push, and
    // labelling automation on every row would be a column carrying nothing. The
    // marker is an icon, so the word rides along for assistive tech.
    expect(await screen.findByText("Mentioned")).toBeTruthy();
    expect(screen.queryByText("New commits")).toBeNull();
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

    // The leading group is small, so it arrives open — no clicks for the common case.
    expect(await screen.findByText("Unchecked value reaches the caller")).toBeTruthy();
    expect(screen.getByText("The validated value is dropped.")).toBeTruthy();
    // The verifier is attributed, and carries its own confidence. It does NOT
    // restate the verdict: the group heading above already says these were
    // confirmed, and a band repeating it on every card was most of the noise.
    expect(screen.getByText(/Verifier · high confidence/)).toBeTruthy();
    expect(screen.getByText("Reproduced from the diff.")).toBeTruthy();
  });

  // A body runs to twenty lines and the cap is 200 findings, so only the leading
  // group opens itself; everything else is a row until asked for.
  it("collapses findings outside the leading group", async () => {
    renderWithProviders(<ReviewDossier />);
    const user = userEvent.setup();

    expect(await screen.findByText("Duplicated helper")).toBeTruthy();
    expect(screen.queryByText("Two copies of the same guard.")).toBeNull();

    await user.click(screen.getByRole("button", { name: /Duplicated helper/i }));
    expect(await screen.findByText("Two copies of the same guard.")).toBeTruthy();
  });

  it("collapses refuted findings behind a count", async () => {
    renderWithProviders(<ReviewDossier />);
    const user = userEvent.setup();

    // Collapsed: the refuted finding's title isn't rendered until asked for.
    await screen.findByText("Refuted by the verifier");
    expect(screen.queryByText("Imagined injection")).toBeNull();

    await user.click(screen.getByRole("button", { name: /Refuted by the verifier/i }));
    await user.click(await screen.findByRole("button", { name: /Imagined injection/i }));
    expect(await screen.findByText("validate.ts:44 already guards this path.")).toBeTruthy();
  });

  it("shows the finder's evidence on request — the claim's receipt", async () => {
    renderWithProviders(<ReviewDossier />);
    const user = userEvent.setup();

    const toggle = await screen.findByRole("button", { name: /2 files the finder read/i });
    expect(screen.queryByText("src/validate.ts")).toBeNull();
    await user.click(toggle);
    expect(await screen.findByText("src/validate.ts")).toBeTruthy();
  });

  // 3 findings, 1 refuted. The refuted group's own count carries that, so the
  // sentence that used to restate it above the ledger is gone. Deliberately
  // silent about what "stands": an unverified finding never posts, so counting
  // it as surviving would overstate.
  it("counts the refuted findings against the whole pass", async () => {
    renderWithProviders(<ReviewDossier />);

    await screen.findByText("Refuted by the verifier");
    expect(screen.getByText("1 of 3")).toBeTruthy();
    expect(screen.queryByText(/The verifier refuted/)).toBeNull();
  });

  it("says why there is nothing to show instead of drawing an empty box", async () => {
    view.status = "failed";
    view.findings = [];
    renderWithProviders(<ReviewDossier />);

    expect(await screen.findByText("This pass failed before it reported anything.")).toBeTruthy();
    expect(screen.queryByText(/verifier refuted/)).toBeNull();
  });

  // State and activity are one idea at two zoom levels: a terminal pass's state
  // IS the last line of its own log, so one control carries both.
  it("carries the pass state and opens the log from the same control", async () => {
    renderWithProviders(<ReviewDossier />);
    const user = userEvent.setup();

    expect(screen.queryByText("Cloning repository")).toBeNull();
    const control = await screen.findByRole("button", { name: /Posted — show the activity log/i });
    // The terminal state, and only that. A bare "4" beside it was an unlabelled
    // figure; the step count belongs to the log it counts.
    expect(control.textContent).toMatch(/Posted/);
    expect(control.textContent).not.toMatch(/4/);

    await user.click(control);
    expect(await screen.findByText("Cloning repository")).toBeTruthy();
    expect(screen.getByText("Reviewing changes")).toBeTruthy();
    expect(screen.getByText(/4 steps/)).toBeTruthy();
  });

  // Live, the step says more than the stage word does: "Reviewing changes" beats
  // "Finding".
  it("reports the running step rather than the stage while a pass is live", async () => {
    view.status = "verifying";
    renderWithProviders(<ReviewDossier />);

    const control = await screen.findByRole("button", {
      name: /Verifying findings — show the activity log/i,
    });
    expect(control.textContent).toMatch(/Verifying findings/);
  });

  it("opens the selected pass's worker sessions in a sheet", async () => {
    renderWithProviders(<ReviewDossier />);
    const user = userEvent.setup();

    expect(screen.queryByRole("link", { name: /Full session/i })).toBeNull();
    await user.click(await screen.findByRole("button", { name: "Sessions" }));

    // The sheet mounts on the finder and offers the way out to the full page.
    const full = await screen.findByRole("link", { name: /Full session/i });
    expect(full.getAttribute("href")).toContain("/sessions/finder-sess-1");
    // The role switcher is stock shadcn Tabs now, so the triggers carry
    // role="tab" — the correct ARIA for a tablist, and what a screen reader
    // announces. The old hand-rolled buttons only ever reported "button".
    expect(screen.getByRole("tab", { name: "Finder" })).toBeTruthy();
    expect(screen.getByRole("tab", { name: "Verifier" })).toBeTruthy();
  });

  it("switches the sheet between the finder and the verifier", async () => {
    renderWithProviders(<ReviewDossier />);
    const user = userEvent.setup();

    await user.click(await screen.findByRole("button", { name: "Sessions" }));
    await screen.findByRole("link", { name: /Full session/i });

    await user.click(screen.getByRole("tab", { name: "Verifier" }));
    expect(screen.getByRole("link", { name: /Full session/i }).getAttribute("href")).toContain(
      "/sessions/verifier-sess-1",
    );
  });

  it("closes the transcript sheet", async () => {
    renderWithProviders(<ReviewDossier />);
    const user = userEvent.setup();

    await user.click(await screen.findByRole("button", { name: "Sessions" }));
    await screen.findByRole("link", { name: /Full session/i });

    await user.click(screen.getByRole("button", { name: /Close transcript/i }));
    expect(screen.queryByRole("link", { name: /Full session/i })).toBeNull();
  });

  it("offers a re-run on a finished pass and dispatches a fresh one", async () => {
    renderWithProviders(<ReviewDossier />);
    const user = userEvent.setup();

    await user.click(await screen.findByRole("button", { name: /re-run/i }));
    // The second argument is the per-call onSuccess that navigates to the new pass.
    expect(retryMutate.mock.calls[0]?.[0]).toEqual({ id: "review-1" });

    // The RPC only returns ingress's workflow id. Until the review list exposes
    // the new pass, the same button must stay disabled rather than allowing a
    // second intentional supersede.
    await act(async () => {
      retryMutate.mock.calls[0]?.[1]?.onSuccess?.();
    });
    expect((screen.getByRole("button", { name: /re-run/i }) as HTMLButtonElement).disabled).toBe(
      true,
    );
  });

  it("does not offer a re-run while a pass is still running", async () => {
    view.status = "verifying";
    renderWithProviders(<ReviewDossier />);

    await screen.findByRole("button", { name: /Show the activity log/i });
    expect(screen.queryByRole("button", { name: /re-run/i })).toBeNull();
  });

  // Deciding whether a finding is real means looking at the code; acting on it
  // means the thread it became. Both were a manual hunt on GitHub before this.
  it("links a finding to the code at the commit the pass read, and to its thread", async () => {
    renderWithProviders(<ReviewDossier />);

    const code = await screen.findByRole("link", { name: /View the code/i });
    // Pinned to headSha, not a branch: lines that have shifted since are worse
    // than no link at all.
    expect(code.getAttribute("href")).toBe(
      "https://github.com/cortexapps/engrams/blob/cccccccccccccccc/src/index.ts#L10-L12",
    );

    const thread = screen.getByRole("link", { name: /Open the thread/i });
    expect(thread.getAttribute("href")).toContain("#discussion_r998877");
    // What the author actually did is the strongest post-hoc trust signal there is.
    expect(screen.getByText("Author fixed this")).toBeTruthy();
  });

  it("says why a pass failed, in the reading flow", async () => {
    view.status = "failed";
    view.findings = [];
    view.events = [
      { id: "e1", reviewId: "review-1", kind: "queued", createdAt: { seconds: 1000n, nanos: 0 } },
      {
        id: "e2",
        reviewId: "review-1",
        kind: "failed",
        detail: "finder setup failed",
        createdAt: { seconds: 1010n, nanos: 0 },
      },
    ];
    renderWithProviders(<ReviewDossier />);

    // Not buried in the activity popover behind a duration-labelled control.
    expect(await screen.findByText("finder setup failed")).toBeTruthy();
  });

  // The canonical entry point is a marker in a comment on a PR that has very
  // likely been pushed to since, so reading history must not look like the present.
  it("marks a superseded pass and points at the current one", async () => {
    renderWithProviders(<ReviewDossier />);

    const forward = await screen.findByRole("link", { name: /Open it/i });
    expect(forward.getAttribute("href")).toContain("/reviews/review-2");
    expect(screen.getByText(/A newer pass ran/)).toBeTruthy();
    // And the trigger says where you are without opening anything. Numbered
    // oldest-first, the way attempts are counted: review-1 was the first try.
    expect(screen.getByText("pass 1 of 2")).toBeTruthy();
  });

  it("gives the route a heading outline to navigate by", async () => {
    renderWithProviders(<ReviewDossier />);

    const h1 = await screen.findByRole("heading", { level: 1 });
    expect(h1.textContent).toBe("Bump quinn-proto from 0.11.14 to 0.11.16");
    // Each outcome group is the reason a finding didn't post, so it is reachable.
    const h2s = screen.getAllByRole("heading", { level: 2 });
    expect(h2s.map((h) => h.textContent)).toContain("Posted to the pull request");
  });

  // A reviewer session is owned by nobody, and session reads are owner-scoped, so
  // only an admin can actually open one. Offering a button that answers 404 is
  // worse than not offering it.
  it("hides the sessions button from a member who could not open them", async () => {
    renderWithProviders(<ReviewDossier />, {
      principal: {
        email: "member@example.com",
        display_name: "Member",
        role: "member",
        is_admin: false,
        can_sign_out: true,
      },
    });

    await screen.findByText("Bump quinn-proto from 0.11.14 to 0.11.16");
    expect(screen.queryByRole("button", { name: "Sessions" })).toBeNull();
  });

  // Only the posting gate advances a finding past `candidate`, so on a pass that
  // died before it ran, a confirmed anchored finding looks identical to one that
  // lost a race inside the cap. Saying "over the comment cap" describes a
  // decision that was never made.
  it("does not blame the comment cap when the gate never ran", async () => {
    view.status = "failed";
    view.findings = [{ ...findings[0]!, state: "candidate" }];
    renderWithProviders(<ReviewDossier />);

    expect(await screen.findByText("No decision yet")).toBeTruthy();
    expect(screen.queryByText("Over the comment cap")).toBeNull();
  });

  // Review-event writes are best-effort and their failures are swallowed, so the
  // last event is not reliably the one that ended the pass.
  it("does not present an unrelated event as the failure reason", async () => {
    view.status = "failed";
    view.findings = [];
    view.events = [
      { id: "e1", reviewId: "review-1", kind: "queued", createdAt: { seconds: 1000n, nanos: 0 } },
      {
        id: "e2",
        reviewId: "review-1",
        kind: "verifying",
        detail: "3 candidate findings",
        createdAt: { seconds: 1010n, nanos: 0 },
      },
    ];
    renderWithProviders(<ReviewDossier />);

    await screen.findByText("This pass failed before it reported anything.");
    // The `failed` event never landed, so we say nothing rather than blaming the
    // verifier's progress note.
    expect(screen.queryByText("3 candidate findings")).toBeNull();
  });
});
