import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, test, vi } from "vitest";

import type { SpecTemplate } from "@/hooks/useSpecTemplates";

const state = vi.hoisted(() => ({
  templates: [] as SpecTemplate[],
  profiles: [] as Array<{ id: string; name: string }>,
  create: vi.fn(),
  navigate: vi.fn(),
}));

vi.mock("@/hooks/useSpecTemplates", () => ({
  useSpecTemplates: () => ({ data: state.templates, isPending: false, error: null }),
}));

vi.mock("@/hooks/useProfiles", () => ({
  useProfiles: () => ({ data: { profiles: state.profiles }, isPending: false, error: null }),
}));

vi.mock("@/hooks/useSpecCreate", () => ({
  useCreateSpec: () => ({ mutate: state.create, isPending: false, error: null }),
}));

vi.mock("@tanstack/react-router", () => ({
  useNavigate: () => state.navigate,
}));

import { NewSpecSheet } from "./NewSpecSheet";

const builtIn: SpecTemplate = {
  id: "00000000-0000-4000-8000-000000000115",
  name: "Engineering design doc",
  description: "Intent to system detail.",
  builtIn: true,
  modifiedFromDefault: false,
  createdAt: "2026-08-11T00:00:00.000Z",
  updatedAt: "2026-08-11T00:00:00.000Z",
  layers: [
    { key: "intent", title: "Intent" },
    { key: "system", title: "System" },
  ],
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
    {
      key: "goals",
      title: "Goals",
      layerKey: "intent",
      guidance: "",
      doneCriteria: [],
      required: true,
      allowNa: false,
    },
    {
      key: "rollout",
      title: "Rollout",
      layerKey: "system",
      guidance: "",
      doneCriteria: [],
      required: true,
      allowNa: false,
    },
  ],
};

const orgTemplate: SpecTemplate = {
  ...builtIn,
  id: "00000000-0000-4000-8000-000000000116",
  name: "Incident review",
  description: "A post-incident record.",
  builtIn: false,
};

beforeEach(() => {
  state.templates = [builtIn, orgTemplate];
  state.profiles = [
    { id: "00000000-0000-4000-8000-0000000001a0", name: "Backend" },
    { id: "00000000-0000-4000-8000-0000000001a1", name: "Web" },
  ];
  state.create.mockReset();
  state.navigate.mockReset();
});

describe("NewSpecSheet", () => {
  test("template cards show their document structure", async () => {
    render(<NewSpecSheet open onOpenChange={() => {}} />);

    const cards = await screen.findAllByRole("radio");
    const designDoc = cards[0]!.closest("label")!;

    // Structure: every layer, with the sections it holds (R2).
    expect(within(designDoc).getByText("Intent")).toBeTruthy();
    expect(within(designDoc).getByText("Problem · Goals")).toBeTruthy();
    expect(within(designDoc).getByText("System")).toBeTruthy();
    expect(within(designDoc).getByText("Rollout")).toBeTruthy();

    const incident = cards[1]!.closest("label")!;
    expect(within(incident).getByText("A post-incident record.")).toBeTruthy();
    expect(within(designDoc).queryByText("Process")).toBeNull();
  });

  test("the organization default is preselected", async () => {
    render(<NewSpecSheet open onOpenChange={() => {}} />);

    const cards = await screen.findAllByRole("radio");
    await waitFor(() => expect((cards[0] as HTMLInputElement).checked).toBe(true));
    expect((cards[1] as HTMLInputElement).checked).toBe(false);
    expect(screen.getByText("Default")).toBeTruthy();
  });

  test("the sheet states that the template locks when the session starts", async () => {
    render(<NewSpecSheet open onOpenChange={() => {}} />);

    expect(await screen.findByText("The template is locked once the session starts.")).toBeTruthy();
  });

  test("creating sends the chosen template, profile, and problem statement", async () => {
    const user = userEvent.setup();
    const onOpenChange = vi.fn();
    render(<NewSpecSheet open onOpenChange={onOpenChange} />);

    await user.type(
      await screen.findByLabelText("What problem are you solving?"),
      "Queued prompts are lost after an eviction.",
    );
    await user.click((await screen.findAllByRole("radio"))[1]!);
    await user.click(screen.getByRole("button", { name: "Create spec" }));

    await waitFor(() => expect(state.create).toHaveBeenCalledTimes(1));
    const [input] = state.create.mock.calls[0]!;
    expect(input).toMatchObject({
      templateId: orgTemplate.id,
      profileId: "00000000-0000-4000-8000-0000000001a0",
      problemStatement: "Queued prompts are lost after an eviction.",
    });
    expect(typeof input.idempotencyKey).toBe("string");
    expect(input.idempotencyKey.length).toBeGreaterThan(0);
    expect(input.title).toBeUndefined();
  });

  test("a spec cannot be created without a problem statement", async () => {
    render(<NewSpecSheet open onOpenChange={() => {}} />);

    const submit = await screen.findByRole("button", { name: "Create spec" });
    expect((submit as HTMLButtonElement).disabled).toBe(true);
    expect(state.create).not.toHaveBeenCalled();
  });

  test("a repeated send of the same request reuses one idempotency key", async () => {
    const user = userEvent.setup();
    render(<NewSpecSheet open onOpenChange={() => {}} />);

    await user.type(
      await screen.findByLabelText("What problem are you solving?"),
      "Queued prompts are lost.",
    );
    const submit = screen.getByRole("button", { name: "Create spec" });
    await user.click(submit);
    await user.click(submit);

    await waitFor(() => expect(state.create).toHaveBeenCalledTimes(2));
    expect(state.create.mock.calls[0]![0].idempotencyKey).toBe(
      state.create.mock.calls[1]![0].idempotencyKey,
    );
  });

  test("an edited request gets its own idempotency key", async () => {
    const user = userEvent.setup();
    render(<NewSpecSheet open onOpenChange={() => {}} />);

    const problem = await screen.findByLabelText("What problem are you solving?");
    await user.type(problem, "One problem.");
    await user.click(screen.getByRole("button", { name: "Create spec" }));
    await user.type(problem, " And another.");
    await user.click(screen.getByRole("button", { name: "Create spec" }));

    await waitFor(() => expect(state.create).toHaveBeenCalledTimes(2));
    expect(state.create.mock.calls[0]![0].idempotencyKey).not.toBe(
      state.create.mock.calls[1]![0].idempotencyKey,
    );
  });
});
