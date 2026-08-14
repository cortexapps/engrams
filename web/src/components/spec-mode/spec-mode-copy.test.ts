import { readdirSync, readFileSync } from "node:fs";
import * as path from "node:path";
import { describe, expect, test } from "vitest";

const SOURCE_ROOT = path.join(process.cwd(), "src");
const SURFACE_ROOTS = ["components/spec-mode", "components/spec", "pages/specmode", "pages/specs"];
const F7_BOUNDARY = new Set(["NewSpecSheet.tsx", "SpecPublishControl.tsx"]);
const BANNED_COPY = [
  /\bfrontier\b/i,
  /\bdrafted\b/i,
  /\bconfirmed\b/i,
  /\bstages?\b/i,
  /\bgap[ -]check\b/i,
  /\bred[ -]team\b/i,
  /\bn\s*\/\s*a\b/i,
];

describe("spec mode vocabulary", () => {
  test("person-facing component strings use the product state names", () => {
    const failures: string[] = [];
    for (const relativeRoot of SURFACE_ROOTS) {
      for (const file of componentFiles(path.join(SOURCE_ROOT, relativeRoot))) {
        if (F7_BOUNDARY.has(path.basename(file))) continue;
        const source = readFileSync(file, "utf8");
        const personFacingSource = source.replaceAll('"n/a"', "").replaceAll("'n/a'", "");
        for (const pattern of BANNED_COPY) {
          const match = personFacingSource.match(pattern);
          if (match) failures.push(`${path.relative(SOURCE_ROOT, file)}: ${match[0]}`);
        }
      }
    }
    expect(failures).toEqual([]);
  });
});

function componentFiles(directory: string): string[] {
  return readdirSync(directory, { withFileTypes: true }).flatMap((entry) => {
    const file = path.join(directory, entry.name);
    if (entry.isDirectory()) return componentFiles(file);
    return entry.name.endsWith(".tsx") && !entry.name.includes(".test.") ? [file] : [];
  });
}
