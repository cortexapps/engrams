import { describe, expect, it } from "vitest";
import { screen } from "@testing-library/react";

import { renderWithProviders } from "../../test-utils";
import { SettingsLayout } from "./SettingsLayout";

describe("SettingsLayout", () => {
  it("lists Automations and no longer lists Reviewed repos (ADR 0119 phase 3.8)", async () => {
    renderWithProviders(<SettingsLayout />);
    expect((await screen.findAllByText("Automations")).length).toBeGreaterThan(0);
    expect(screen.queryByText("Reviewed repos")).toBeNull();
  });
});
