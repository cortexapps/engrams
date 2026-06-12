// @ts-expect-error - node built-ins available in vitest's Node.js runner;
// @types/node is not installed but the modules exist at runtime.
// eslint-disable-next-line @typescript-eslint/ban-ts-comment
/* eslint-disable */
// @ts-nocheck
import { describe, it, expect } from "vitest";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

// ADR 0039 §6: web/src/lib/ability.ts is a verbatim copy of
// orchestrator/src/authz/ability.ts with a single header comment prepended.
// This test detects *meaningful* drift between the two files.
//
// "Meaningful" means: the test compares the normalised token stream rather
// than byte equality, so that auto-formatters (oxfmt, prettier, etc.) running
// in the web tree cannot trip the guard by reflowing multi-line imports, union
// types, or removing trailing commas.  If the logic, exports, or doc-comments
// change in the orchestrator file but are NOT reflected here, the normalised
// strings will diverge and the test will fail.

function normalise(src: string): string {
  return (
    src
      // 1. Remove single-line // comments.
      .replace(/\/\/[^\n]*/g, "")
      // 2. Remove block /* ... */ comments.
      .replace(/\/\*[\s\S]*?\*\//g, "")
      // 3. Remove trailing commas before closing brackets/parens/braces.
      .replace(/,(\s*[}\]>)]+)/g, "$1")
      // 4. Collapse all whitespace (spaces, tabs, newlines) to a single space.
      .replace(/\s+/g, " ")
      // 5. Remove spaces around pipe separators (union formatting).
      .replace(/\s*\|\s*/g, "|")
      // 6. Remove leading pipe after assignment (= | "type" → ="type").
      //    Also normalise "= " before string literals (collapsed union vs
      //    multi-line union differ by a leading pipe; both should reach ="...).
      .replace(/= \|/g, "=")
      .replace(/=\|/g, "=")
      .replace(/= "/g, '="')
      // 7. Trim.
      .trim()
  );
}

describe("ability.ts drift guard", () => {
  it("web/src/lib/ability.ts has the same normalised content as orchestrator/src/authz/ability.ts (minus the SOURCE header)", () => {
    // web/src/lib/ is 3 levels down from the repo root:
    //   repo-root/web/src/lib/ability.drift.test.ts
    const thisDir = path.dirname(fileURLToPath(import.meta.url));
    const repoRoot = path.resolve(thisDir, "../../..");

    const webFile = path.join(repoRoot, "web/src/lib/ability.ts");
    const orchestratorFile = path.join(repoRoot, "orchestrator/src/authz/ability.ts");

    const webContent = fs.readFileSync(webFile, "utf-8");
    const orchestratorContent = fs.readFileSync(orchestratorFile, "utf-8");

    // Strip the SOURCE: header comment (first non-blank content line) from the
    // web copy so we compare only the shared logic.
    const webLines = webContent.split("\n");
    let headerEnd = 0;
    for (let i = 0; i < webLines.length; i++) {
      if (webLines[i].includes("SOURCE:")) {
        // Also consume the following blank line if present.
        headerEnd = webLines[i + 1]?.trim() === "" ? i + 2 : i + 1;
        break;
      }
    }
    const webStripped = webLines.slice(headerEnd).join("\n");

    expect(normalise(webStripped)).toBe(normalise(orchestratorContent));
  });
});
