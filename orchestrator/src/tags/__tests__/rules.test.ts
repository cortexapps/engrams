import { describe, expect, test } from "bun:test";

import { normalizeTagColor, normalizeTagDescription, normalizeTagName } from "../rules.ts";

describe("normalizeTagName", () => {
  test("trims, collapses inner whitespace to one space, and keeps the case", () => {
    expect(normalizeTagName("  Team   Platform \t")).toBe("Team Platform");
    expect(normalizeTagName("a\tb")).toBe("a b");
  });

  test("allows 32 characters and refuses 33, counting characters, not UTF-16 units", () => {
    expect(normalizeTagName("a".repeat(32))).toBe("a".repeat(32));
    expect(normalizeTagName("é".repeat(32))).toBe("é".repeat(32));
    expect(normalizeTagName("🙂".repeat(32))).toBe("🙂".repeat(32));
    expect(normalizeTagName("a".repeat(33))).toBeNull();
  });

  test("refuses empty text and control characters", () => {
    expect(normalizeTagName("")).toBeNull();
    expect(normalizeTagName("   ")).toBeNull();
    expect(normalizeTagName("a\u0000b")).toBeNull();
    expect(normalizeTagName("a\u0007b")).toBeNull();
  });
});

describe("normalizeTagDescription", () => {
  test("trims and allows an empty description", () => {
    expect(normalizeTagDescription("  Owned by platform ")).toBe("Owned by platform");
    expect(normalizeTagDescription("")).toBe("");
    expect(normalizeTagDescription("   ")).toBe("");
  });

  test("allows 100 characters and refuses 101", () => {
    expect(normalizeTagDescription("a".repeat(100))).toBe("a".repeat(100));
    expect(normalizeTagDescription("a".repeat(101))).toBeNull();
  });

  test("refuses line breaks and other control characters", () => {
    expect(normalizeTagDescription("a\nb")).toBeNull();
    expect(normalizeTagDescription("a\r\nb")).toBeNull();
    expect(normalizeTagDescription("a\u0000b")).toBeNull();
  });
});

describe("normalizeTagColor", () => {
  test("accepts #rrggbb, trimmed and lowercased", () => {
    expect(normalizeTagColor("#1F6FEB")).toBe("#1f6feb");
    expect(normalizeTagColor("#000000")).toBe("#000000");
    expect(normalizeTagColor(" #1F6FEB ")).toBe("#1f6feb");
  });

  test("refuses every other form", () => {
    for (const bad of ["1f6feb", "#1f6", "#1f6febff", "#zzzzzz", "", "rgb(0,0,0)"]) {
      expect(normalizeTagColor(bad)).toBeNull();
    }
  });
});
