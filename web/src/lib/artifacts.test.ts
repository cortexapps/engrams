import { describe, expect, it } from "vitest";

import { artifactBytesUrl, artifactPageUrl, extensionOf, mediaKind, slugify } from "./artifacts";

describe("mediaKind", () => {
  it("classifies the artifact + media types", () => {
    expect(mediaKind("text/html")).toBe("html");
    expect(mediaKind("image/svg+xml")).toBe("html"); // sandboxed like HTML
    expect(mediaKind("text/markdown")).toBe("markdown");
    expect(mediaKind("image/png")).toBe("image");
    expect(mediaKind("video/mp4")).toBe("video");
    expect(mediaKind("audio/mpeg")).toBe("audio");
    expect(mediaKind("text/plain")).toBe("text");
    expect(mediaKind("application/json")).toBe("text");
    expect(mediaKind("application/pdf")).toBe("binary");
    expect(mediaKind("application/octet-stream")).toBe("binary");
  });
});

describe("urls", () => {
  it("builds byte urls with optional version/token/theme", () => {
    expect(artifactBytesUrl("a1")).toBe("/api/v1/artifacts/a1");
    expect(artifactBytesUrl("a1", { v: 2 })).toBe("/api/v1/artifacts/a1?v=2");
    expect(artifactBytesUrl("a1", { token: "t.k", theme: "dark" })).toBe(
      "/api/v1/artifacts/a1?token=t.k&theme=dark",
    );
  });

  it("builds the stable page url with a pretty suffix", () => {
    expect(artifactPageUrl("a1")).toBe("/artifacts/a1");
    expect(artifactPageUrl("a1", "Q3 Capacity Report!")).toBe("/artifacts/a1/q3-capacity-report");
  });

  it("slugify + extensionOf behave on edge cases", () => {
    expect(slugify("--Weird  __ name--")).toBe("weird-name");
    expect(extensionOf("report.html")).toBe("html");
    expect(extensionOf("noext")).toBe("");
    expect(extensionOf(".hidden")).toBe("");
  });
});
