import { expect, test } from "bun:test";

import { connectorRegistry } from "../../../connectors/registry.ts";
import { providerSpecs } from "../../../providers/index.ts";
import { builtinActions } from "../builtin.ts";

/** Every `builtin` action id that a shipped connector names. */
function declaredBuiltinIds(): string[] {
  return [...connectorRegistry().values()].flatMap((connector) =>
    (connector.actions ?? []).flatMap((action) =>
      action.execute.kind === "builtin" ? [action.execute.id] : [],
    ),
  );
}

test("every builtin action a connector names has an implementation", () => {
  const table = builtinActions();
  const missing = declaredBuiltinIds().filter((id) => !table.has(id));
  expect(missing).toEqual([]);
});

test("every implementation is named by a connector", () => {
  const declared = new Set(declaredBuiltinIds());
  const unused = [...builtinActions().keys()].filter((id) => !declared.has(id));
  expect(unused).toEqual([]);
});

test("every git forge has the forge actions", () => {
  const table = builtinActions();
  for (const { provider, forge } of providerSpecs()) {
    if (!forge) continue;
    for (const name of ["post_pr_review", "upsert_pr_comment", "update_pr_comment"]) {
      expect(table.has(`${provider}.${name}`)).toBe(true);
    }
  }
});
