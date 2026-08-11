import { Hono, type Context } from "hono";
import { HTTPException } from "hono/http-exception";

import { getSessionFromHeaders } from "../auth/session.ts";
import { abilityFor } from "../authz/ability.ts";
import type { GetSession } from "./guard.ts";
import { SpecTemplateCatalog, SpecTemplateValidationError } from "../specs/template-catalog.ts";

const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[1-5][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/i;

export interface SpecTemplatesRouteDeps {
  catalog: Pick<SpecTemplateCatalog, "list" | "create" | "clone" | "update" | "restoreDefault">;
  orgId: string;
  getSession?: GetSession;
}

export function makeSpecTemplatesRoute(deps: SpecTemplatesRouteDeps): Hono {
  const app = new Hono();
  const getSession = deps.getSession ?? getSessionFromHeaders;

  async function requireAbility(c: Context, action: "read" | "manage") {
    const session = await getSession(c.req.raw.headers);
    if (!session) throw new HTTPException(401, { message: "unauthenticated" });
    const actor = { id: session.user.id, role: session.user.role ?? "user" };
    if (!abilityFor(actor).can(action, "SpecTemplate")) {
      throw new HTTPException(403, { message: "forbidden" });
    }
    return actor;
  }

  function templateId(c: Context): string {
    const id = c.req.param("id");
    if (typeof id !== "string" || !UUID.test(id)) {
      throw new HTTPException(404, { message: "not found" });
    }
    return id;
  }

  app.get("/api/v1/spec-templates", async (c) => {
    await requireAbility(c, "read");
    return c.json({ templates: await deps.catalog.list(deps.orgId) });
  });

  app.post("/api/v1/spec-templates", async (c) => {
    await requireAbility(c, "manage");
    const input = await jsonBody(c);
    try {
      return c.json({ template: await deps.catalog.create(deps.orgId, input) }, 201);
    } catch (error) {
      throw validationException(error);
    }
  });

  app.put("/api/v1/spec-templates/:id", async (c) => {
    await requireAbility(c, "manage");
    const id = templateId(c);
    const input = await jsonBody(c);
    try {
      const template = await deps.catalog.update(deps.orgId, id, input);
      if (!template) throw new HTTPException(404, { message: "not found" });
      return c.json({ template });
    } catch (error) {
      throw validationException(error);
    }
  });

  app.post("/api/v1/spec-templates/:id/clone", async (c) => {
    await requireAbility(c, "manage");
    const template = await deps.catalog.clone(deps.orgId, templateId(c));
    if (!template) throw new HTTPException(404, { message: "not found" });
    return c.json({ template }, 201);
  });

  app.post("/api/v1/spec-templates/:id/restore", async (c) => {
    await requireAbility(c, "manage");
    const template = await deps.catalog.restoreDefault(deps.orgId, templateId(c));
    if (!template) throw new HTTPException(404, { message: "not found" });
    return c.json({ template });
  });

  return app;
}

async function jsonBody(c: Context): Promise<unknown> {
  try {
    return await c.req.json();
  } catch {
    throw new HTTPException(400, { message: "invalid JSON body" });
  }
}

function validationException(error: unknown): Error {
  if (error instanceof HTTPException) return error;
  if (error instanceof SpecTemplateValidationError) {
    return new HTTPException(400, { message: error.message });
  }
  return error instanceof Error ? error : new Error(String(error));
}
