import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, test, vi } from "vitest";

import type { SpecTemplate } from "@/hooks/useSpecTemplates";

const state = vi.hoisted(() => ({
  templates: [] as SpecTemplate[],
  save: vi.fn(),
  clone: vi.fn(),
  restore: vi.fn(),
}));

vi.mock("@/hooks/useSpecTemplates", () => ({
  useSpecTemplates: () => ({ data: state.templates, isPending: false, error: null }),
  useSaveSpecTemplate: () => ({
    mutateAsync: state.save,
    isPending: false,
    error: null,
  }),
  useCloneSpecTemplate: () => ({
    mutateAsync: state.clone,
    isPending: false,
    error: null,
  }),
  useRestoreSpecTemplate: () => ({
    mutateAsync: state.restore,
    isPending: false,
    error: null,
  }),
}));

import { SpecTemplates } from "./SpecTemplates";

const builtIn: SpecTemplate = {
  id: "00000000-0000-4000-8000-000000000115",
  name: "Engineering design doc",
  description: "Three layers",
  builtIn: true,
  modifiedFromDefault: true,
  createdAt: "2026-08-11T00:00:00.000Z",
  updatedAt: "2026-08-11T01:00:00.000Z",
  layers: [{ key: "intent", title: "Intent", description: "Frame the work" }],
  sections: [
    {
      key: "problem",
      title: "Problem",
      layerKey: "intent",
      guidance: "State the problem.",
      doneCriteria: ["The problem is clear."],
      required: true,
      allowNa: false,
    },
  ],
};

beforeEach(() => {
  state.templates = [structuredClone(builtIn)];
  state.save.mockReset();
  state.clone.mockReset();
  state.restore.mockReset();
  state.save.mockResolvedValue(structuredClone(builtIn));
  state.clone.mockResolvedValue({
    ...structuredClone(builtIn),
    id: "template-copy",
    builtIn: false,
  });
  state.restore.mockResolvedValue({ ...structuredClone(builtIn), modifiedFromDefault: false });
});

describe("SpecTemplates", () => {
  test("an admin edits the built-in template in place", async () => {
    render(<SpecTemplates />);
    const name = await screen.findByDisplayValue("Engineering design doc");

    fireEvent.change(name, { target: { value: "Engineering platform design" } });
    fireEvent.click(screen.getByRole("button", { name: "Save template" }));

    await waitFor(() => expect(state.save).toHaveBeenCalledTimes(1));
    expect(state.save).toHaveBeenCalledWith(
      expect.objectContaining({
        id: builtIn.id,
        definition: expect.objectContaining({ name: "Engineering platform design" }),
      }),
    );
    expect(screen.getByText("Modified from default")).toBeTruthy();
  });

  test("an admin can type multiple done criteria", async () => {
    render(<SpecTemplates />);
    const criteria = (await screen.findByLabelText("Done criteria")) as HTMLTextAreaElement;

    fireEvent.change(criteria, { target: { value: " First criterion \n" } });
    expect(criteria.value).toBe(" First criterion \n");
    fireEvent.change(criteria, { target: { value: " First criterion \nSecond criterion " } });
    fireEvent.click(screen.getByRole("button", { name: "Save template" }));

    await waitFor(() => expect(state.save).toHaveBeenCalledTimes(1));
    expect(state.save).toHaveBeenCalledWith(
      expect.objectContaining({
        definition: expect.objectContaining({
          sections: [
            expect.objectContaining({
              doneCriteria: ["First criterion", "Second criterion"],
            }),
          ],
        }),
      }),
    );
  });

  test("an admin can restore and clone the built-in template", async () => {
    render(<SpecTemplates />);
    await screen.findByDisplayValue("Engineering design doc");

    fireEvent.click(screen.getByRole("button", { name: "Restore default" }));
    await waitFor(() => expect(state.restore).toHaveBeenCalledWith(builtIn.id));

    fireEvent.click(screen.getByRole("button", { name: "Clone" }));
    await waitFor(() => expect(state.clone).toHaveBeenCalledWith(builtIn.id));
  });

  test("an admin can start a valid new template", async () => {
    render(<SpecTemplates />);
    fireEvent.click(await screen.findByRole("button", { name: "New" }));

    expect(screen.getByDisplayValue("Untitled template")).toBeTruthy();
    expect(screen.getByDisplayValue("First layer")).toBeTruthy();
    expect(screen.getByDisplayValue("First section")).toBeTruthy();
  });
});
