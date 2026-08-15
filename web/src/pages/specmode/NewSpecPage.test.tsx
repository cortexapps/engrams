import { cleanup, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, describe, expect, test, vi } from "vitest";

import type { CreateSpecInput } from "@/hooks/useSpecCreate";
import type { SpecTemplate } from "@/hooks/useSpecTemplates";
import { renderWithProviders } from "@/test-utils";

interface TestProfile {
  id: string;
  name: string;
  description: string;
  icon: string;
  imageId: string;
  includeUserTokens: boolean;
  integrationGrants: never[];
  network: { default: string; allowHosts: string[]; allowHostPatterns: string[] };
  repos: Array<{ path: string; remote: { owner: string; name: string } }>;
  harness: string;
  modelRouter: string;
}

const state = vi.hoisted(() => ({
  templates: [] as SpecTemplate[],
  profiles: [] as TestProfile[],
  createError: null as Error | null,
  create: vi.fn(),
  navigate: vi.fn(),
}));

vi.mock("@/hooks/useSpecTemplates", () => ({
  useSpecTemplates: () => ({
    data: state.templates,
    isPending: false,
    error: null,
  }),
}));
vi.mock("@/hooks/useProfiles", () => ({
  useProfiles: () => ({
    data: { profiles: state.profiles },
    isPending: false,
    error: null,
    refetch: vi.fn(),
  }),
}));
vi.mock("@/hooks/useSpecCreate", async () => {
  const { useState } = await import("react");
  return {
    useCreateSpec: () => {
      const [error, setError] = useState<Error | null>(null);
      return {
        mutate: (input: CreateSpecInput, options: unknown) => {
          state.create(input, options);
          if (state.createError) setError(state.createError);
        },
        isPending: false,
        error,
      };
    },
  };
});
vi.mock("@/hooks/useIntegrations", () => ({
  useIntegrationCatalog: () => ({ data: { providers: [] } }),
}));
vi.mock("@/hooks/useEnabledImages", () => ({
  useEnabledImages: () => ({ data: [] }),
}));
vi.mock("@/hooks/useHarnessCatalog", () => ({
  useHarnessCatalog: () => ({
    data: [
      {
        name: "claude",
        descriptor: {
          name: "claude",
          label: "Claude Code",
          routerProtocols: ["anthropic"],
          models: [{ id: "sonnet", label: "Sonnet", default: true }],
          effort: [{ id: "high", label: "High" }],
          modes: [{ id: "plan", label: "Plan" }],
        },
      },
      {
        name: "codex",
        descriptor: {
          name: "codex",
          label: "Codex",
          routerProtocols: ["anthropic"],
          models: [{ id: "gpt-5", label: "GPT-5", default: true }],
          effort: [{ id: "high", label: "High" }],
          modes: [{ id: "plan", label: "Plan" }],
        },
      },
    ],
  }),
}));
vi.mock("@/hooks/useModelRouters", () => ({
  useModelRouters: () => ({
    data: { routers: [{ id: "router-1", label: "Gateway", protocols: ["anthropic"] }] },
  }),
  useRouterModels: () => ({ data: { models: [] } }),
}));
vi.mock("@/hooks/useHarnessEnv", () => ({
  useHarnessEnv: () => ({ data: [] }),
}));
vi.mock("@/hooks/useCredentials", () => ({
  useCredentials: () => ({ data: [] }),
}));
vi.mock("@tanstack/react-router", async (importOriginal) => {
  const actual = await importOriginal<typeof import("@tanstack/react-router")>();
  return { ...actual, useNavigate: () => state.navigate };
});

import { NewSpecPage } from "./NewSpecPage";

const engineering: SpecTemplate = {
  id: "00000000-0000-4000-8000-000000000115",
  name: "Engineering spec",
  description: "Alternatives get compared before the design is written.",
  builtIn: true,
  modifiedFromDefault: false,
  createdAt: "2026-08-11T00:00:00.000Z",
  updatedAt: "2026-08-11T00:00:00.000Z",
  layers: [{ key: "intent", title: "Intent" }],
  sections: [
    {
      key: "problem",
      title: "Problem",
      layerKey: "intent",
      guidance: "",
      doneCriteria: [],
      required: true,
      allowNa: false,
    },
  ],
};

const lightweight: SpecTemplate = {
  ...engineering,
  id: "00000000-0000-4000-8000-000000000116",
  name: "Lightweight RFC",
  description: "For a change small enough to hold in your head.",
  builtIn: false,
  sections: [
    ...engineering.sections,
    {
      key: "rollout",
      title: "Rollout",
      layerKey: "intent",
      guidance: "",
      doneCriteria: [],
      required: true,
      allowNa: false,
    },
  ],
};

const backend = {
  id: "00000000-0000-4000-8000-0000000001a0",
  name: "Backend",
  description: "Backend work.",
  icon: "Server",
  imageId: "img-1",
  includeUserTokens: false,
  integrationGrants: [],
  network: { default: "deny", allowHosts: [], allowHostPatterns: [] },
  repos: [
    {
      path: "/workspace/engrams",
      remote: { owner: "cortexapps", name: "engrams" },
    },
  ],
  harness: "claude",
  modelRouter: "router-1",
};

const platform = {
  ...backend,
  id: "00000000-0000-4000-8000-0000000001a1",
  name: "Platform",
  description: "Platform work.",
};

beforeEach(() => {
  localStorage.clear();
  state.templates = [engineering, lightweight];
  state.profiles = [backend, platform];
  state.createError = null;
  state.create.mockReset();
  state.navigate.mockReset();
});

afterEach(() => {
  cleanup();
  localStorage.clear();
});

describe("NewSpecPage", () => {
  test("keeps the template radiogroup and has no repository chips", async () => {
    const user = userEvent.setup();
    renderWithProviders(<NewSpecPage />);

    const shapes = await screen.findAllByRole("radio");
    expect(shapes).toHaveLength(2);
    await waitFor(() => expect(shapes[0]?.getAttribute("aria-checked")).toBe("true"));
    await user.click(shapes[0]!);
    await user.keyboard("{ArrowRight}");
    expect(document.activeElement).toBe(shapes[1]);
    expect(screen.getByText("The template is locked once the session starts.")).toBeTruthy();
    expect(screen.queryByText("cortexapps/engrams")).toBeNull();
    expect(screen.queryByText("+1 more")).toBeNull();
  });

  test("posts the chosen profile and all composer overrides", async () => {
    const user = userEvent.setup();
    renderWithProviders(<NewSpecPage />);

    expect(await screen.findByText("Backend")).toBeTruthy();
    await user.click(screen.getByTestId("profile-switcher"));
    await user.click(screen.getByTestId("profile-option-" + platform.id));

    // The new-spec composer is quiet: the plumbing sits behind Advanced.
    await user.click(screen.getByRole("button", { name: "Advanced" }));
    await user.click(screen.getByTestId("session-harness-select"));
    await user.click(screen.getByRole("menuitem", { name: "Codex" }));
    await user.click(screen.getByTestId("session-router-select"));
    await user.click(screen.getByRole("menuitem", { name: "Direct" }));
    await user.click(screen.getByTestId("session-model-select"));
    await user.click(screen.getByRole("option", { name: /GPT-5/ }));
    await user.click(screen.getByTestId("session-effort-select"));
    await user.click(screen.getByRole("menuitem", { name: "High" }));
    await user.click(screen.getByTestId("session-mode-chip"));

    await user.type(
      await screen.findByLabelText("What is the spec about?"),
      "Queued prompts are lost after an eviction.",
    );
    await user.click(screen.getByRole("button", { name: "Start" }));

    await waitFor(() => expect(state.create).toHaveBeenCalledTimes(1));
    expect(createInput(0)).toMatchObject({
      templateId: engineering.id,
      profileId: platform.id,
      problemStatement: "Queued prompts are lost after an eviction.",
      harness: "codex",
      modelRouter: "",
      model: "gpt-5",
      effort: "high",
      harnessMode: "plan",
    });
    expect(createInput(0).idempotencyKey).not.toBe("");
  });

  test("navigates to the created spec", async () => {
    const user = userEvent.setup();
    state.create.mockImplementation(
      (_input: unknown, options?: { onSuccess?: (spec: { id: string }) => void }) => {
        options?.onSuccess?.({ id: "spec-new" });
      },
    );
    renderWithProviders(<NewSpecPage />);

    await user.type(
      await screen.findByLabelText("What is the spec about?"),
      "Design a safer queue.",
    );
    await user.click(screen.getByRole("button", { name: "Start" }));

    await waitFor(() =>
      expect(state.navigate).toHaveBeenCalledWith({
        to: "/specs/$specId",
        params: { specId: "spec-new" },
      }),
    );
  });

  test("keeps the typed prompt when creation fails", async () => {
    const user = userEvent.setup();
    renderWithProviders(<NewSpecPage />);
    const prompt = await screen.findByLabelText<HTMLElement>("What is the spec about?");

    state.createError = new Error("The service is unavailable.");
    await user.type(prompt, "Keep this exact prompt after a failed create.");
    await user.click(screen.getByRole("button", { name: "Start" }));
    expect(state.create).toHaveBeenCalledTimes(1);

    expect(prompt.textContent).toBe("Keep this exact prompt after a failed create.");
    expect(screen.getByText("The spec did not start. The service is unavailable.")).toBeTruthy();
  });

  test("reuses a key for retries and changes it with composer state", async () => {
    const user = userEvent.setup();
    renderWithProviders(<NewSpecPage />);
    const prompt = await screen.findByLabelText("What is the spec about?");

    await user.type(prompt, "One problem.");
    await user.click(screen.getByRole("button", { name: "Start" }));
    await user.click(screen.getByRole("button", { name: "Start" }));
    await waitFor(() => expect(state.create).toHaveBeenCalledTimes(2));
    expect(createInput(0).idempotencyKey).toBe(createInput(1).idempotencyKey);

    await user.click(screen.getByTestId("session-mode-chip"));
    await user.click(screen.getByRole("button", { name: "Start" }));
    await waitFor(() => expect(state.create).toHaveBeenCalledTimes(3));
    expect(createInput(2).idempotencyKey).not.toBe(createInput(1).idempotencyKey);
  });
});

function createInput(index: number): CreateSpecInput {
  return state.create.mock.calls[index]?.[0] as CreateSpecInput;
}
