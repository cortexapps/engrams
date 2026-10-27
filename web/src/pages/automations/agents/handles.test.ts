import { describe, expect, it } from "vitest";

import { parseHandle } from "./handles";

describe("parseHandle", () => {
  it("turns a GitHub pull request handle into a link", () => {
    expect(parseHandle("github:engrams/engrams#1248")).toEqual({
      provider: "github",
      label: "engrams/engrams#1248",
      href: "https://github.com/engrams/engrams/pull/1248",
    });
  });

  it("names Slack channels and threads without inventing a workspace link", () => {
    expect(parseHandle("slack:C012345:1724.100")).toEqual({
      provider: "slack",
      label: "#C012345 · thread",
    });
    expect(parseHandle("slack:C012345")).toEqual({
      provider: "slack",
      label: "#C012345",
    });
  });

  it("keeps an unknown handle intact", () => {
    expect(parseHandle("linear:ENG-42")).toEqual({
      provider: "linear",
      label: "linear:ENG-42",
    });
  });
});
