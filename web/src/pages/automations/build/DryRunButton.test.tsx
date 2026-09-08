import { describe, expect, it, vi, beforeEach } from "vitest";
import { fireEvent, screen, waitFor } from "@testing-library/react";

import { renderWithProviders } from "@/test-utils";

import { DryRunButton } from "./DryRunButton";

const navigate = vi.hoisted(() => vi.fn());
const dryRunMutate = vi.hoisted(() => vi.fn());

vi.mock("@tanstack/react-router", async (orig) => ({
  ...(await orig()),
  useNavigate: () => navigate,
}));
vi.mock("@/hooks/useAutomationCode", async (orig) => ({
  ...(await orig<typeof import("@/hooks/useAutomationCode")>()),
  useDryRun: () => ({ mutate: dryRunMutate, isPending: false }),
}));
vi.mock("sonner", () => ({ toast: { success: vi.fn(), error: vi.fn() } }));

describe("DryRunButton", () => {
  beforeEach(() => {
    navigate.mockReset();
    dryRunMutate.mockReset();
  });

  it("starts a dry run and navigates to the returned run", async () => {
    dryRunMutate.mockImplementation((_req, opts: { onSuccess: (r: { runId: string }) => void }) =>
      opts.onSuccess({ runId: "autorun:a1:manual:x" }),
    );
    renderWithProviders(<DryRunButton automationId="a1" sampleId="s9" />);
    fireEvent.click(await screen.findByTestId("dry-run-button"));

    const [request] = dryRunMutate.mock.calls[0]!;
    expect(request).toEqual({ automationId: "a1", sample: { case: "sampleId", value: "s9" } });
    await waitFor(() =>
      expect(navigate).toHaveBeenCalledWith({
        href: "/automations/a1/runs/autorun:a1:manual:x",
      }),
    );
  });

  it("is disabled while the editor is dirty (a dry run executes the saved definition)", async () => {
    renderWithProviders(<DryRunButton automationId="a1" disabled />);
    expect(await screen.findByTestId("dry-run-button")).toHaveProperty("disabled", true);
  });
});
