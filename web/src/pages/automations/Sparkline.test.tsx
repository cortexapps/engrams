import { describe, it, expect } from "vitest";
import { render } from "@testing-library/react";
import { create } from "@bufbuild/protobuf";

import { DayRunCountSchema, type DayRunCount } from "@/gen/engram/app/v1/automation_pb";
import { dayTone, dayTotal, sevenDayWindow, Sparkline } from "./Sparkline";

// A pinned clock: "today" is 2026-08-21 UTC, so the window is 08-15..08-21.
const NOW = new Date("2026-08-21T15:30:00Z");
const D = (offset: number) => `2026-08-${String(15 + offset).padStart(2, "0")}`;

function day(completed: number, failed = 0, filtered = 0, label = D(6), other = 0): DayRunCount {
  return create(DayRunCountSchema, { day: label, completed, failed, filtered, other });
}

describe("dayTone", () => {
  it("maps a day's mix onto the instrument tones", () => {
    expect(dayTone(day(0))).toBe("muted");
    expect(dayTone(day(3))).toBe("nominal");
    expect(dayTone(day(0, 0, 2))).toBe("nominal"); // filtered-only is not a failure
    expect(dayTone(day(2, 1))).toBe("critical");
    expect(dayTone(day(0, 2))).toBe("critical");
  });

  it("an other-only day (superseded / in flight) is activity, not a zero stub", () => {
    // A superseding automation can have whole days of superseded runs.
    expect(dayTone(day(0, 0, 0, D(6), 5))).toBe("nominal");
    expect(dayTotal(day(1, 0, 0, D(6), 4))).toBe(5);
    const { container } = render(<Sparkline days={[day(0, 0, 0, D(6), 5)]} now={NOW} />);
    const today = container.querySelectorAll("rect")[6]!;
    expect(today.getAttribute("data-tone")).toBe("nominal");
    expect(Number(today.getAttribute("height"))).toBeGreaterThan(2);
  });
});

describe("sevenDayWindow", () => {
  it("zero-fills the last seven UTC dates and places each reported bucket on its calendar day", () => {
    // The server only emits days that had runs: here, two days ago and today.
    const sparse = [day(2, 0, 0, D(4)), day(5, 0, 0, D(6))];
    const week = sevenDayWindow(sparse, NOW);
    expect(week.map((d) => d.day)).toEqual([D(0), D(1), D(2), D(3), D(4), D(5), D(6)]);
    expect(week.map((d) => d.completed)).toEqual([0, 0, 0, 0, 2, 0, 5]);
  });

  it("ignores buckets outside the window and tolerates an empty report", () => {
    const stale = [day(9, 0, 0, "2026-08-01")];
    expect(sevenDayWindow(stale, NOW).every((d) => d.completed === 0)).toBe(true);
    expect(sevenDayWindow([], NOW)).toHaveLength(7);
  });
});

describe("Sparkline", () => {
  it("renders seven calendar-aligned bars, newest on the right, from the server's sparse output", () => {
    // One run six days ago, a mixed day two days ago, a busy day today.
    const sparse = [day(1, 0, 0, D(0)), day(2, 1, 0, D(4)), day(8, 0, 0, D(6))];
    const { container } = render(<Sparkline days={sparse} now={NOW} />);
    const bars = [...container.querySelectorAll("rect")];
    expect(bars).toHaveLength(7);
    expect(bars.map((b) => b.getAttribute("data-tone"))).toEqual([
      "nominal", // D0: the run six days ago sits on the LEFT
      "muted",
      "muted",
      "muted",
      "critical", // D4: two days ago — a gap-separated slot, not adjacent to D0
      "muted",
      "nominal", // D6: today, on the RIGHT
    ]);
    // Zero days keep a 2px stub; the busiest day fills the full height.
    expect(bars[1]!.getAttribute("height")).toBe("2");
    expect(bars[6]!.getAttribute("height")).toBe("18");
    expect(bars[0]!.getAttribute("width")).toBe("5");
    expect(bars.map((bar) => bar.getAttribute("x"))).toEqual([
      "0",
      "7",
      "14",
      "21",
      "28",
      "35",
      "42",
    ]);
    expect(bars[1]!.getAttribute("class")).toContain("fill-border");
    expect(bars[4]!.getAttribute("class")).toContain("instrument-critical");
  });

  it("a single run two days ago renders as one bar in its slot, not at the far left", () => {
    const { container } = render(<Sparkline days={[day(1, 0, 0, D(4))]} now={NOW} />);
    const bars = [...container.querySelectorAll("rect")];
    expect(bars).toHaveLength(7);
    expect(bars.findIndex((b) => b.getAttribute("data-tone") === "nominal")).toBe(4);
  });
});
