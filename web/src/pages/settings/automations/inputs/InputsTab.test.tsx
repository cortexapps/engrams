// The Inputs tab under the built-in editing model (ADR 0119 phase 3.6): the
// review-shaped schema renders as a form on a BUILT-IN, a repository can be
// added by free-typing its key (the picker is ledger-observed), and Save
// posts the whole value object through SetInputs. Validation blocks Save.

import { describe, it, expect, vi, beforeEach } from "vitest";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";

import { InputsTab } from "./InputsTab";
import { REVIEW_INPUTS_SCHEMA } from "@/lib/automation-inputs.fixture";

const automationHolder = vi.hoisted(() => ({
  value: undefined as undefined | { automation: unknown },
}));
const setInputs = vi.hoisted(() => vi.fn().mockResolvedValue({ automation: { id: "b1" } }));
const keyOptions = vi.hoisted(() => ({
  value: { options: [{ key: "acme/observed", label: "acme/observed" }] },
}));

vi.mock("@/hooks/useAutomationEditor", () => ({
  useEditorAutomation: () => ({ data: automationHolder.value, isLoading: false, error: null }),
}));
vi.mock("@/hooks/useAutomationInputs", () => ({
  useInputKeyOptions: () => ({ data: keyOptions.value }),
  useSetInputs: () => ({ mutateAsync: setInputs, isPending: false }),
}));
vi.mock("@/hooks/useOrgSecrets", () => ({
  useOrgSecretNames: () => ({ data: ["GH_TOKEN"], isLoading: false }),
}));
// Radix Select needs pointer APIs jsdom lacks; the free-typed path is what
// these tests exercise, so the picker renders as a plain stub.
vi.mock("@/components/ui/select", () => ({
  Select: ({ children }: { children: React.ReactNode }) => <div>{children}</div>,
  SelectTrigger: ({ children }: { children: React.ReactNode }) => <div>{children}</div>,
  SelectValue: () => null,
  SelectContent: ({ children }: { children: React.ReactNode }) => <div>{children}</div>,
  SelectItem: ({ children }: { children: React.ReactNode }) => <div>{children}</div>,
}));

function reviewBuiltin(inputs: Record<string, unknown>, schema: unknown[] = REVIEW_INPUTS_SCHEMA) {
  return {
    automation: {
      id: "b1",
      name: "PR review",
      description: "",
      kind: "builtin",
      builtinKey: "pr_review",
      enabled: true,
      currentVersion: 1,
      inputsJson: JSON.stringify(inputs),
      blockOverridesJson: "{}",
      version: {
        definitionJson: JSON.stringify({
          engine: 1,
          trigger: { kind: "manual" },
          blocks: [],
          inputsSchema: schema,
          settings: { endSessionsOnFinish: false },
        }),
      },
    },
  };
}

describe("InputsTab", () => {
  beforeEach(() => {
    setInputs.mockClear();
    automationHolder.value = reviewBuiltin({
      repos: { "engrams/engrams": { mode: "auto", autofix: false } },
      mention: "@engrams",
    });
  });

  it("renders the review-shaped schema on a built-in with the stored values", () => {
    render(<InputsTab automationId="b1" />);
    expect(screen.getByText(/This is a built-in/)).toBeTruthy();
    expect(screen.getByTestId("map-row-engrams/engrams")).toBeTruthy();
    expect((screen.getByLabelText("Mention") as HTMLInputElement).value).toBe("@engrams");
    expect((screen.getByLabelText("Reviewer profile") as HTMLInputElement).value).toBe(
      "pr_reviewer",
    );
    expect(screen.getByLabelText("Instructions").tagName).toBe("TEXTAREA");
    expect(screen.getByTestId("list-input-categories")).toBeTruthy();
    // Not dirty yet → Save disabled.
    expect(
      (screen.getByRole("button", { name: /save inputs/i }) as HTMLButtonElement).disabled,
    ).toBe(true);
  });

  it("adds a free-typed repo (normalized), edits a row, and posts the inputs_json", async () => {
    render(<InputsTab automationId="b1" />);

    const keyInput = screen.getByLabelText("New Repositories key");
    fireEvent.change(keyInput, { target: { value: " Acme/NewRepo " } });
    fireEvent.keyDown(keyInput, { key: "Enter" });
    expect(screen.getByTestId("map-row-acme/newrepo")).toBeTruthy();

    // Flip the new row's autofix on.
    fireEvent.click(screen.getByLabelText("acme/newrepo Autofix"));
    // Change the mention.
    fireEvent.change(screen.getByLabelText("Mention"), { target: { value: "@reviewer" } });

    fireEvent.click(screen.getByRole("button", { name: /save inputs/i }));
    await waitFor(() => expect(setInputs).toHaveBeenCalledTimes(1));
    const payload = JSON.parse(setInputs.mock.calls[0]![0].inputsJson) as Record<string, unknown>;
    expect(payload).toEqual({
      repos: {
        "engrams/engrams": { mode: "auto", autofix: false },
        "acme/newrepo": { mode: "on_request", autofix: true },
      },
      profile: "pr_reviewer",
      mention: "@reviewer",
      categories: ["functional-correctness", "security"],
      instructions: "",
    });
    expect(setInputs.mock.calls[0]![0].automationId).toBe("b1");
  });

  it("rejects an invalid free-typed key and never adds it", () => {
    render(<InputsTab automationId="b1" />);
    const keyInput = screen.getByLabelText("New Repositories key");
    fireEvent.change(keyInput, { target: { value: "not-a-repo" } });
    fireEvent.keyDown(keyInput, { key: "Enter" });
    expect(screen.getByRole("alert").textContent).toContain("owner/repo");
    expect(screen.queryByTestId("map-row-not-a-repo")).toBeNull();
  });

  it("blocks Save on a required input and shows the error", async () => {
    render(<InputsTab automationId="b1" />);
    fireEvent.click(screen.getByLabelText("Remove engrams/engrams"));
    fireEvent.click(screen.getByRole("button", { name: /save inputs/i }));
    await waitFor(() => expect(screen.getByText("Repositories is required")).toBeTruthy());
    expect(setInputs).not.toHaveBeenCalled();
  });

  it("routes a server error back to its field", async () => {
    setInputs.mockRejectedValueOnce(new Error("inputs.mention: mention must start with @"));
    render(<InputsTab automationId="b1" />);
    fireEvent.change(screen.getByLabelText("Mention"), { target: { value: "reviewer" } });
    fireEvent.click(screen.getByRole("button", { name: /save inputs/i }));
    await waitFor(() => expect(screen.getByText("mention must start with @")).toBeTruthy());
  });

  it("discard restores the stored values", () => {
    render(<InputsTab automationId="b1" />);
    fireEvent.change(screen.getByLabelText("Mention"), { target: { value: "@x" } });
    expect(screen.getByText("Unsaved changes")).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: /discard/i }));
    expect((screen.getByLabelText("Mention") as HTMLInputElement).value).toBe("@engrams");
    expect(screen.queryByText("Unsaved changes")).toBeNull();
  });

  it("explains when the automation declares no inputs", () => {
    automationHolder.value = reviewBuiltin({}, []);
    render(<InputsTab automationId="b1" />);
    expect(screen.getByText(/declares no inputs/)).toBeTruthy();
  });
});
