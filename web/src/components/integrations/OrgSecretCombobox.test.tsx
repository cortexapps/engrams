// Interaction contract for OrgSecretCombobox — the org-secret ref typeahead
// used by the profile secrets editor and the image-pull-secret field.
//
// The pure helpers around it were covered; the click path was not, which is how
// "picking a secret does nothing" could ship. These pin the two ways a ref gets
// committed (pick an existing name, create a new one) and that the trigger then
// shows it.

import { afterEach, describe, expect, test, vi } from "vitest";
import { cleanup, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";

import { OrgSecretCombobox } from "./OrgSecretCombobox";

afterEach(cleanup);

const NAMES = ["OPENROUTER_API_KEY", "DATADOG_API_KEY"];

describe("OrgSecretCombobox", () => {
  test("picking a listed secret commits it", async () => {
    const user = userEvent.setup();
    const onChange = vi.fn();
    render(<OrgSecretCombobox value="" onChange={onChange} secretNames={NAMES} />);

    await user.click(screen.getByRole("combobox"));
    await user.click(await screen.findByText("OPENROUTER_API_KEY"));

    expect(onChange).toHaveBeenCalledWith("OPENROUTER_API_KEY");
  });

  test("typing an unknown name commits it as a new secret", async () => {
    const user = userEvent.setup();
    const onChange = vi.fn();
    render(<OrgSecretCombobox value="" onChange={onChange} secretNames={NAMES} />);

    await user.click(screen.getByRole("combobox"));
    await user.type(screen.getByPlaceholderText("Search or create…"), "NEW_KEY");
    await user.click(await screen.findByText(/Create new secret/));

    expect(onChange).toHaveBeenCalledWith("NEW_KEY");
  });

  test("the trigger shows the committed ref", () => {
    render(<OrgSecretCombobox value="DATADOG_API_KEY" onChange={vi.fn()} secretNames={NAMES} />);
    expect(screen.getByRole("combobox").textContent).toContain("DATADOG_API_KEY");
  });
});
