import { describe, expect, it } from "vitest";

import { relativeAge, relativeTime } from "./relative-time";

const NOW = new Date("2026-08-22T12:00:00Z").getTime();

describe("relativeTime", () => {
  it("is symmetric for past and future (next-fire renders 'in Xm', never 'just now')", () => {
    expect(relativeTime("2026-08-22T11:56:00Z", NOW)).toBe("4m ago");
    expect(relativeTime("2026-08-22T12:04:00Z", NOW)).toBe("in 4m");
    expect(relativeTime("2026-08-22T15:00:00Z", NOW)).toBe("in 3h");
    expect(relativeTime("2026-08-24T12:00:00Z", NOW)).toBe("in 2d");
    expect(relativeTime("2026-08-22T12:00:30Z", NOW)).toBe("any moment");
    expect(relativeTime("2026-08-22T11:59:30Z", NOW)).toBe("just now");
  });

  it("names the absent and the ancient", () => {
    expect(relativeTime(undefined, NOW)).toBe("never");
    expect(relativeTime(null, NOW)).toBe("never");
    expect(relativeTime("2026-06-01T00:00:00Z", NOW)).toBe("2026-06-01");
    expect(relativeTime("not a date", NOW)).toBe("not a date");
  });
});

describe("relativeAge", () => {
  it("is terse, past-only, and floors each unit", () => {
    expect(relativeAge("2026-08-22T11:59:48Z", NOW)).toBe("12s");
    expect(relativeAge("2026-08-22T11:56:30Z", NOW)).toBe("3m");
    expect(relativeAge("2026-08-22T09:10:00Z", NOW)).toBe("2h");
    expect(relativeAge("2026-08-17T12:00:00Z", NOW)).toBe("5d");
  });

  it("has no sentence for a missing or future timestamp", () => {
    expect(relativeAge(null, NOW)).toBe("—");
    expect(relativeAge(undefined, NOW)).toBe("—");
    // A clock skew into the future clamps to zero rather than going negative.
    expect(relativeAge("2026-08-22T12:00:30Z", NOW)).toBe("0s");
  });
});
