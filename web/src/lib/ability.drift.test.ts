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
// This test detects drift between the two files.

describe("ability.ts drift guard", () => {
  it("web/src/lib/ability.ts matches orchestrator/src/authz/ability.ts (minus the header comment)", () => {
    // web/src/lib/ is 3 levels down from the repo root:
    //   repo-root/web/src/lib/ability.drift.test.ts
    const thisDir = path.dirname(fileURLToPath(import.meta.url));
    const repoRoot = path.resolve(thisDir, "../../..");

    const webFile = path.join(repoRoot, "web/src/lib/ability.ts");
    const orchestratorFile = path.join(repoRoot, "orchestrator/src/authz/ability.ts");

    const webContent = fs.readFileSync(webFile, "utf-8");
    const orchestratorContent = fs.readFileSync(orchestratorFile, "utf-8");

    // Strip the header comment from the web copy. The header is the first line
    // (the "SOURCE:" comment) plus a blank line separator, i.e. lines up to and
    // including the first line that contains "SOURCE:".
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

    expect(webStripped).toBe(orchestratorContent);
  });
});
