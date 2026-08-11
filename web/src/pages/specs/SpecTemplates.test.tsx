import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, test, vi } from "vitest";

import type { SpecTemplate } from "@/hooks/useSpecTemplates";

const state = vi.hoisted(() => ({
  isAdmin: false,
  templates: [] as SpecTemplate[],
  save: vi.fn(),
  clone: vi.fn(),
  restore: vi.fn(),
}));

vi.mock("@/auth/AuthProvider", () => ({ useIsAdmin: () => state.isAdmin }));
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
  stageFlags: { alternatives: "on", talkItThrough: "suggested", gapCheck: "on" },
};

beforeEach(() => {
  state.isAdmin = false;
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
  test("a member can view the full template but cannot change it", async () => {
    render(<SpecTemplates />);

    expect(
      (await screen.findByDisplayValue("Engineering design doc")).hasAttribute("disabled"),
    ).toBe(true);
    expect(screen.getByDisplayValue("Problem").hasAttribute("disabled")).toBe(true);
    expect(screen.getByLabelText("Alternatives stage").hasAttribute("disabled")).toBe(true);
    expect(screen.queryByRole("button", { name: "Save template" })).toBeNull();
    expect(screen.queryByRole("button", { name: "New" })).toBeNull();
  });

  test("an admin edits the built-in template in place", async () => {
    state.isAdmin = true;
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
    state.isAdmin = true;
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
    state.isAdmin = true;
    render(<SpecTemplates />);
    await screen.findByDisplayValue("Engineering design doc");

    fireEvent.click(screen.getByRole("button", { name: "Restore default" }));
    await waitFor(() => expect(state.restore).toHaveBeenCalledWith(builtIn.id));

    fireEvent.click(screen.getByRole("button", { name: "Clone" }));
    await waitFor(() => expect(state.clone).toHaveBeenCalledWith(builtIn.id));
  });

  test("an admin can start a valid new template", async () => {
    state.isAdmin = true;
    render(<SpecTemplates />);
    fireEvent.click(await screen.findByRole("button", { name: "New" }));

    expect(screen.getByDisplayValue("Untitled template")).toBeTruthy();
    expect(screen.getByDisplayValue("First layer")).toBeTruthy();
    expect(screen.getByDisplayValue("First section")).toBeTruthy();
  });
});
