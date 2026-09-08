// Regression: the edit page crashed in production with React #310 ("rendered
// more hooks than during the previous render") because a hook sat BELOW the
// shell's loading early-return. The sibling test file mocks every hook to a
// constant, so it can never observe a hook-count change across renders. This
// file drives the one transition that matters — pending → loaded — with a
// STATEFUL useEditorAutomation and the hooks below it real, and asserts the
// shell survives it. Any future hook placed after an early return fails here.

import { beforeEach, describe, expect, it, vi } from "vitest";
import { act, render, screen } from "@testing-library/react";

const editorState = vi.hoisted(() => ({
  value: { data: undefined as unknown, isPending: true, error: null as unknown },
  listeners: new Set<() => void>(),
}));

vi.mock("@/hooks/useAutomationEditor", async (orig) => {
  const real = await orig<typeof import("@/hooks/useAutomationEditor")>();
  const { useSyncExternalStore } = await import("react");
  return {
    ...real,
    // A real hook (useSyncExternalStore) so the shell re-renders when the
    // automation "arrives", exactly like the connect-query hook does.
    useEditorAutomation: () =>
      useSyncExternalStore(
        (cb) => {
          editorState.listeners.add(cb);
          return () => editorState.listeners.delete(cb);
        },
        () => editorState.value,
      ),
    useCreateAutomationV2: () => ({ mutateAsync: vi.fn(), isPending: false }),
    useSaveVersionV2: () => ({ mutateAsync: vi.fn(), isPending: false }),
    useSetBlockOverrides: () => ({ mutateAsync: vi.fn(), isPending: false }),
    useUpdateAutomationMetaV2: () => ({ mutateAsync: vi.fn(), isPending: false }),
    useSetAutomationEnabledV2: () => ({ mutate: vi.fn(), isPending: false }),
    useDuplicateAutomation: () => ({ mutateAsync: vi.fn(), isPending: false }),
    useEventCatalog: () => ({ data: undefined }),
    useActionCatalog: () => ({ data: undefined }),
    useEditorWebhookRegistrations: () => ({ data: { registrations: [] } }),
  };
});
// The hooks BELOW the early returns are the ones whose placement this test
// guards. They stay real in shape (each is itself a hook call) but inert.
vi.mock("@/hooks/useAutomationTest", async () => {
  const { useMemo } = await import("react");
  return {
    useAutomationTest: () =>
      useMemo(
        () => ({
          isTimed: false,
          samples: [],
          samplesLoading: false,
          sample: { kind: "none" as const },
          setSample: vi.fn(),
          latest: null,
          tally: null,
          running: false,
          runOnce: vi.fn(),
          runAcrossSamples: vi.fn(),
          variableValues: {},
        }),
        [],
      ),
  };
});
vi.mock("@/hooks/useAutomationCode", () => ({
  useDryRun: () => ({ mutate: vi.fn(), isPending: false }),
}));
vi.mock("@/hooks/useProfiles", () => ({
  useProfiles: () => ({ data: { profiles: [] } }),
}));
vi.mock("@/hooks/useHarnessCatalog", () => ({ useHarnessCatalog: () => ({ data: [] }) }));
vi.mock("@/hooks/useModelRouters", () => ({
  useModelRouters: () => ({ data: { routers: [] } }),
}));
vi.mock("@tanstack/react-router", async (orig) => ({
  ...(await orig()),
  useNavigate: () => vi.fn(),
  useParams: () => ({ id: "a1" }),
  useSearch: () => ({ tab: "build" }),
  Link: ({ children }: { children: React.ReactNode }) => <a>{children}</a>,
}));
vi.mock("@/components/ui/resizable", () => ({
  ResizablePanelGroup: ({ children }: { children: React.ReactNode }) => <div>{children}</div>,
  ResizablePanel: ({ children }: { children: React.ReactNode }) => <div>{children}</div>,
  ResizableHandle: () => null,
}));

import { AutomationEditor } from "./AutomationEditor";

vi.mock("@/hooks/useAutomationRuns", () => ({
  useRunList: () => ({ data: undefined, isPending: false, error: null }),
}));

const loaded = {
  automation: {
    id: "a1",
    name: "My automation",
    description: "",
    kind: "user",
    enabled: true,
    currentVersion: 1,
    inputsJson: "{}",
    blockOverridesJson: "{}",
    archived: false,
    createdAt: "",
    updatedAt: "",
    version: {
      automationId: "a1",
      number: 1,
      definitionJson: JSON.stringify({
        engine: 1,
        trigger: { kind: "manual" },
        blocks: [
          {
            id: "launch",
            type: "create_session",
            config: { profileId: "p", promptTemplate: "go" },
          },
        ],
        inputsSchema: [],
        settings: { endSessionsOnFinish: false },
      }),
      createdAt: "",
    },
  },
};

describe("AutomationEditor hook order", () => {
  beforeEach(() => {
    editorState.value = { data: undefined, isPending: true, error: null };
    editorState.listeners.clear();
  });

  it("survives the pending → loaded transition (every hook runs on both renders)", () => {
    const errors: unknown[] = [];
    const onError = (e: ErrorEvent) => {
      errors.push(e.error);
      e.preventDefault();
    };
    window.addEventListener("error", onError);
    try {
      render(<AutomationEditor mode="edit" />);
      expect(screen.getByRole("status", { name: /loading/i })).toBeTruthy();

      // The automation arrives: the shell must re-render without changing
      // its hook count. React #310 surfaces here as a thrown render error.
      act(() => {
        editorState.value = { data: loaded, isPending: false, error: null };
        for (const cb of editorState.listeners) cb();
      });

      expect(screen.queryByText(/loading/i)).toBeNull();
      expect(screen.getByDisplayValue("My automation")).toBeTruthy();
      expect(errors).toEqual([]);
    } finally {
      window.removeEventListener("error", onError);
    }
  });
});
