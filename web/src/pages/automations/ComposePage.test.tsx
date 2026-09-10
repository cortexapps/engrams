import { fireEvent, render, screen } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";

import type { TaskComposerState } from "@/components/composer/TaskComposer";

const navigate = vi.hoisted(() => vi.fn());
const mutateAsync = vi.hoisted(() => vi.fn());
const duplicate = vi.hoisted(() => vi.fn());
const editorState = vi.hoisted(() => ({ automation: undefined as unknown }));
const builtinState = vi.hoisted(() => ({ automation: undefined as unknown }));

vi.mock("@tanstack/react-router", async (orig) => ({
  ...(await orig()),
  useNavigate: () => navigate,
  Link: ({
    children,
    to,
    params: _params,
    search: _search,
    ...rest
  }: {
    children: React.ReactNode;
    to: string;
    params?: unknown;
    search?: unknown;
  }) => (
    <a href={to} {...rest}>
      {children}
    </a>
  ),
}));
vi.mock("@connectrpc/connect-query", () => ({
  useMutation: () => ({ mutateAsync, isPending: false }),
}));
vi.mock("@/hooks/useAutomationEditor", () => ({
  useEditorAutomation: () =>
    editorState.automation ? { data: { automation: editorState.automation } } : { data: undefined },
}));
vi.mock("@/hooks/useAutomations", () => ({
  useBuiltinAutomation: () =>
    builtinState.automation
      ? { data: { automation: builtinState.automation } }
      : { data: undefined },
  useDuplicateAutomation: () => ({ mutateAsync: duplicate, isPending: false }),
}));
vi.mock("@/hooks/useSessionEvents", () => ({
  useSessionEvents: () => ({
    events: [{ idx: 1 }],
    streamingText: "Drafting",
    hasMore: false,
    loadingOlder: false,
    loadOlder: vi.fn(),
    oldestIdx: 1,
  }),
}));
vi.mock("@/components/session-thread/SessionThread", () => ({
  SessionThread: ({ sessionId }: { sessionId: string }) => (
    <div data-testid="session-thread-stub" data-session-id={sessionId} />
  ),
}));
// The real composer loads profiles, images, and harness catalogs. This stub
// keeps the page test on request mapping and draft lifecycle behavior.
vi.mock("@/components/composer/TaskComposer", () => ({
  TaskComposer: ({
    onSubmit,
    initialPrompt,
    disabled,
    pending,
    submitLabel,
  }: {
    onSubmit: (state: TaskComposerState) => void;
    initialPrompt?: string;
    disabled?: boolean;
    pending: boolean;
    submitLabel: string;
  }) => (
    <button
      type="button"
      data-testid="composer-stub"
      data-initial={initialPrompt ?? ""}
      data-disabled={String(Boolean(disabled))}
      data-pending={String(pending)}
      disabled={disabled}
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
      {submitLabel}
    </button>
  ),
}));

import { ComposePage } from "./ComposePage";

describe("ComposePage", () => {
  beforeEach(() => {
    navigate.mockReset();
    mutateAsync.mockReset();
    duplicate.mockReset();
    editorState.automation = undefined;
    builtinState.automation = undefined;
    mutateAsync.mockResolvedValue({ automationId: "auto-9", sessionId: "sess-9", created: true });
    duplicate.mockResolvedValue({ automation: { id: "copy-1" } });
  });

  it("keeps drafting on the page and opens the Builder only after the first version lands", async () => {
    const view = render(<ComposePage />);
    expect(screen.getByTestId("composer-stub").textContent).toBe("Draft it");
    fireEvent.click(screen.getByTestId("composer-stub"));

    await vi.waitFor(() => expect(mutateAsync).toHaveBeenCalled());
    expect(mutateAsync).toHaveBeenCalledWith(
      expect.objectContaining({
        prompt: "When a PR opens, run the tests",
        profileId: "prof-1",
        harnessMode: "plan",
        idempotencyKey: expect.any(String),
      }),
    );
    expect(navigate).not.toHaveBeenCalled();

    editorState.automation = { draftSessionId: "s1", currentVersion: 0 };
    view.rerender(<ComposePage />);
    expect(await screen.findByTestId("drafting-thread")).toBeTruthy();
    expect(screen.getByTestId("session-thread-stub").getAttribute("data-session-id")).toBe("s1");
    expect(screen.getByTestId("composer-stub").getAttribute("data-disabled")).toBe("true");
    expect(navigate).not.toHaveBeenCalled();

    editorState.automation = { draftSessionId: "s1", currentVersion: 1 };
    view.rerender(<ComposePage />);
    await vi.waitFor(() => expect(navigate).toHaveBeenCalledTimes(1));
    expect(navigate.mock.calls[0]![0]).toMatchObject({
      to: "/automations/$id",
      params: { id: "auto-9" },
      search: { tab: "build" },
    });
  });

  it("a starting point seeds the composer prompt", () => {
    render(<ComposePage />);
    fireEvent.click(screen.getByText(/Slack digest of merged PRs/));
    expect(screen.getByTestId("composer-stub").getAttribute("data-initial")).toContain(
      "Slack digest",
    );
  });

  it("offers the build-by-hand peer action", () => {
    render(<ComposePage />);
    expect(screen.getByTestId("build-by-hand").textContent).toBe("Build by hand");
  });

  it("duplicates the built-in from its conditional starting point", async () => {
    const view = render(<ComposePage />);
    expect(screen.queryByText(/Duplicate the built-in PR review/)).toBeNull();

    builtinState.automation = { id: "builtin-pr" };
    view.rerender(<ComposePage />);
    fireEvent.click(screen.getByText("Duplicate the built-in PR review and change the repos"));

    await vi.waitFor(() => expect(duplicate).toHaveBeenCalledWith({ automationId: "builtin-pr" }));
    await vi.waitFor(() =>
      expect(navigate).toHaveBeenCalledWith({
        to: "/automations/$id",
        params: { id: "copy-1" },
        search: { tab: "build" },
      }),
    );
  });
});
