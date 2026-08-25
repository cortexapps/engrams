import { beforeEach, describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen } from "@testing-library/react";

import type { TaskComposerState } from "@/components/composer/TaskComposer";

const navigate = vi.fn();
const mutateAsync = vi.fn();

vi.mock("@tanstack/react-router", async (orig) => ({
  ...(await orig()),
  useNavigate: () => navigate,
  Link: ({ children, ...rest }: { children: React.ReactNode }) => (
    <a {...(rest as object)}>{children}</a>
  ),
}));
vi.mock("@connectrpc/connect-query", () => ({
  useMutation: () => ({ mutateAsync, isPending: false }),
}));
// The real composer pulls profiles/images/harness catalogs; the page's
// contract is (state) => request mapping, so a stub drives it.
vi.mock("@/components/composer/TaskComposer", () => ({
  TaskComposer: ({
    onSubmit,
    initialPrompt,
  }: {
    onSubmit: (s: TaskComposerState) => void;
    initialPrompt?: string;
  }) => (
    <button
      type="button"
      data-testid="composer-stub"
      data-initial={initialPrompt ?? ""}
      onClick={() =>
        onSubmit({
          prompt: initialPrompt ?? "When a PR opens, run the tests",
          profileId: "prof-1",
          harnessOverride: {
            harness: null,
            model: null,
            modelRouter: null,
            effort: null,
            mode: "plan",
          } as never,
          valid: true,
        })
      }
    >
      Start drafting
    </button>
  ),
}));

import { ComposePage } from "./ComposePage";

describe("ComposePage", () => {
  beforeEach(() => {
    navigate.mockClear();
    mutateAsync.mockReset();
    mutateAsync.mockResolvedValue({ automationId: "auto-9", sessionId: "sess-9", created: true });
  });

  it("submits the drafting request and lands in the Builder", async () => {
    render(<ComposePage />);
    fireEvent.click(screen.getByTestId("composer-stub"));
    await vi.waitFor(() => expect(navigate).toHaveBeenCalled());
    expect(mutateAsync).toHaveBeenCalledWith(
      expect.objectContaining({
        prompt: "When a PR opens, run the tests",
        profileId: "prof-1",
        harnessMode: "plan",
        idempotencyKey: expect.any(String),
      }),
    );
    expect(navigate.mock.calls[0]![0]).toMatchObject({
      to: "/settings/automations/$id",
      params: { id: "auto-9" },
      search: { tab: "build" },
    });
  });

  it("a suggestion chip seeds the composer prompt", () => {
    render(<ComposePage />);
    fireEvent.click(screen.getByText(/Slack digest of merged PRs/));
    expect(screen.getByTestId("composer-stub").getAttribute("data-initial")).toContain(
      "Slack digest",
    );
  });

  it("offers the build-by-hand escape hatch", () => {
    render(<ComposePage />);
    expect(screen.getByTestId("build-by-hand")).toBeTruthy();
  });
});
