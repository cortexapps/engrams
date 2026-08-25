// Contract test for the editor shell's save semantics under the built-in
// editing model (ADR 0119 phase 3.3): a built-in saves only the changed
// tunable fields as overrides (SetBlockOverrides); a user automation saves
// the full definition as a new version (SaveVersion).

import { describe, it, expect, vi, beforeEach } from "vitest";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";

import { AutomationEditor } from "./AutomationEditor";

const automationHolder = vi.hoisted(() => ({
  value: undefined as undefined | { automation: unknown },
}));
const searchHolder = vi.hoisted(() => ({ value: {} as { tab?: string } }));
const navigate = vi.hoisted(() => vi.fn());
const saveVersion = vi.hoisted(() => vi.fn().mockResolvedValue({ automation: { id: "a1" } }));
const setOverrides = vi.hoisted(() => vi.fn().mockResolvedValue({ automation: { id: "b1" } }));
const updateMeta = vi.hoisted(() => vi.fn().mockResolvedValue({ automation: { id: "a1" } }));
const duplicate = vi.hoisted(() => vi.fn().mockResolvedValue({ automation: { id: "copy" } }));

// 3.5: the header's DryRun button uses a connect-query mutation; this suite
// renders without a QueryClient, so stub it like every other hook here.
vi.mock("./DraftRail", () => ({
  DraftRail: ({ sessionId }: { sessionId: string }) => (
    <div data-testid="draft-rail">{sessionId}</div>
  ),
}));
vi.mock("@/hooks/useAutomationCode", () => ({
  useDryRun: () => ({ mutate: vi.fn(), isPending: false }),
}));
vi.mock("@/hooks/useAutomationEditor", () => ({
  useEditorAutomation: () => ({ data: automationHolder.value, isPending: false, error: null }),
  useCreateAutomationV2: () => ({ mutateAsync: vi.fn(), isPending: false }),
  useSaveVersionV2: () => ({ mutateAsync: saveVersion, isPending: false }),
  useSetBlockOverrides: () => ({ mutateAsync: setOverrides, isPending: false }),
  useUpdateAutomationMetaV2: () => ({ mutateAsync: updateMeta, isPending: false }),
  useSetAutomationEnabledV2: () => ({ mutate: vi.fn(), isPending: false }),
  useDuplicateAutomation: () => ({ mutateAsync: duplicate, isPending: false }),
  useEventCatalog: () => ({ data: undefined }),
  useActionCatalog: () => ({ data: undefined }),
  useEditorWebhookRegistrations: () => ({ data: { registrations: [] } }),
}));
// 3.4: the shell instantiates the test-with-sample state; the shell tests
// exercise save paths, not rendering, so the hook is inert here.
vi.mock("@/hooks/useAutomationTest", () => ({
  useAutomationTest: () => ({
    isTimed: false,
    samples: [],
    samplesLoading: false,
    sample: { kind: "none" },
    setSample: vi.fn(),
    latest: null,
    tally: null,
    running: false,
    runOnce: vi.fn(),
    runAcrossSamples: vi.fn(),
    variableValues: {},
  }),
}));
vi.mock("@/hooks/useProfiles", () => ({
  useProfiles: () => ({
    data: { profiles: [{ id: "pr_reviewer", name: "PR reviewer", harness: "claude" }] },
  }),
}));
vi.mock("@/hooks/useHarnessCatalog", () => ({ useHarnessCatalog: () => ({ data: [] }) }));
vi.mock("@/hooks/useModelRouters", () => ({
  useModelRouters: () => ({ data: { routers: [] } }),
  useRouterModels: () => ({ data: { models: [] } }),
}));
vi.mock("@tanstack/react-router", async (orig) => ({
  ...(await orig()),
  useNavigate: () => navigate,
  useParams: () => ({ id: "a1" }),
  useSearch: () => searchHolder.value,
  Link: ({ children }: { children: React.ReactNode }) => <a>{children}</a>,
}));
// Panels measure layout; jsdom has none.
vi.mock("@/components/ui/resizable", () => ({
  ResizablePanelGroup: ({ children }: { children: React.ReactNode }) => <div>{children}</div>,
  ResizablePanel: ({ children }: { children: React.ReactNode }) => <div>{children}</div>,
  ResizableHandle: () => null,
}));

const definition = {
  engine: 1,
  trigger: { kind: "manual" },
  blocks: [
    {
      id: "finder",
      type: "create_session",
      tunable: ["promptTemplate"],
      config: { profileId: "pr_reviewer", promptTemplate: "Review it.", role: "finder" },
    },
  ],
  inputsSchema: [],
  settings: { endSessionsOnFinish: false },
};

function automation(kind: "user" | "builtin") {
  return {
    id: kind === "builtin" ? "b1" : "a1",
    name: kind === "builtin" ? "PR review" : "My automation",
    description: "",
    kind,
    builtinKey: kind === "builtin" ? "pr_review" : undefined,
    enabled: true,
    currentVersion: 1,
    inputsJson: "{}",
    blockOverridesJson: "{}",
    archived: false,
    createdAt: "",
    updatedAt: "",
    version: {
      automationId: "x",
      number: 1,
      definitionJson: JSON.stringify(definition),
      createdAt: "",
    },
  };
}

describe("AutomationEditor", () => {
  beforeEach(() => {
    saveVersion.mockClear();
    setOverrides.mockClear();
    updateMeta.mockClear();
    navigate.mockClear();
    searchHolder.value = {};
  });

  it("drafting (Builder v2): rail when bound; agent versions adopt when clean, banner when dirty", async () => {
    const v2 = {
      ...definition,
      blocks: [
        {
          id: "finder",
          type: "create_session",
          tunable: ["promptTemplate"],
          config: { profileId: "pr_reviewer", promptTemplate: "Agent v2.", role: "finder" },
        },
      ],
    };
    const base = {
      ...automation("user"),
      draftSessionId: "draft-sess-1",
    };
    automationHolder.value = { automation: base };
    const view = render(<AutomationEditor mode="edit" />);
    expect(screen.getByTestId("draft-rail").textContent).toBe("draft-sess-1");
    expect(screen.queryByTestId("draft-stale-banner")).toBeNull();

    // CLEAN editor: the agent saves v2 → the editor adopts silently.
    automationHolder.value = {
      automation: {
        ...base,
        currentVersion: 2,
        version: {
          automationId: "x",
          number: 2,
          definitionJson: JSON.stringify(v2),
          createdAt: "",
        },
      },
    };
    view.rerender(<AutomationEditor mode="edit" />);
    fireEvent.click(screen.getByTestId("block-row-finder"));
    expect(screen.getByTestId("field-promptTemplate").querySelector("textarea")!.value).toBe(
      "Agent v2.",
    );
    expect(screen.queryByTestId("draft-stale-banner")).toBeNull();

    // DIRTY editor: a local edit, then the agent saves v3 → banner, no clobber.
    const prompt = screen.getByTestId("field-promptTemplate").querySelector("textarea")!;
    fireEvent.change(prompt, { target: { value: "My local edit." } });
    const v3 = {
      ...v2,
      blocks: [
        { ...v2.blocks[0]!, config: { ...v2.blocks[0]!.config, promptTemplate: "Agent v3." } },
      ],
    };
    automationHolder.value = {
      automation: {
        ...base,
        currentVersion: 3,
        version: {
          automationId: "x",
          number: 3,
          definitionJson: JSON.stringify(v3),
          createdAt: "",
        },
      },
    };
    view.rerender(<AutomationEditor mode="edit" />);
    expect(screen.getByTestId("draft-stale-banner")).toBeTruthy();
    expect(screen.getByTestId("field-promptTemplate").querySelector("textarea")!.value).toBe(
      "My local edit.",
    );

    // Reload adopts the agent's version and clears the banner.
    fireEvent.click(screen.getByRole("button", { name: /reload/i }));
    expect(screen.queryByTestId("draft-stale-banner")).toBeNull();
    expect(screen.getByTestId("field-promptTemplate").querySelector("textarea")!.value).toBe(
      "Agent v3.",
    );
  });

  it("entrypoint bar (D9): switching edits the extra entrypoint; save keeps main untouched", async () => {
    const withEp = {
      ...definition,
      entrypoints: [
        {
          id: "sweep",
          trigger: { kind: "manual" },
          blocks: [
            {
              id: "nudge",
              type: "send_prompt",
              config: {
                session: { template: "s" },
                promptTemplate: "wake up",
                waitFor: { kind: "none" },
              },
            },
          ],
        },
      ],
    };
    automationHolder.value = {
      automation: {
        ...automation("user"),
        version: {
          automationId: "x",
          number: 1,
          definitionJson: JSON.stringify(withEp),
          createdAt: "",
        },
      },
    };
    render(<AutomationEditor mode="edit" />);
    expect(screen.getByTestId("entrypoint-bar")).toBeTruthy();
    // Main shows its own blocks.
    expect(screen.getByTestId("block-row-finder")).toBeTruthy();
    expect(screen.queryByTestId("block-row-nudge")).toBeNull();

    fireEvent.click(screen.getByTestId("entrypoint-sweep"));
    expect(screen.getByTestId("block-row-nudge")).toBeTruthy();
    expect(screen.queryByTestId("block-row-finder")).toBeNull();

    // Edit the extra entrypoint's prompt and save: the payload carries the
    // edit inside entrypoints[0] while main's blocks stay untouched.
    fireEvent.click(screen.getByTestId("block-row-nudge"));
    const prompt = screen.getByTestId("field-promptTemplate").querySelector("textarea")!;
    fireEvent.change(prompt, { target: { value: "review feedback arrived" } });
    fireEvent.click(screen.getByTestId("save-button"));
    await waitFor(() => expect(saveVersion).toHaveBeenCalledTimes(1));
    const sent = JSON.parse(saveVersion.mock.calls[0]![0].definitionJson);
    expect(sent.blocks[0].config.promptTemplate).toBe("Review it.");
    expect(sent.entrypoints[0].blocks[0].config.promptTemplate).toBe("review feedback arrived");
  });

  it("selects the tab from the search param and navigates on tab change", async () => {
    automationHolder.value = { automation: automation("user") };
    searchHolder.value = { tab: "runs" };
    render(<AutomationEditor mode="edit" />);
    expect(screen.getByRole("tab", { name: "Runs" }).getAttribute("aria-selected")).toBe("true");
    fireEvent.mouseDown(screen.getByRole("tab", { name: "Build" }));
    fireEvent.click(screen.getByRole("tab", { name: "Build" }));
    await waitFor(() => expect(navigate).toHaveBeenCalled());
    const call = navigate.mock.calls.at(-1)![0] as {
      search: (prev: Record<string, unknown>) => Record<string, unknown>;
    };
    expect(call.search({})).toEqual({ tab: "build" });
  });

  it("built-in: shows the banner + Duplicate, locks the structure, and saves only changed tunable fields as overrides", async () => {
    automationHolder.value = { automation: automation("builtin") };
    render(<AutomationEditor mode="edit" />);
    expect(screen.getByTestId("builtin-banner")).toBeTruthy();
    expect(screen.getByRole("button", { name: /duplicate/i })).toBeTruthy();
    expect(screen.queryByLabelText("Insert block here")).toBeNull();

    // Edit the tunable prompt; the pinned role stays disabled.
    fireEvent.click(screen.getByTestId("block-row-finder"));
    const prompt = screen.getByTestId("field-promptTemplate").querySelector("textarea")!;
    fireEvent.change(prompt, { target: { value: "Be strict." } });
    expect(screen.getByTestId("field-role").querySelector("input")!.disabled).toBe(true);

    fireEvent.click(screen.getByTestId("save-button"));
    await waitFor(() => expect(setOverrides).toHaveBeenCalledTimes(1));
    expect(JSON.parse(setOverrides.mock.calls[0]![0].overridesJson)).toEqual({
      finder: { promptTemplate: "Be strict." },
    });
    expect(saveVersion).not.toHaveBeenCalled();
    // Name is pinned on a built-in, so no meta update either.
    expect(updateMeta).not.toHaveBeenCalled();

    fireEvent.click(screen.getByRole("button", { name: /duplicate/i }));
    await waitFor(() => expect(duplicate).toHaveBeenCalledWith({ automationId: "b1" }));
    expect(navigate).toHaveBeenCalledWith(expect.objectContaining({ params: { id: "copy" } }));
  });

  it("user automation: saves the full definition as a new version", async () => {
    automationHolder.value = { automation: automation("user") };
    render(<AutomationEditor mode="edit" />);
    expect(screen.queryByTestId("builtin-banner")).toBeNull();
    fireEvent.click(screen.getByTestId("block-row-finder"));
    const prompt = screen.getByTestId("field-promptTemplate").querySelector("textarea")!;
    fireEvent.change(prompt, { target: { value: "Changed." } });
    fireEvent.click(screen.getByTestId("save-button"));
    await waitFor(() => expect(saveVersion).toHaveBeenCalledTimes(1));
    const sent = JSON.parse(saveVersion.mock.calls[0]![0].definitionJson);
    expect(sent.blocks[0].config.promptTemplate).toBe("Changed.");
    expect(sent.blocks[0].id).toBe("finder");
    expect(setOverrides).not.toHaveBeenCalled();
  });
});
