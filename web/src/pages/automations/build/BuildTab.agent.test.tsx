/** The automation's agent on the Build tab: one node under every way in,
 * a stub per way in saying what it does with the agent, the agent's own
 * inspector, and the dashed "Add an agent" node when there is none. */

import { describe, expect, it, vi } from "vitest";
import { fireEvent, screen, within } from "@testing-library/react";

import { renderWithProviders } from "@/test-utils";
import type { AutomationDefinition } from "@/lib/automation-blocks";

import { BuildTab } from "./BuildTab";

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
  useActionCatalog: () => ({ data: undefined }),
  useEventCatalog: () => ({ data: undefined }),
  useEditorWebhookRegistrations: () => ({ data: { registrations: [] } }),
}));

const withAgent: AutomationDefinition = {
  engine: 1,
  trigger: { kind: "manual" },
  blocks: [
    { id: "admit", type: "filter", config: { conditions: { mode: "all", conditions: [] } } },
  ],
  entrypoints: [{ id: "done", trigger: { kind: "manual" }, blocks: [], role: "close" }],
  agent: {
    scope: {
      kind: "key",
      keyTemplate: "${{ event.raw.repository.full_name }}#${{ event.raw.pull_request.number }}",
    },
    profileId: "pr_reviewer",
    promptTemplate: "Review it.",
  },
  inputsSchema: [],
  settings: { endSessionsOnFinish: false },
};

const withoutAgent: AutomationDefinition = {
  engine: 1,
  trigger: { kind: "manual" },
  blocks: [],
  inputsSchema: [],
  settings: { endSessionsOnFinish: false },
};

function mount(definition: AutomationDefinition, builtin = false) {
  const onChange = vi.fn();
  const onSelectEntrypoint = vi.fn();
  renderWithProviders(
    <BuildTab
      definition={definition}
      entrypointId="main"
      onSelectEntrypoint={onSelectEntrypoint}
      onChange={onChange}
      builtin={builtin}
      errors={[]}
      triggerSummaryFor={() => "Manual"}
    />,
  );
  return { onChange, onSelectEntrypoint };
}

describe("BuildTab agent", () => {
  it("draws one agent node under every way in, each with its role on the stub", async () => {
    mount(withAgent);
    const node = await screen.findByTestId("agent-node");
    expect(screen.getAllByTestId("agent-node")).toHaveLength(1);
    expect(node.textContent).toContain("PR reviewer");
    expect(node.textContent).toContain("per PR");
    const stubs = screen.getAllByTestId("agent-stub");
    expect(stubs.map((s) => s.getAttribute("data-role"))).toEqual(["start", "close"]);
    expect(stubs[0]!.textContent).toBe("tells it");
    expect(stubs[1]!.textContent).toBe("closes it");
  });

  it("a way in's message shows on its stub; the stub opens the way in, whose inspector edits it", async () => {
    const { onChange } = mount({
      ...withAgent,
      promptTemplate: "Review ${{ steps.admit.passed }}\nsecond line",
    });
    const stub = (await screen.findAllByTestId("agent-stub"))[0]!;
    expect(screen.getByTestId("agent-stub-message").textContent).toBe(
      "Review ${{ steps.admit.passed }}",
    );
    fireEvent.click(within(stub).getByRole("button"));
    const message = (await screen.findByLabelText("Message to the agent")) as HTMLTextAreaElement;
    expect(message.value).toBe("Review ${{ steps.admit.passed }}\nsecond line");
    fireEvent.change(message, { target: { value: "Review it" } });
    expect((onChange.mock.calls.at(-1)![0] as AutomationDefinition).promptTemplate).toBe(
      "Review it",
    );
  });

  it("a click on the node opens the agent's inspector; the trigger's inspector offers the role", async () => {
    mount(withAgent);
    fireEvent.click(await screen.findByTestId("agent-node"));
    expect(await screen.findByTestId("agent-inspector")).toBeTruthy();
    expect(screen.getByTestId("agent-node").getAttribute("aria-current")).toBe("true");
    expect(screen.getByRole("button", { name: "Remove the agent" })).toBeTruthy();

    fireEvent.click(screen.getAllByTestId("block-row-trigger")[0]!);
    expect(await screen.findByTestId("field-role")).toBeTruthy();
  });

  it("removing the agent drops it and every way in's role and message", async () => {
    const { onChange } = mount({
      ...withAgent,
      promptTemplate: "Hi",
      entrypoints: [{ ...withAgent.entrypoints![0]!, role: "start", promptTemplate: "Bye" }],
    });
    fireEvent.click(await screen.findByTestId("agent-node"));
    fireEvent.click(await screen.findByRole("button", { name: "Remove the agent" }));
    const next = onChange.mock.calls.at(-1)![0] as AutomationDefinition;
    expect("agent" in next).toBe(false);
    expect("role" in next).toBe(false);
    expect("promptTemplate" in next).toBe(false);
    expect(next.entrypoints?.[0] && "role" in next.entrypoints[0]).toBe(false);
    expect(next.entrypoints?.[0] && "promptTemplate" in next.entrypoints[0]).toBe(false);
  });

  it("with no agent the canvas offers to add one, scoped by the trigger", async () => {
    const { onChange } = mount(withoutAgent);
    expect(screen.queryByTestId("agent-node")).toBeNull();
    fireEvent.click(await screen.findByTestId("add-agent"));
    expect(onChange).toHaveBeenLastCalledWith(
      expect.objectContaining({
        agent: { scope: { kind: "event" }, profileId: "", promptTemplate: "" },
      }),
    );
  });

  it("a built-in without an agent offers nothing", async () => {
    mount(withoutAgent, true);
    await screen.findAllByTestId("block-row-trigger");
    expect(screen.queryByTestId("add-agent")).toBeNull();
    expect(screen.queryByTestId("agent-node")).toBeNull();
  });
});
