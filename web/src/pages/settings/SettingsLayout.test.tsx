import { describe, expect, it } from "vitest";
import { screen } from "@testing-library/react";

import { renderWithProviders } from "../../test-utils";
import { SettingsLayout } from "./SettingsLayout";
import { isSettingsItemActive, visibleSettingsGroups } from "./SettingsRail";

describe("SettingsLayout", () => {
  it("lists the infrastructure and feedback pages that retired into Settings", async () => {
    renderWithProviders(<SettingsLayout />);
    // The rail AND the mobile strip both render the label; either is proof.
    expect((await screen.findAllByText("Fleet")).length).toBeGreaterThan(0);
    expect(screen.getAllByText("Storage").length).toBeGreaterThan(0);
    expect(screen.getAllByText("Images").length).toBeGreaterThan(0);
    expect(screen.getAllByText("Papercuts").length).toBeGreaterThan(0);
    expect(screen.getAllByText("Session profiles").length).toBeGreaterThan(0);
    // Automations is a spine product, not a settings page (ADR 0119 phase 3.8
    // retired Reviewed repos before that).
    expect(screen.queryByText("Automations")).toBeNull();
    expect(screen.queryByText("Reviewed repos")).toBeNull();
  });

  it("shows a member their own pages and the feedback they could always read", () => {
    const groups = visibleSettingsGroups(false);
    expect(groups.map((g) => g.label)).toEqual(["You", "Feedback"]);
    expect(groups[0]!.items.map((it) => it.label)).toEqual(["Profile", "Credentials"]);
  });

  it("shows an admin all five groups", () => {
    expect(visibleSettingsGroups(true).map((g) => g.label)).toEqual([
      "You",
      "Workspace",
      "Runtime",
      "Infrastructure",
      "Feedback",
    ]);
  });

  it("matches a rail row by segment, so Profile does not light Session profiles", () => {
    expect(isSettingsItemActive("/settings/profiles", "/settings/profile")).toBe(false);
    expect(isSettingsItemActive("/settings/profiles/new", "/settings/profiles")).toBe(true);
    expect(isSettingsItemActive("/settings/profile", "/settings/profile")).toBe(true);
  });
});
