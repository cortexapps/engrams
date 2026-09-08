import { describe, expect, it } from "vitest";
import { render } from "@testing-library/react";

import { EngramMark, strokeFor } from "./EngramMark";

const q = (c: HTMLElement, slot: string) => c.querySelector(`[data-slot="${slot}"]`);

describe("EngramMark", () => {
  it("draws one silhouette: the base trace, the entry ring, the terminal dot", () => {
    const { container } = render(<EngramMark size={64} />);
    expect(q(container, "trace")!.getAttribute("d")).toBe("M14 68 L32 50 L50 68 L68 32 L86 50");
    expect(q(container, "entry")!.getAttribute("stroke")).toBe("var(--mark-entry)");
    expect(q(container, "terminal")!.getAttribute("fill")).toBe("var(--mark-terminal)");
    // No middle nodes, no lattice.
    expect(container.querySelectorAll("circle")).toHaveLength(2);
    expect(container.querySelectorAll("rect")).toHaveLength(0);
  });

  it("heavies the stroke as it shrinks and drops the ring below 20px", () => {
    expect(strokeFor(72)).toBe(9);
    expect(strokeFor(32)).toBe(10);
    expect(strokeFor(16)).toBe(12);
    const { container } = render(<EngramMark size={16} />);
    expect(q(container, "entry")).toBeNull();
    expect(q(container, "terminal")).not.toBeNull();
    expect(q(container, "trace")!.getAttribute("stroke-width")).toBe("12");
  });

  it("takes the cover palette on the spine and one colour in mono", () => {
    const cover = render(<EngramMark size={24} ground="cover" />).container;
    expect(q(cover, "terminal")!.getAttribute("fill")).toBe("var(--sidebar-primary)");
    expect(q(cover, "entry")!.getAttribute("stroke")).toBe("#e0913d");
    const mono = render(<EngramMark size={24} mono />).container;
    expect(q(mono, "terminal")!.getAttribute("fill")).toBe("currentColor");
    expect(q(mono, "entry")!.getAttribute("stroke")).toBe("currentColor");
  });

  it("as a loader shows the ghost trace with a travelling segment and no nodes", () => {
    const { container } = render(<EngramMark size={56} mode="loader" />);
    const paths = container.querySelectorAll("path");
    expect(paths).toHaveLength(2);
    expect(paths[0]!.getAttribute("stroke-opacity")).toBe("0.24");
    expect((q(container, "trace") as SVGPathElement).style.strokeDasharray).toBe("60 60");
    expect(q(container, "entry")).toBeNull();
    expect(q(container, "terminal")).toBeNull();
  });

  it("names itself for a screen reader", () => {
    const { getByRole } = render(<EngramMark title="Connecting…" />);
    expect(getByRole("img", { name: "Connecting…" })).toBeTruthy();
  });
});
