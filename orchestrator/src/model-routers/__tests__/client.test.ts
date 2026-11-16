import { describe, expect, test } from "bun:test";

import { routerFor } from "../client.ts";
import type { RouterModels } from "../models.ts";
import { listModelRouterDefinitions } from "../registry.ts";

const NONE: RouterModels = { default: null, title: null, decision: null };

describe("routerFor", () => {
  const [first, second] = listModelRouterDefinitions();
  if (!first || !second) throw new Error("the test needs two model routers");
  const pick = (models: Record<string, RouterModels>) =>
    routerFor("decision", {
      routers: async () => [first, second],
      models: async (router) => models[router.id] ?? NONE,
    });

  test("an admin's choice on a later router wins over a built-in model", async () => {
    const chosen = await pick({
      [first.id]: { ...NONE, decision: { model: "builtin-model", chosen: false } },
      [second.id]: { ...NONE, decision: { model: "admin-model", chosen: true } },
    });
    expect(chosen).toEqual({ router: second, model: "admin-model" });
  });

  test("with no choice, the first router that has a built-in model wins", async () => {
    const chosen = await pick({
      [second.id]: { ...NONE, decision: { model: "builtin-model", chosen: false } },
    });
    expect(chosen).toEqual({ router: second, model: "builtin-model" });
  });

  test("no router has a model for the role", async () => {
    expect(await pick({})).toBeNull();
  });
});
