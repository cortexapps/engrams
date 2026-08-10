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

    const prose = container.querySelector('[data-diff-granularity="word"]');
    const code = container.querySelector('[data-diff-granularity="line"]');
    expect(prose).not.toBeNull();
    expect(code).not.toBeNull();
    expect(prose!.querySelector("del")?.textContent).toBe("fast");
    expect(prose!.querySelector("ins")?.textContent).toBe("bounded");
    expect(code!.querySelector("del")?.textContent).toBe("const retries = 2;\n");
    expect(code!.querySelector("ins")?.textContent).toBe("const retries = 3;\n");
    expect(screen.getByLabelText("Checkpoint comparison")).toBeTruthy();
  });
});
