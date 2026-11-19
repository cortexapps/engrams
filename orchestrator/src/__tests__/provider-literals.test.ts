/** Product code names no provider.
 *
 * A provider's name and its modules belong to `src/providers/<name>/`. Other
 * code reaches a provider through its role (git forge, chat, model router),
 * its event source or its actions, all of which `providers/index.ts` lists.
 * ALLOWED lists the files that name a provider for a reason that is not one
 * of those, with the reason. A file that stops naming a provider must leave
 * the list.
 */

import { readdirSync, readFileSync } from "node:fs";
import { join, relative } from "node:path";

import { expect, test } from "bun:test";

import { providerSpecs } from "../providers/index.ts";

const SRC = join(import.meta.dir, "..");
const PROVIDER_DIRS = readdirSync(join(SRC, "providers"), { withFileTypes: true })
  .filter((entry) => entry.isDirectory())
  .map((entry) => entry.name)
  .sort();
const NAMES = PROVIDER_DIRS.join("|");

/** A quoted provider id, e.g. `"slack"`. */
const LITERAL = new RegExp(`["'\`](${NAMES})["'\`]`);
/** An import of a provider's module, or of a module named for a provider. */
const IMPORT = new RegExp(`from\\s+"[^"]*/(?:providers/(${NAMES})/[^"]*|(${NAMES})[^"/]*)"`);
/** A module named for a provider, e.g. `integrations/slack.ts`. */
const FILENAME = new RegExp(`(^|/)(${NAMES})[^/]*\\.ts$`);

const ALLOWED: string[] = [
  // The drafting prompt shows the stored shape of a relay target.
  "automations/draft-catalog.ts",
  // A backfill of rows that were GitHub's before triggers named a provider.
  "automations/retire-provider-webhooks.ts",
  // Column defaults for rows written before the column existed.
  "db/schema.ts",
  // The spec ticket sync and the `team` key picker are Linear's: no role
  // covers an issue tracker.
  "index.ts",
  "routes/spec-ticket-sync.ts",
  "rpc/automations.ts",
  "specs/ticket-sync-connector.ts",
  "specs/ticket-sync-service.ts",
  "specs/ticket-sync.ts",
  "workflows/spec-ticket-sync.ts",
  // "github" is a markdown dialect here, not a provider.
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

test("every provider directory is in the provider list, under its own name", () => {
  expect(providerSpecs().map((spec) => spec.provider).sort()).toEqual(PROVIDER_DIRS);
});
