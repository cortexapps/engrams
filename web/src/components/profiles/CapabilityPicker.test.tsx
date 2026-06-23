// CapabilityPicker (ADR 0057 D1): toggles provider:action from the connector
// catalog, scopes a selection with @resource, and keeps orphans removable.

import { afterEach, describe, expect, test, vi } from "vitest";
import { cleanup, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";

const connectors = [
  {
    provider: "github",
    configJson: JSON.stringify({
      provider: "github",
      operations: [{ grants: ["issues:write"] }, { grants: ["contents:read"] }],
    }),
    builtin: true,
    createdAt: "",
    updatedAt: "",
  },
];

vi.mock("../../hooks/useIntegrations", () => ({
  useConnectors: () => ({ data: { connectors }, isLoading: false }),
}));

import { CapabilityPicker } from "./CapabilityPicker";

describe("CapabilityPicker", () => {
  afterEach(cleanup);

  test("renders a toggle per grantable provider:action from the catalog", () => {
    render(<CapabilityPicker value={[]} onChange={() => {}} />);
    expect(screen.getByLabelText("github:issues:write")).toBeTruthy();
    expect(screen.getByLabelText("github:contents:read")).toBeTruthy();
  });

  test("toggling on adds the capability", async () => {
    const onChange = vi.fn();
    render(<CapabilityPicker value={[]} onChange={onChange} />);
    await userEvent.setup().click(screen.getByLabelText("github:issues:write"));
    expect(onChange).toHaveBeenCalledWith(["github:issues:write"]);
  });

  test("a scoped capability shows its @resource", () => {
    render(<CapabilityPicker value={["github:contents:read@owner/repo"]} onChange={() => {}} />);
    const input = screen.getByLabelText("github:contents:read resource") as HTMLInputElement;
    expect(input.value).toBe("owner/repo");
  });

  test("an orphan capability (no current connector) is shown and removable", async () => {
    const onChange = vi.fn();
    render(<CapabilityPicker value={["stripe:charges:write"]} onChange={onChange} />);
    expect(screen.getByText("stripe:charges:write")).toBeTruthy();
    await userEvent.setup().click(screen.getByLabelText("remove stripe:charges:write"));
    expect(onChange).toHaveBeenCalledWith([]);
  });
});
