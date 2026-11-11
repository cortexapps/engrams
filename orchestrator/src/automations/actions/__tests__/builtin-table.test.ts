import { expect, test } from "bun:test";

import { BUILTIN_ACTION_IDS } from "../../../connectors/builtin-actions-allowlist.ts";
import { forgeProviders } from "../../../forges/registry.ts";
import { BUILTIN_ACTIONS } from "../builtin.ts";

test("the ids a connector may name are the ids that have an implementation", () => {
  expect(Object.keys(BUILTIN_ACTIONS).sort()).toEqual([...BUILTIN_ACTION_IDS].sort());
});

test("every git forge has a post_pr_review action", () => {
  for (const provider of forgeProviders()) {
    expect(BUILTIN_ACTIONS).toHaveProperty([`${provider}.post_pr_review`]);
  }
});
