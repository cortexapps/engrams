import { render, screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";

import { CheckpointDiff } from "./CheckpointDiff";

describe("CheckpointDiff", () => {
  it("diffs prose by word and fenced code by line", () => {
    const { container } = render(
      <CheckpointDiff
        before={"## Design\n\nUse fast retries.\n\n```ts\nconst retries = 2;\nrun(retries);\n```\n"}
        after={
          "## Design\n\nUse bounded retries.\n\n```ts\nconst retries = 3;\nrun(retries);\n```\n"
        }
      />,
    );

    const prose = [...container.querySelectorAll('[data-diff-granularity="word"]')].find((node) =>
      node.querySelector("del"),
    );
    const code = [...container.querySelectorAll('[data-diff-granularity="line"]')].find((node) =>
      node.querySelector("del"),
    );
    expect(prose).not.toBeNull();
    expect(code).not.toBeNull();
    expect(prose!.querySelector("del")?.textContent).toBe("fast");
    expect(prose!.querySelector("ins")?.textContent).toBe("bounded");
    expect(code!.querySelector("del")?.textContent).toBe("const retries = 2;\n");
    expect(code!.querySelector("ins")?.textContent).toBe("const retries = 3;\n");
    expect(screen.getByLabelText("Checkpoint comparison")).toBeTruthy();
  });

  it("keeps later prose aligned when a code block is inserted", () => {
    const before = "## Design\n\nKeep this paragraph.\n\nStable prose after the block.\n";
    const after =
      "## Design\n\nKeep this paragraph.\n\n```ts\nconst ready = true;\n```\n\nStable prose after the block.\n";
    const { container } = render(<CheckpointDiff before={before} after={after} />);

    const changes = [...container.querySelectorAll("ins, del")].map((node) => node.textContent);
    expect(changes.join("\n")).toContain("const ready = true;");
    expect(changes.join("\n")).not.toContain("Stable prose after the block.");
  });

  it("keeps later prose aligned when a code block is removed", () => {
    const before =
      "## Design\n\nKeep this paragraph.\n\n```ts\nconst legacy = true;\n```\n\nStable prose after the block.\n";
    const after = "## Design\n\nKeep this paragraph.\n\nStable prose after the block.\n";
    const { container } = render(<CheckpointDiff before={before} after={after} />);

    const changes = [...container.querySelectorAll("ins, del")].map((node) => node.textContent);
    expect(changes.join("\n")).toContain("const legacy = true;");
    expect(changes.join("\n")).not.toContain("Stable prose after the block.");
  });

  it("uses a bounded line diff when the segment matrix is large", () => {
    const before = Array.from({ length: 100 }, (_, index) => `Before ${index}.\n\n`).join("");
    const after = Array.from({ length: 100 }, (_, index) => `After ${index}.\n\n`).join("");
    const { container } = render(<CheckpointDiff before={before} after={after} />);

    const segments = container.querySelectorAll("[data-diff-granularity]");
    expect(segments).toHaveLength(1);
    expect(segments[0]?.getAttribute("data-diff-granularity")).toBe("line");
  });
});
