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

  test("renders a verifier without lens files", () => {
    const files = renderReviewer({
      role: "verifier",
      enabledCategories: REVIEW_CATEGORIES,
      orgInstructions: ORG_INSTRUCTIONS,
    });

    expect(files).toHaveLength(1);
    expect(files[0]?.path).toBe(`${REVIEW_GUEST_DIR}/verifier.md`);
    expect(files[0]?.content).not.toContain("{{");
    expect(files.some((file) => file.path.includes("/lenses/"))).toBe(false);
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
