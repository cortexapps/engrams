import { describe, expect, test } from "bun:test";

import {
  REVIEW_CATEGORIES,
  REVIEW_GUEST_DIR,
  renderReviewer,
  type ReviewCategory,
} from "../render";

const ORG_INSTRUCTIONS = "Always verify tenant isolation before submitting a finding.";

describe("renderReviewer", () => {
  test("renders a finder with every category and organization instructions", () => {
    const files = renderReviewer({
      role: "finder",
      enabledCategories: REVIEW_CATEGORIES,
      orgInstructions: ORG_INSTRUCTIONS,
    });

    expect(files).toHaveLength(7);
    expect(files.map((file) => file.path)).toEqual([
      `${REVIEW_GUEST_DIR}/finder.md`,
      `${REVIEW_GUEST_DIR}/lenses/security-privacy.md`,
      `${REVIEW_GUEST_DIR}/lenses/stability-availability.md`,
      `${REVIEW_GUEST_DIR}/lenses/data-integrity-integration.md`,
      `${REVIEW_GUEST_DIR}/lenses/functional-correctness.md`,
      `${REVIEW_GUEST_DIR}/lenses/performance-scalability.md`,
      `${REVIEW_GUEST_DIR}/lenses/maintainability-quality.md`,
    ]);
    for (const file of files) expect(file.content).not.toContain("{{");
    expect(files).toMatchSnapshot();
  });

  test("renders a verifier with its lens files", () => {
    // The verifier enforces each lens's "Do not report" bar on candidates, so
    // it gets the same lens files as the finder.
    const files = renderReviewer({
      role: "verifier",
      enabledCategories: REVIEW_CATEGORIES,
      orgInstructions: ORG_INSTRUCTIONS,
    });

    expect(files).toHaveLength(7);
    expect(files[0]?.path).toBe(`${REVIEW_GUEST_DIR}/verifier.md`);
    expect(
      files.slice(1).every((file) => file.path.includes("/lenses/")),
    ).toBe(true);
    for (const file of files) expect(file.content).not.toContain("{{");
    expect(files).toMatchSnapshot();
  });

  test("renders only a two-category finder subset", () => {
    const files = renderReviewer({
      role: "finder",
      enabledCategories: ["performance-scalability", "security-privacy"],
    });

    expect(files).toHaveLength(3);
    expect(files.map((file) => file.path)).toEqual([
      `${REVIEW_GUEST_DIR}/finder.md`,
      `${REVIEW_GUEST_DIR}/lenses/security-privacy.md`,
      `${REVIEW_GUEST_DIR}/lenses/performance-scalability.md`,
    ]);

    const roleContent = files[0]?.content ?? "";
    expect(roleContent).toContain(
      "- 🔒 **Security & Privacy** — read `/workspace/.review/lenses/security-privacy.md`",
    );
    expect(roleContent).toContain(
      "- 🚀 **Performance & Scalability** — read `/workspace/.review/lenses/performance-scalability.md`",
    );
    for (const category of [
      "stability-availability",
      "data-integrity-integration",
      "functional-correctness",
      "maintainability-quality",
    ]) {
      expect(files.some((file) => file.path.endsWith(`/lenses/${category}.md`))).toBe(false);
    }
  });

  test("uses the fixed placeholder for absent or whitespace-only organization instructions", () => {
    for (const orgInstructions of [undefined, " \n\t "]) {
      const files = renderReviewer({ role: "verifier", orgInstructions });
      expect(files[0]?.content).toContain("_No organization instructions configured._");
    }
  });

  test("fills org instructions literally — `$`-metacharacters and slot-shaped text are not interpreted", () => {
    // `$&`/`$$`/`$1` are JS replacement-pattern metacharacters, and `{{TOOL_CONTRACT}}`
    // is another slot. A naive string replace would expand the first and re-expand
    // the second; a single-pass function replacement must take all of it verbatim.
    const orgInstructions =
      "Document replacements using literal $& and $$ and $1; never write {{TOOL_CONTRACT}} yourself.";
    const files = renderReviewer({ role: "finder", orgInstructions });
    const content = files[0]?.content ?? "";

    expect(content).toContain(orgInstructions);
    expect(content).not.toContain("{{ORG_INSTRUCTIONS}}");
    // The literal `{{TOOL_CONTRACT}}` inside org text must survive as text, not be
    // re-expanded into a second copy of the tool contract.
    expect(content.match(/submit_finding/g)?.length ?? 0).toBe(
      renderReviewer({ role: "finder", orgInstructions: "none" })[0]!.content.match(
        /submit_finding/g,
      )!.length,
    );
  });

  test("rejects an unknown category", () => {
    expect(() =>
      Reflect.apply(renderReviewer, undefined, [
        { role: "finder", enabledCategories: ["unknown-category"] },
      ]),
    ).toThrow("unknown review category: unknown-category");
  });

  test("sorts and de-duplicates categories deterministically", () => {
    const sorted: readonly ReviewCategory[] = [
      "security-privacy",
      "functional-correctness",
      "maintainability-quality",
    ];
    const scrambled: readonly ReviewCategory[] = [
      "maintainability-quality",
      "security-privacy",
      "functional-correctness",
      "security-privacy",
    ];

    expect(renderReviewer({ role: "finder", enabledCategories: scrambled })).toEqual(
      renderReviewer({ role: "finder", enabledCategories: sorted }),
    );
  });
});
