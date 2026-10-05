import { describe, it, expect, vi, beforeEach } from "vitest";
import { screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { renderWithProviders } from "../../test-utils";
import { Retention } from "./Retention";

const save = vi.hoisted(() => vi.fn().mockResolvedValue({}));
const state = vi.hoisted(() => ({ runDetailDays: 30 }));

vi.mock("../../hooks/useRetention", () => ({
  useRetentionPolicy: () => ({
    data: { policy: { runDetailDays: state.runDetailDays } },
    isPending: false,
    error: null,
  }),
  useSetRetentionPolicy: () => ({ mutateAsync: save, isPending: false }),
}));

beforeEach(() => {
  save.mockClear();
  state.runDetailDays = 30;
});

describe("Retention", () => {
  it("shows the stored run-detail retention and saves a new value", async () => {
    renderWithProviders(<Retention />);
    const input = (await screen.findByLabelText(/keep run details/i)) as HTMLInputElement;
    expect(input.value).toBe("30");
    const user = userEvent.setup();
    await user.clear(input);
    await user.type(input, "90");
    await user.click(screen.getByRole("button", { name: /^save$/i }));
    await waitFor(() => expect(save).toHaveBeenCalledWith({ policy: { runDetailDays: 90 } }));
  });

  it("refuses a value outside the bounds and keeps Save disabled", async () => {
    renderWithProviders(<Retention />);
    const input = await screen.findByLabelText(/keep run details/i);
    const user = userEvent.setup();
    await user.clear(input);
    await user.type(input, "3");
    expect(screen.getByRole("alert").textContent).toMatch(/between 7 and 365/i);
    expect((screen.getByRole("button", { name: /^save$/i }) as HTMLButtonElement).disabled).toBe(
      true,
    );
    expect(save).not.toHaveBeenCalled();
  });
});
