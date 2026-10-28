import { describe, expect, it, vi } from "vitest";
import { fireEvent, screen } from "@testing-library/react";

import { renderWithProviders } from "@/test-utils";
import type { AgentSpec } from "@/lib/automation-blocks";

import { AgentInspector } from "./AgentInspector";

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
// Radix Select needs pointer APIs jsdom lacks; render it as a plain control
// whose items are buttons, so a pick is a click.
vi.mock("@/components/ui/select", () => ({
  Select: ({
    children,
    disabled,
    onValueChange,
    value,
  }: {
    children: React.ReactNode;
    disabled?: boolean;
    onValueChange?: (v: string) => void;
    value?: string;
  }) => (
    <div
      data-testid="select"
      data-value={value}
      data-disabled={disabled ? "true" : undefined}
      onClick={(e) => {
        const picked = (e.target as HTMLElement).getAttribute("data-option");
        if (picked && !disabled) onValueChange?.(picked);
      }}
    >
      {children}
    </div>
  ),
  SelectTrigger: ({ children }: { children: React.ReactNode }) => <div>{children}</div>,
  SelectValue: () => null,
  SelectContent: ({ children }: { children: React.ReactNode }) => <div>{children}</div>,
  SelectItem: ({ children, value }: { children: React.ReactNode; value: string }) => (
    <button type="button" data-option={value}>
      {children}
    </button>
  ),
}));

function mount(agent: AgentSpec, builtin = false, slackTrigger = true) {
  const onChange = vi.fn();
  renderWithProviders(
    <AgentInspector
      agent={agent}
      onChange={onChange}
      builtin={builtin}
      errors={[]}
      variablePaths={["trigger.kind", "event.raw"]}
      slackTrigger={slackTrigger}
    />,
  );
  return { onChange };
}

const base: AgentSpec = {
  scope: { kind: "event" },
  profileId: "pr_reviewer",
  promptTemplate: "Answer it.",
};

describe("AgentInspector", () => {
  it("a Slack reply switch writes the thread relay; off removes it", async () => {
    const { onChange } = mount(base);
    await screen.findByTestId("agent-inspector");
    const relay = screen.getByRole("switch", {
      name: /reply in the slack thread/i,
    }) as HTMLButtonElement;
    expect(relay.getAttribute("aria-checked")).toBe("false");
    fireEvent.click(relay);
    expect(onChange).toHaveBeenLastCalledWith(
      expect.objectContaining({
        relay: expect.objectContaining({
          provider: "slack",
          channel: "${{ event.raw.event.channel }}",
          threadTs: "${{ event.raw.event.thread_ts | default: event.raw.event.ts }}",
        }),
      }),
    );
    // The six templates stay behind a disclosure until asked for.
    expect(screen.queryByTestId("field-relay.channel")).toBeNull();
  });

  it("a relay already set shows on, and off removes it", async () => {
    const { onChange } = mount({
      ...base,
      relay: { provider: "slack", team: "${{ event.raw.team_id }}", channel: "C1" },
    });
    const relay = await screen.findByRole("switch", { name: /reply in the slack thread/i });
    expect(relay.getAttribute("aria-checked")).toBe("true");
    fireEvent.click(screen.getByText("Where the relay reads the thread from"));
    expect(await screen.findByTestId("field-relay.channel")).toBeTruthy();
    fireEvent.click(relay);
    const last = onChange.mock.calls.at(-1)![0] as AgentSpec;
    expect("relay" in last).toBe(false);
  });

  it("without a Slack way in the relay switch is hidden unless a relay is set", async () => {
    mount(base, false, false);
    await screen.findByTestId("field-scope");
    expect(screen.queryByTestId("field-relay")).toBeNull();
  });

  it("the session is kept by default: an absent keepOnFinish reads as on", async () => {
    mount(base);
    fireEvent.click(await screen.findByText("More"));
    const keep = await screen.findByTestId("field-keepOnFinish");
    expect(keep.querySelector('[role="switch"]')?.getAttribute("aria-checked")).toBe("true");
  });

  it("the scope is a named preset; a preset writes its key, a custom key shows the templates", async () => {
    const { onChange } = mount(base);
    const scope = await screen.findByTestId("field-scope");
    expect(scope.querySelector('[data-testid="select"]')?.getAttribute("data-value")).toBe("event");
    expect(screen.queryByTestId("scope-fields")).toBeNull();
    fireEvent.click(screen.getByText("Each Slack thread"));
    expect(onChange).toHaveBeenLastCalledWith(
      expect.objectContaining({
        scope: expect.objectContaining({
          kind: "key",
          keyTemplate: expect.stringContaining('coalesce: "thread_ts", "ts"'),
        }),
      }),
    );
  });

  it("a key of its own opens the key templates", async () => {
    mount({
      ...base,
      scope: { kind: "key", keyTemplate: "${{ event.raw.data.id }}" },
    });
    const scope = await screen.findByTestId("field-scope");
    expect(scope.querySelector('[data-testid="select"]')?.getAttribute("data-value")).toBe(
      "custom",
    );
    expect(await screen.findByTestId("field-scope.keyTemplate")).toBeTruthy();
  });

  it("on a built-in every field is pinned", async () => {
    const { onChange } = mount(base, true);
    const scope = await screen.findByTestId("field-scope");
    expect(scope.querySelector('[data-testid="select"]')?.getAttribute("data-disabled")).toBe(
      "true",
    );
    expect(scope.textContent).toContain("Set by the built-in");
    fireEvent.click(screen.getByText("The whole automation"));
    expect(onChange).not.toHaveBeenCalled();
  });
});
