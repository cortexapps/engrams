import { describe, expect, it, vi } from "vitest";
import { fireEvent, screen } from "@testing-library/react";

import { renderWithProviders } from "@/test-utils";
import type { TriggerSpec } from "@/lib/automation-blocks";

import { TriggerInspector } from "./TriggerInspector";

vi.mock("@/hooks/useAutomationEditor", () => ({
  useActionCatalog: () => ({ data: undefined }),
  useEventCatalog: () => ({
    data: {
      events: [
        {
          key: "pull_request.opened",
          label: "Pull request opened",
          description: "",
          observed: true,
        },
      ],
      scope: { key: "repositories", label: "Repositories" },
      variables: [],
      defaultConnectionId: "conn-github",
    },
  }),
  useEditorWebhookRegistrations: () => ({ data: { registrations: [] } }),
}));
// Radix Select needs pointer APIs jsdom lacks; render it as a plain control so
// the disabled state is observable.
vi.mock("@/components/ui/select", () => ({
  Select: ({ children, disabled }: { children: React.ReactNode; disabled?: boolean }) => (
    <div data-testid="select" data-disabled={disabled ? "true" : undefined}>
      {children}
    </div>
  ),
  SelectTrigger: ({ children }: { children: React.ReactNode }) => <div>{children}</div>,
  SelectValue: () => null,
  SelectContent: ({ children }: { children: React.ReactNode }) => <div>{children}</div>,
  SelectItem: ({ children }: { children: React.ReactNode }) => <div>{children}</div>,
}));

const reviewTrigger: TriggerSpec = {
  kind: "integration",
  provider: "github",
  connectionId: "conn-github",
  eventKeys: ["pull_request.opened"],
  scope: { fromInput: "repos" },
};

describe("TriggerInspector", () => {
  it("on a built-in the scope is pinned too — no control is editable, and the hint points at the Inputs tab", async () => {
    const onChange = vi.fn();
    renderWithProviders(
      <TriggerInspector trigger={reviewTrigger} onChange={onChange} builtin errors={[]} />,
    );
    const scope = await screen.findByTestId("field-trigger.scope");
    // The mode select and the from-input field are both disabled.
    expect(scope.querySelector('[data-testid="select"]')?.getAttribute("data-disabled")).toBe(
      "true",
    );
    const input = scope.querySelector("input") as HTMLInputElement;
    expect(input.disabled).toBe(true);
    expect(scope.textContent).toContain("Set by the built-in");
    expect(scope.textContent).toContain("Inputs tab");
    // The event switches are pinned as well.
    const eventSwitch = screen.getByLabelText("Pull request opened") as HTMLButtonElement;
    expect(eventSwitch.disabled).toBe(true);
    expect(onChange).not.toHaveBeenCalled();
  });

  it("on a user automation the scope is editable", async () => {
    const onChange = vi.fn();
    renderWithProviders(
      <TriggerInspector
        trigger={{ ...reviewTrigger, scope: { values: ["acme/repo"] } }}
        onChange={onChange}
        builtin={false}
        errors={[]}
      />,
    );
    const scope = await screen.findByTestId("field-trigger.scope");
    const input = scope.querySelector("input") as HTMLInputElement;
    expect(input.disabled).toBe(false);
    fireEvent.change(input, { target: { value: "acme/repo, acme/other" } });
    expect(onChange).toHaveBeenLastCalledWith(
      expect.objectContaining({ scope: { values: ["acme/repo", "acme/other"] } }),
    );
  });
});
