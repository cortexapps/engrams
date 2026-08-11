import { describe, expect, mock, test } from "bun:test";
import { Hono } from "hono";

import { makeSpecTemplatesRoute } from "../routes/spec-templates.ts";
import {
  ENGINEERING_DESIGN_TEMPLATE,
  type SpecTemplateCatalogItem,
} from "../specs/template-catalog.ts";

const TEMPLATE_ID = "00000000-0000-4000-8000-000000000115";
const ITEM: SpecTemplateCatalogItem = {
  id: TEMPLATE_ID,
  ...structuredClone(ENGINEERING_DESIGN_TEMPLATE),
  builtIn: true,
  modifiedFromDefault: false,
  createdAt: "2026-08-11T00:00:00.000Z",
  updatedAt: "2026-08-11T00:00:00.000Z",
};

function appFor(role: "admin" | "user", overrides: Record<string, unknown> = {}) {
  const catalog = {
    list: mock(async () => []),
    create: mock(async () => ITEM),
    clone: mock(async () => ITEM),
    update: mock(async () => ITEM),
    restoreDefault: mock(async () => ITEM),
    ...overrides,
  };
  const app = new Hono();
  app.route(
    "/",
    makeSpecTemplatesRoute({
      catalog,
      orgId: "org-1",
      getSession: async () => ({ user: { id: "member-1", role } }),
    }),
  );
  return { app, catalog };
}

describe("spec template routes", () => {
  test("a non-admin cannot list templates", async () => {
    const { app, catalog } = appFor("user");
    const response = await app.request("/api/v1/spec-templates");

    expect(response.status).toBe(403);
    expect(catalog.list).not.toHaveBeenCalled();
  });

  test("a non-admin write is denied by CASL", async () => {
    const { app, catalog } = appFor("user");
    const response = await app.request(`/api/v1/spec-templates/${TEMPLATE_ID}`, {
      method: "PUT",
      headers: { "content-type": "application/json" },
      body: JSON.stringify(ENGINEERING_DESIGN_TEMPLATE),
    });

    expect(response.status).toBe(403);
    expect(catalog.update).not.toHaveBeenCalled();
  });

  test("an admin can clone a visible template", async () => {
    const { app, catalog } = appFor("admin");
    const response = await app.request(`/api/v1/spec-templates/${TEMPLATE_ID}/clone`, {
      method: "POST",
    });

    expect(response.status).toBe(201);
    expect(catalog.clone).toHaveBeenCalledWith("org-1", TEMPLATE_ID);
  });
});
