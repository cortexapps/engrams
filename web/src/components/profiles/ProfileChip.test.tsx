import { describe, it, expect } from "vitest";
import { render, screen } from "@testing-library/react";
import { ProfileChip } from "./ProfileChip";

describe("ProfileChip", () => {
  it("renders the profile name + glyph", () => {
    render(
      <ProfileChip
        profile={{
          id: "p",
          name: "Backend",
          icon: "Server",
          archived: false,
          imageUri: "registry/api:latest",
          skills: [],
        }}
      />,
    );
    expect(screen.getByText("Backend")).toBeTruthy();
  });
  it("falls back to the image string when profile-less", () => {
    render(<ProfileChip profile={null} fallbackImage="registry/api:latest" />);
    expect(screen.getByText(/api:latest/)).toBeTruthy();
  });
  it("marks archived profiles", () => {
    render(
      <ProfileChip
        profile={{ id: "p", name: "Old", icon: "Bot", archived: true, imageUri: "x", skills: [] }}
      />,
    );
    expect(screen.getByText("Old")).toBeTruthy();
    expect(screen.getByText(/archived/i)).toBeTruthy();
  });
});
