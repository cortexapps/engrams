import { describe, it, expect } from "vitest";
import { render } from "@testing-library/react";
import { create } from "@bufbuild/protobuf";

import { DayRunCountSchema, type DayRunCount } from "@/gen/engram/app/v1/automation_pb";
import { dayTone, Sparkline } from "./Sparkline";

function day(completed: number, failed = 0, filtered = 0, label = "2026-08-21"): DayRunCount {
  return create(DayRunCountSchema, { day: label, completed, failed, filtered });
}

describe("dayTone", () => {
  it("maps a day's mix onto the instrument tones", () => {
    expect(dayTone(day(0))).toBe("muted");
    expect(dayTone(day(3))).toBe("nominal");
    expect(dayTone(day(0, 0, 2))).toBe("nominal"); // filtered-only is not a failure
    expect(dayTone(day(2, 1))).toBe("caution");
    expect(dayTone(day(0, 2))).toBe("critical");
  });
});

describe("Sparkline", () => {
  it("renders seven bars, newest last, with a per-day tone and a stub for zero days", () => {
    const days = [
      day(0, 0, 0, "d1"),
      day(1, 0, 0, "d2"),
      day(2, 1, 0, "d3"),
      day(0, 2, 0, "d4"),
      day(4, 0, 0, "d5"),
      day(0, 0, 1, "d6"),
      day(8, 0, 0, "d7"),
    ];
    const { container } = render(<Sparkline days={days} />);
    const bars = [...container.querySelectorAll("rect")];
    expect(bars).toHaveLength(7);
    expect(bars.map((b) => b.getAttribute("data-tone"))).toEqual([
      "muted",
      "nominal",
      "caution",
      "critical",
      "nominal",
      "nominal",
      "nominal",
    ]);
    // Zero day keeps a 2px stub; the busiest day fills the full height.
    expect(bars[0]!.getAttribute("height")).toBe("2");
    expect(bars[6]!.getAttribute("height")).toBe("24");
    expect(bars[0]!.getAttribute("class")).toContain("muted");
    expect(bars[3]!.getAttribute("class")).toContain("instrument-critical");
  });

  it("keeps only the last seven days when given more", () => {
    const days = Array.from({ length: 10 }, (_, i) => day(i, 0, 0, `d${i}`));
    const { container } = render(<Sparkline days={days} />);
    expect(container.querySelectorAll("rect")).toHaveLength(7);
  });
});
