import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, test, vi } from "vitest";

import type { CreateSpecInput } from "@/hooks/useSpecCreate";
import type { SpecTemplate } from "@/hooks/useSpecTemplates";

const state = vi.hoisted(() => ({
  templates: [] as SpecTemplate[],
  profiles: [] as Array<{
    id: string;
    name: string;
    repos: Array<{
      path: string;
      remote?: { owner: string; name: string };
    }>;
  }>,
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
  }),
}));

vi.mock("@/hooks/useSpecCreate", () => ({
  useCreateSpec: () => ({
    mutate: state.create,
    isPending: false,
    error: state.createError,
  }),
}));

vi.mock("@tanstack/react-router", () => ({
  useNavigate: () => state.navigate,
}));

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

beforeEach(() => {
  state.templates = [engineering, lightweight];
  state.profiles = [
    {
      id: "00000000-0000-4000-8000-0000000001a0",
      name: "Backend",
      repos: [
        {
          path: "/workspace/engrams",
          remote: { owner: "cortexapps", name: "engrams" },
        },
      ],
    },
  ];
  state.createError = null;
  state.create.mockReset();
  state.navigate.mockReset();
});

describe("NewSpecPage", () => {
  test("lists the available template shapes and preselects the default", async () => {
    render(<NewSpecPage />);

    expect(screen.getByText("Engineering spec")).toBeTruthy();
    expect(screen.getByText("Lightweight RFC")).toBeTruthy();
    expect(
      screen.getByText("Alternatives get compared before the design is written."),
    ).toBeTruthy();
    expect(screen.getByText("For a change small enough to hold in your head.")).toBeTruthy();

    const shapes = await screen.findAllByRole("radio");
    expect(shapes).toHaveLength(2);
    await waitFor(() => expect(shapes[0]?.getAttribute("aria-checked")).toBe("true"));
    expect(shapes[1]?.getAttribute("aria-checked")).toBe("false");
    expect(screen.getByText("The template is locked once the session starts.")).toBeTruthy();
  });

  test("starts with the chosen shape and navigates to the created spec", async () => {
    const user = userEvent.setup();
    state.create.mockImplementation(
      (_input: unknown, options?: { onSuccess?: (spec: { id: string }) => void }) => {
        options?.onSuccess?.({ id: "spec-new" });
      },
    );
    render(<NewSpecPage />);

    await user.type(
      screen.getByLabelText("What is the spec about?"),
      "Queued prompts are lost after an eviction.",
    );
    await user.click(screen.getByRole("radio", { name: /Lightweight RFC/ }));
    await user.click(screen.getByRole("button", { name: "Start" }));

    await waitFor(() => expect(state.create).toHaveBeenCalledTimes(1));
    expect(createInput(0)).toMatchObject({
      templateId: lightweight.id,
      profileId: "00000000-0000-4000-8000-0000000001a0",
      problemStatement: "Queued prompts are lost after an eviction.",
    });
    expect(createInput(0).idempotencyKey).not.toBe("");
    expect(state.navigate).toHaveBeenCalledWith({
      to: "/specs/$specId",
      params: { specId: "spec-new" },
    });
  });

  test("keeps the typed prompt when creation fails", async () => {
    const user = userEvent.setup();
    const view = render(<NewSpecPage />);
    const prompt = screen.getByLabelText<HTMLTextAreaElement>("What is the spec about?");

    await user.type(prompt, "Keep this exact prompt after a failed create.");
    await user.click(screen.getByRole("button", { name: "Start" }));
    expect(state.create).toHaveBeenCalledTimes(1);

    state.createError = new Error("The service is unavailable.");
    view.rerender(<NewSpecPage />);

    expect(prompt.value).toBe("Keep this exact prompt after a failed create.");
    expect(screen.getByText("The spec did not start. The service is unavailable.")).toBeTruthy();
  });

  test("reuses one idempotency key for the same request", async () => {
    const user = userEvent.setup();
    render(<NewSpecPage />);

    await user.type(screen.getByLabelText("What is the spec about?"), "One problem.");
    await user.click(screen.getByRole("button", { name: "Start" }));
    await user.click(screen.getByRole("button", { name: "Start" }));

    await waitFor(() => expect(state.create).toHaveBeenCalledTimes(2));
    expect(createInput(0).idempotencyKey).toBe(createInput(1).idempotencyKey);
  });

  test("uses a new idempotency key after the prompt changes", async () => {
    const user = userEvent.setup();
    render(<NewSpecPage />);
    const prompt = screen.getByLabelText("What is the spec about?");

    await user.type(prompt, "One problem.");
    await user.click(screen.getByRole("button", { name: "Start" }));
    await user.type(prompt, " A second detail.");
    await user.click(screen.getByRole("button", { name: "Start" }));

    await waitFor(() => expect(state.create).toHaveBeenCalledTimes(2));
    expect(createInput(0).idempotencyKey).not.toBe(createInput(1).idempotencyKey);
  });
});

function createInput(index: number): CreateSpecInput {
  return state.create.mock.calls[index]?.[0] as CreateSpecInput;
}
