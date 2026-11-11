/** Product code names no provider.
 *
 * A provider's name and its modules belong to `src/providers/<name>/`. Other
 * code reaches a provider through its role (git forge, chat, model router).
 * ALLOWED lists the files that still break the rule. It only shrinks: a file
 * that stops offending must leave the list, and no file may join it.
 */

import { readdirSync, readFileSync } from "node:fs";
import { join, relative } from "node:path";

import { expect, test } from "bun:test";

const SRC = join(import.meta.dir, "..");
const PROVIDERS = "github|slack|openrouter";

/** A quoted provider id, e.g. `"slack"`. */
const LITERAL = new RegExp(`["'\`](${PROVIDERS})["'\`]`);
/** An import of a module named for a provider, e.g. `../integrations/slack-policy.ts`. */
const IMPORT = new RegExp(`from\\s+"[^"]*/(${PROVIDERS})[^"/]*"`);
/** A module named for a provider, e.g. `integrations/slack.ts`. */
const FILENAME = new RegExp(`(^|/)(${PROVIDERS})[^/]*\\.ts$`);

const ALLOWED: string[] = [
  "automations/draft-catalog.ts",
  "automations/retire-provider-webhooks.ts",
  "db/schema.ts",
  "specs/projection.ts",
];

function sourceFiles(dir: string): string[] {
  return readdirSync(dir, { withFileTypes: true }).flatMap((entry) => {
    const path = join(dir, entry.name);
    if (entry.isDirectory()) {
      return entry.name === "__tests__" ? [] : sourceFiles(path);
    }
    return entry.name.endsWith(".ts") && !entry.name.endsWith(".test.ts") ? [path] : [];
  });
}

function offenders(): string[] {
  return sourceFiles(SRC)
    .map((path) => relative(SRC, path))
    .filter((file) => !file.startsWith("providers/") && !file.startsWith("gen/"))
    .filter((file) => {
      const text = readFileSync(join(SRC, file), "utf8");
      return FILENAME.test(file) || LITERAL.test(text) || IMPORT.test(text);
    })
    .sort();
}

test("only the listed files name a provider outside src/providers", () => {
  expect(offenders()).toEqual([...ALLOWED].sort());
});
