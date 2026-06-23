// ADR 0054 Flavor A: the collapsed file-change row. We assert the path + the
// jsdiff-computed +/− counts, which render WITHOUT expanding — so the heavy
// Pierre/Shiki diff (lazy, inside the collapsed content) never mounts under
// jsdom. Expanding the diff is exercised manually, not here.

import { afterEach, describe, expect, test } from "vitest";
import { cleanup, render, screen } from "@testing-library/react";
import { FileChangePart } from "./FileChangePart";
import type { FileChangeArgs } from "./buildMessages";

afterEach(cleanup);

function renderPart(args: FileChangeArgs) {
  // The renderer reads only `args`; the rest of the tool-part props don't
  // affect the collapsed row under test.
  const props = { args } as unknown as React.ComponentProps<typeof FileChangePart>;
  return render(<FileChangePart {...props} />);
}

describe("FileChangePart", () => {
  test("a write shows the path and an additions count, diff not yet loaded", () => {
    renderPart({ path: "new.txt", change: { write: { content: "a\nb\nc\n" } } });
    expect(screen.getByText("new.txt")).toBeTruthy();
    expect(screen.getByText(/^\+\d+$/)).toBeTruthy();
    // Lazy Pierre diff lives in the collapsed content — not mounted.
    expect(screen.queryByText("Loading diff…")).toBeNull();
  });

  test("an edit shows both additions and deletions counts", () => {
    renderPart({
      path: "src/a.rs",
      change: { edit: { hunks: [{ old: "let x = 1;\nold line\n", new: "let x = 2;\n" }] } },
    });
    expect(screen.getByText("src/a.rs")).toBeTruthy();
    expect(screen.getByText(/^\+\d+$/)).toBeTruthy();
    // Deletions use the U+2212 minus sign, matching the component.
    expect(screen.getByText(/^−\d+$/)).toBeTruthy();
  });
});
