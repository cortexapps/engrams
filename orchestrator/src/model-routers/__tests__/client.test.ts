import { describe, expect, test } from "bun:test";

import { routerFor, routerForDecisionModel } from "../client.ts";
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

describe("routerForDecisionModel", () => {
  const routers = listModelRouterDefinitions();
  const native = routers.find((router) => router.decisions !== undefined);
  const plain = routers.find((router) => router.decisions === undefined);
  if (!native || !plain) throw new Error("the test needs a router with a decisions endpoint and one without");
  const pick = (
    connected: typeof routers,
    opts: { decision?: Record<string, string>; listed?: Record<string, string[]> } = {},
  ) =>
    routerForDecisionModel("acme/decider", {
      routers: async () => connected,
      models: async (router) => {
        const model = opts.decision?.[router.id];
        return { ...NONE, decision: model ? { model, chosen: true } : null };
      },
      listed: async (routerId, model) => opts.listed?.[routerId]?.includes(model) ?? false,
    });

  test("the router whose decision model it is wins over one that lists it", async () => {
    const chosen = await pick([native, plain], {
      decision: { [plain.id]: "acme/decider" },
      listed: { [native.id]: ["acme/decider"] },
    });
    expect(chosen).toEqual({ router: plain, model: "acme/decider" });
  });

  test("a router whose catalog lists the model serves it", async () => {
    const chosen = await pick([native, plain], { listed: { [plain.id]: ["acme/decider"] } });
    expect(chosen).toEqual({ router: plain, model: "acme/decider" });
  });

  test("an unlisted model goes to a router with a decisions endpoint", async () => {
    expect(await pick([plain, native])).toEqual({ router: native, model: "acme/decider" });
  });

  test("a router with no decisions endpoint does not get a model it does not list", async () => {
    expect(await pick([plain])).toBeNull();
  });
});
