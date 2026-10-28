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
const searchHolder = vi.hoisted(() => ({ value: {} as { tab?: string; run?: string } }));
const navigate = vi.hoisted(() => vi.fn());
const saveVersion = vi.hoisted(() => vi.fn().mockResolvedValue({ automation: { id: "a1" } }));
const setOverrides = vi.hoisted(() => vi.fn().mockResolvedValue({ automation: { id: "b1" } }));
const updateMeta = vi.hoisted(() => vi.fn().mockResolvedValue({ automation: { id: "a1" } }));
const duplicate = vi.hoisted(() => vi.fn().mockResolvedValue({ automation: { id: "copy" } }));
const createAutomation = vi.hoisted(() =>
  vi.fn().mockResolvedValue({ automation: { id: "new-1" } }),
);
/** What the editor last told the router's blocker. */
const blockerHolder = vi.hoisted(() => ({
  opts: undefined as { enableBeforeUnload?: () => boolean } | undefined,
}));

// 3.5: the header's DryRun button uses a connect-query mutation; this suite
// renders without a QueryClient, so stub it like every other hook here.
const runHolder = vi.hoisted(() => ({ value: undefined as unknown }));
vi.mock("@/hooks/useAutomationRuns", () => ({
  useRunList: () => ({ data: undefined, isPending: false, error: null }),
  useRun: (id: string | undefined) => ({
    data: id ? runHolder.value : undefined,
    isPending: false,
    error: null,
  }),
}));
vi.mock("@/hooks/useAutomationCode", () => ({
  useDryRun: () => ({ mutate: vi.fn(), isPending: false }),
}));
vi.mock("@/hooks/useAutomationEditor", () => ({
  useEditorAutomation: () => ({ data: automationHolder.value, isPending: false, error: null }),
  useCreateAutomationV2: () => ({ mutateAsync: createAutomation, isPending: false }),
  useSaveVersionV2: () => ({ mutateAsync: saveVersion, isPending: false }),
  useSetBlockOverrides: () => ({ mutateAsync: setOverrides, isPending: false }),
  useUpdateAutomationMetaV2: () => ({ mutateAsync: updateMeta, isPending: false }),
  useSetAutomationEnabledV2: () => ({ mutate: vi.fn(), isPending: false }),
  useDuplicateAutomation: () => ({ mutateAsync: duplicate, isPending: false }),
  useEventCatalog: () => ({ data: undefined }),
  useAutomationVersions: () => ({ data: undefined }),
  useActionCatalog: () => ({ data: undefined }),
  useEditorWebhookRegistrations: () => ({ data: { registrations: [] } }),
}));
// 3.4: the shell instantiates the test-with-sample state; the shell tests
// exercise save paths, not rendering, so the hook is inert here.
const runOnce = vi.hoisted(() => vi.fn(async () => null));
vi.mock("@/hooks/useAutomationTest", () => ({
  useAutomationTest: () => ({
    isTimed: false,
    samples: [],
    samplesLoading: false,
    sample: { kind: "latest" },
    setSample: vi.fn(),
    latest: null,
    tally: null,
    running: false,
    runOnce,
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
const runNow = { mutateAsync: vi.fn(async () => ({ runId: "run-manual" })), isPending: false };
vi.mock("@/hooks/useAutomations", () => ({ useRunNow: () => runNow }));
// The drafting pane opens a session stream; the shell tests exercise the
// hand-off (toggle, banner), not the thread.
const draftingHolder = vi.hoisted(() => ({ value: undefined as { working: boolean } | undefined }));
vi.mock("./build/DraftingPane", () => ({
  useDrafting: (sessionId: string | undefined) =>
    sessionId && draftingHolder.value
      ? { sessionId, working: draftingHolder.value.working }
      : undefined,
  DraftingPane: ({
    drafting,
    onClose,
  }: {
    drafting: { sessionId: string };
    onClose: () => void;
  }) => (
    <div data-testid="drafting-pane" data-session-id={drafting.sessionId}>
      <button type="button" onClick={onClose}>
        hide
      </button>
    </div>
  ),
}));
vi.mock("@/hooks/useModelRouters", () => ({
  useModelRouters: () => ({ data: { routers: [] } }),
  useRouterModels: () => ({ data: { models: [] } }),
}));
vi.mock("@tanstack/react-router", async (orig) => ({
  ...(await orig()),
  useNavigate: () => navigate,
  useBlocker: (opts: { enableBeforeUnload?: () => boolean }) => {
    blockerHolder.opts = opts;
    return { status: "idle", proceed: vi.fn(), reset: vi.fn() };
  },
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
    runOnce.mockClear();
    setOverrides.mockClear();
    updateMeta.mockClear();
    navigate.mockClear();
    searchHolder.value = {};
    draftingHolder.value = undefined;
  });

  it("a drafted automation shows the drafting agent beside the canvas, and says what it is doing", () => {
    draftingHolder.value = { working: true };
    const seed = { ...definition, blocks: [] };
    automationHolder.value = {
      automation: {
        ...automation("user"),
        draftSessionId: "draft-sess-1",
        version: {
          automationId: "x",
          number: 1,
          definitionJson: JSON.stringify(seed),
          createdAt: "",
        },
      },
    };
    const view = render(<AutomationEditor mode="edit" />);
    // The pane is open by default and carries the drafting session.
    expect(screen.getByTestId("drafting-pane").getAttribute("data-session-id")).toBe(
      "draft-sess-1",
    );
    // The seed (manual trigger, no steps, no agent) while the agent works.
    expect(screen.getByTestId("drafting-banner").getAttribute("data-working")).toBe("true");

    // The agent goes idle without saving: the banner says so.
    draftingHolder.value = { working: false };
    view.rerender(<AutomationEditor mode="edit" />);
    expect(screen.getByTestId("drafting-banner").getAttribute("data-working")).toBe("false");

    // The pane's own close hides it; the header toggle brings it back.
    fireEvent.click(screen.getByText("hide"));
    expect(screen.queryByTestId("drafting-pane")).toBeNull();
    expect(screen.getByTestId("drafting-toggle").getAttribute("aria-pressed")).toBe("false");
    fireEvent.click(screen.getByTestId("drafting-toggle"));
    expect(screen.getByTestId("drafting-pane")).toBeTruthy();

    // A real version lands: the banner goes, the pane stays.
    automationHolder.value = {
      automation: {
        ...automation("user"),
        draftSessionId: "draft-sess-1",
        currentVersion: 2,
        version: {
          automationId: "x",
          number: 2,
          definitionJson: JSON.stringify(definition),
          createdAt: "",
        },
      },
    };
    view.rerender(<AutomationEditor mode="edit" />);
    expect(screen.queryByTestId("drafting-banner")).toBeNull();
    expect(screen.getByTestId("drafting-pane")).toBeTruthy();
  });

  it("an automation built by hand has no drafting agent", () => {
    automationHolder.value = { automation: automation("user") };
    render(<AutomationEditor mode="edit" />);
    expect(screen.queryByTestId("drafting-toggle")).toBeNull();
    expect(screen.queryByTestId("drafting-pane")).toBeNull();
    expect(screen.queryByTestId("drafting-banner")).toBeNull();
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

  it("drafting (Builder v2): the user's own save never banners — the matching version adopts", async () => {
    const base = { ...automation("user"), draftSessionId: "draft-sess-1" };
    automationHolder.value = { automation: base };
    const view = render(<AutomationEditor mode="edit" />);

    // The user edits...
    fireEvent.click(screen.getByTestId("block-row-finder"));
    const prompt = screen.getByTestId("field-promptTemplate").querySelector("textarea")!;
    fireEvent.change(prompt, { target: { value: "My saved edit." } });

    // ...saves, and the refetch brings back v2 EQUAL to the screen state.
    const savedDef = {
      ...definition,
      blocks: [
        {
          ...definition.blocks[0]!,
          config: { ...definition.blocks[0]!.config, promptTemplate: "My saved edit." },
        },
      ],
    };
    automationHolder.value = {
      automation: {
        ...base,
        currentVersion: 2,
        version: {
          automationId: "x",
          number: 2,
          definitionJson: JSON.stringify(savedDef),
          createdAt: "",
        },
      },
    };
    view.rerender(<AutomationEditor mode="edit" />);
    expect(screen.queryByTestId("draft-stale-banner")).toBeNull();

    // The baseline advanced: a LATER agent version now adopts silently.
    const agentDef = {
      ...savedDef,
      blocks: [
        {
          ...savedDef.blocks[0]!,
          config: { ...savedDef.blocks[0]!.config, promptTemplate: "Agent v3." },
        },
      ],
    };
    automationHolder.value = {
      automation: {
        ...base,
        currentVersion: 3,
        version: {
          automationId: "x",
          number: 3,
          definitionJson: JSON.stringify(agentDef),
          createdAt: "",
        },
      },
    };
    view.rerender(<AutomationEditor mode="edit" />);
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
    // Every way in is its own column on the canvas, each headed by its
    // trigger node; there is no entrypoint bar to switch between them.
    expect(screen.queryByTestId("entrypoint-bar")).toBeNull();
    expect(screen.getByTestId("entrypoint-canvas-main")).toBeTruthy();
    expect(screen.getByTestId("entrypoint-canvas-sweep")).toBeTruthy();
    expect(screen.getByTestId("block-row-finder")).toBeTruthy();
    expect(screen.getByTestId("block-row-nudge")).toBeTruthy();

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
    searchHolder.value = { tab: "activity" };
    render(<AutomationEditor mode="edit" />);
    expect(screen.getByRole("tab", { name: "Activity" }).getAttribute("aria-selected")).toBe(
      "true",
    );
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

  it("a manual trigger has a Run now button that starts a run and lands on Activity", async () => {
    automationHolder.value = { automation: automation("user") };
    render(<AutomationEditor mode="edit" />);
    const button = screen.getByTestId("run-now-button");
    expect((button as HTMLButtonElement).disabled).toBe(false);
    fireEvent.click(button);
    await waitFor(() => expect(runNow.mutateAsync).toHaveBeenCalledWith({ automationId: "a1" }));
    const landed = navigate.mock.calls.at(-1)![0] as {
      search: (prev: Record<string, unknown>) => Record<string, unknown>;
    };
    expect(landed.search({})).toMatchObject({ tab: "activity" });
    // An unsaved edit disables it: Run now starts the SAVED definition.
    fireEvent.click(screen.getByTestId("block-row-finder"));
    const prompt = screen.getByTestId("field-promptTemplate").querySelector("textarea")!;
    fireEvent.change(prompt, { target: { value: "Changed." } });
    expect((screen.getByTestId("run-now-button") as HTMLButtonElement).disabled).toBe(true);
  });

  it("Try it renders the draft, unsaved edits and all, and opens the test drawer", async () => {
    automationHolder.value = { automation: automation("user") };
    render(<AutomationEditor mode="edit" />);
    fireEvent.click(screen.getByTestId("block-row-finder"));
    const prompt = screen.getByTestId("field-promptTemplate").querySelector("textarea")!;
    fireEvent.change(prompt, { target: { value: "Changed." } });
    expect(screen.getByTestId("test-drawer").getAttribute("data-state")).toBe("closed");
    fireEvent.click(screen.getByTestId("try-it-button"));
    await waitFor(() => expect(runOnce).toHaveBeenCalledTimes(1));
    expect(screen.getByTestId("test-drawer").getAttribute("data-state")).toBe("open");
    // Nothing was saved to try it.
    expect(saveVersion).not.toHaveBeenCalled();
  });

  it("?run= on the Build tab replays that run: the banner, the step's sentence, its inputs", async () => {
    searchHolder.value = { tab: "build", run: "run-7" };
    runHolder.value = {
      run: {
        brief: {
          id: "run-7",
          automationId: "a1",
          version: 1,
          status: "failed",
          error: "no host had capacity",
          triggerSource: "manual",
          dryRun: false,
          createdAt: new Date().toISOString(),
          startedAt: new Date().toISOString(),
          instanceId: "",
        },
        triggerJson: JSON.stringify({ kind: "manual" }),
        steps: [
          {
            blockId: "finder",
            attempt: 0,
            status: "failed",
            error: "no host had capacity",
            inputsJson: JSON.stringify({ promptTemplate: "Review it." }),
            outputsJson: "{}",
          },
        ],
        sessionIds: [],
      },
    };
    automationHolder.value = { automation: automation("user") };
    render(<AutomationEditor mode="edit" />);
    expect((await screen.findByTestId("replay-banner")).textContent).toContain(
      "no host had capacity",
    );
    // The failed step is selected and its details are in the inspector.
    const details = await screen.findByTestId("step-details");
    expect(details.textContent).toContain("Review it.");
    expect(screen.getByTestId("block-row-finder").textContent).toContain("no host had capacity");
    expect(screen.queryByLabelText("Insert block here")).toBeNull();
    // Leaving the replay drops ?run= and keeps the tab.
    fireEvent.click(screen.getByTestId("replay-exit"));
    const call = navigate.mock.calls.at(-1)![0] as {
      search: (p: Record<string, unknown>) => unknown;
    };
    expect(call.search({ tab: "build", run: "run-7" })).toEqual({ tab: "build" });
  });

  it("a created automation is saved: the move to its editor is not blocked as unsaved", async () => {
    render(<AutomationEditor mode="create" />);
    fireEvent.change(screen.getByPlaceholderText("Name your automation"), {
      target: { value: "Nightly" },
    });
    expect(blockerHolder.opts?.enableBeforeUnload?.()).toBe(true);
    fireEvent.click(screen.getByRole("button", { name: "Create" }));
    await waitFor(() => expect(createAutomation).toHaveBeenCalledTimes(1));
    await waitFor(() =>
      expect(navigate).toHaveBeenCalledWith(
        expect.objectContaining({ to: "/automations/$id", params: { id: "new-1" } }),
      ),
    );
    expect(blockerHolder.opts?.enableBeforeUnload?.()).toBe(false);
  });

  it("a new automation can be tried before it is created", async () => {
    render(<AutomationEditor mode="create" />);
    fireEvent.click(screen.getByTestId("try-it-button"));
    await waitFor(() => expect(runOnce).toHaveBeenCalledTimes(1));
  });
});
