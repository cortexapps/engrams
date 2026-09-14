import { describe, expect, it, vi } from "vitest";
import { fireEvent, screen } from "@testing-library/react";

import { renderWithProviders } from "@/test-utils";
import type { BlockDef } from "@/lib/automation-blocks";

import { BlockInspector } from "./BlockInspector";

vi.mock("@/hooks/useProfiles", () => ({
  useProfiles: () => ({
    data: { profiles: [{ id: "pr_reviewer", name: "PR reviewer", harness: "claude" }] },
  }),
}));
vi.mock("@/hooks/useHarnessCatalog", () => ({
  useHarnessCatalog: () => ({ data: [] }),
}));
vi.mock("@/hooks/useModelRouters", () => ({
  useModelRouters: () => ({ data: { routers: [] } }),
  useRouterModels: () => ({ data: { models: [] } }),
}));
vi.mock("@/hooks/useAutomationEditor", () => ({
  useActionCatalog: () => ({
    data: {
      actions: [
        {
          id: "create_issue_comment",
          label: "Create issue comment",
          description: "",
          inputSchemaJson: JSON.stringify({
            type: "object",
            required: ["repo", "number", "body"],
            properties: {
              repo: { type: "string" },
              number: { type: "integer" },
              body: { type: "string" },
            },
          }),
        },
      ],
    },
  }),
  useEventCatalog: () => ({ data: undefined }),
  useEditorWebhookRegistrations: () => ({ data: { registrations: [] } }),
}));

function mount(
  block: BlockDef,
  builtin: boolean,
  errors: Array<{ blockId: string; field: string; message: string }> = [],
) {
  const onChange = vi.fn();
  renderWithProviders(
    <BlockInspector
      block={block}
      onChange={onChange}
      builtin={builtin}
      errors={errors}
      sessionSources={["launch"]}
      variablePaths={["trigger.kind", "steps.launch.session_id"]}
    />,
  );
  return { onChange };
}

describe("BlockInspector", () => {
  it("dispatches a generic-form kind and writes dotted config paths", async () => {
    const { onChange } = mount(
      {
        id: "ping",
        type: "send_prompt",
        config: {
          session: { blockId: "launch" },
          promptTemplate: "hi",
          waitFor: { kind: "run_end" },
        },
      },
      false,
    );
    const textarea = (await screen.findByTestId("field-promptTemplate")).querySelector("textarea")!;
    fireEvent.change(textarea, { target: { value: "hello" } });
    expect(onChange).toHaveBeenLastCalledWith(
      expect.objectContaining({ config: expect.objectContaining({ promptTemplate: "hello" }) }),
    );
  });

  it("on a built-in renders tunable fields editable and pinned fields read-only with the hint", async () => {
    mount(
      {
        id: "finder",
        type: "create_session",
        tunable: ["promptTemplate"],
        config: { profileId: "pr_reviewer", promptTemplate: "Review it.", role: "finder" },
      },
      true,
    );
    // Editable: promptTemplate.
    const prompt = (await screen.findByTestId("field-promptTemplate")).querySelector("textarea")!;
    expect(prompt.disabled).toBe(false);
    // Pinned: role (string field) shows the hint and is disabled.
    const role = screen.getByTestId("field-role");
    expect(role.querySelector("input")!.disabled).toBe(true);
    expect(role.textContent).toContain("Set by the built-in");
    expect(screen.getByText(/Editable here: promptTemplate/)).toBeTruthy();
  });

  it("routes a BlockError to its field", async () => {
    mount(
      {
        id: "ping",
        type: "send_prompt",
        config: {
          session: { blockId: "launch" },
          promptTemplate: "",
          waitFor: { kind: "run_end" },
        },
      },
      false,
      [{ blockId: "ping", field: "promptTemplate", message: "template is required" }],
    );
    const field = await screen.findByTestId("field-promptTemplate");
    expect(field.getAttribute("data-invalid")).toBe("true");
    expect(field.textContent).toContain("template is required");
  });

  it("renders an unknown block kind read-only", async () => {
    mount({ id: "mystery", type: "not_a_kind_yet", config: { status: "completed" } }, true);
    expect(await screen.findByText("Configuration (read-only)")).toBeTruthy();
  });

  it("integration_action derives param fields from the action's input schema", async () => {
    mount(
      {
        id: "ack",
        type: "integration_action",
        config: { provider: "github", actionId: "create_issue_comment", params: {} },
      },
      false,
    );
    expect(await screen.findByTestId("field-params.repo")).toBeTruthy();
    expect(screen.getByTestId("field-params.number")).toBeTruthy();
    expect(screen.getByTestId("field-params.body").querySelector("textarea")).not.toBeNull();
  });

  it("integration_action numeric params are written as numbers, never strings", async () => {
    // The action validates params at RUN time and rejects a numeric string.
    const block: BlockDef = {
      id: "ack",
      type: "integration_action",
      config: { provider: "github", actionId: "create_issue_comment", params: {} },
    };
    const { onChange } = mount(block, false);
    const field = await screen.findByTestId("field-params.number");
    const input = field.querySelector("input")!;

    fireEvent.change(input, { target: { value: "42" } });
    const written = onChange.mock.calls.at(-1)![0] as BlockDef;
    expect((written.config["params"] as Record<string, unknown>)["number"]).toBe(42);

    // A half-typed / non-numeric value stays local with an inline error and
    // never reaches the config as NaN.
    onChange.mockClear();
    fireEvent.change(input, { target: { value: "4x" } });
    expect(onChange).not.toHaveBeenCalled();
    expect(field.getAttribute("data-invalid")).toBe("true");
    expect(screen.getByText("Enter a whole number")).toBeTruthy();

    // A template is accepted verbatim (it renders to a number before validation).
    fireEvent.change(input, { target: { value: "${{ event.pr.number }}" } });
    const templated = onChange.mock.calls.at(-1)![0] as BlockDef;
    expect((templated.config["params"] as Record<string, unknown>)["number"]).toBe(
      "${{ event.pr.number }}",
    );

    // Clearing removes the key (required-ness is the schema's call).
    fireEvent.change(input, { target: { value: "" } });
    const cleared = onChange.mock.calls.at(-1)![0] as BlockDef;
    expect("number" in (cleared.config["params"] as Record<string, unknown>)).toBe(false);
  });
});
