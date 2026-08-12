/**
 * The ticket tree routes (ADR 0114 D6, R39-R40).
 *
 * Direct manipulation is the primary verb here, so this is a plain REST
 * surface over the tree rather than an agent tool: the canvas retitles,
 * edits, splits, merges, reorders, re-parents, adds and deletes, and each of
 * those is one request that returns the whole tree back.
 *
 * Returning the whole tree is deliberate. A move changes the ordinal of every
 * later sibling, so a response that carried only the moved row would make the
 * browser guess the rest. The tree is small (R39 caps a proposal at 500), and
 * one authoritative reply is what keeps an optimistic drag honest.
 */

import { Hono, type Context } from "hono";
import { HTTPException } from "hono/http-exception";

import {
  SpecTicketError,
  type SpecTicketTreeService,
  type SpecTicketTreeView,
} from "../specs/ticket-tree.ts";
import { SpecTicketTreeError } from "@engrams/spec-document";
import type { GetSession, ResolveSpecMembership } from "./guard.ts";
import { makeSpecMemberHeaderGuard } from "./guard.ts";

const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[1-5][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/i;

/** The wire shape of one ticket. */
export interface SpecTicketPayload {
  id: string;
  parentId: string | null;
  ordinal: number;
  depth: number;
  title: string;
  /** The description a person edits: the stored text without its link line. */
  body: string;
  description: string;
  backlink: { sectionId: string; sectionTitle: string; href: string };
  dependsOn: string[];
  syncState: string;
  linearId: string | null;
  syncError: string | null;
  openQuestions: Array<{ id: string; sectionId: string; text: string }>;
}

export interface SpecTicketTreePayload {
  specId: string;
  checkpointId: string;
  docSeq: string;
  publishedAt: string | null;
  sections: Array<{ id: string; title: string }>;
  tickets: SpecTicketPayload[];
  unattachedQuestions: Array<{ id: string; sectionId: string; text: string }>;
}

export interface SpecTicketRouteDeps {
  tickets: SpecTicketTreeService;
  resolveMembership: ResolveSpecMembership;
  getSession?: GetSession;
}

export function makeSpecTicketRoute(deps: SpecTicketRouteDeps): Hono {
  const app = new Hono();
  const authorize = makeSpecMemberHeaderGuard(deps.resolveMembership, deps.getSession);

  async function requireMember(c: Context): Promise<string> {
    const specId = c.req.param("id");
    if (typeof specId !== "string" || !UUID.test(specId)) {
      throw new HTTPException(404, { message: "not found" });
    }
    const result = await authorize(c.req.raw.headers, specId);
    if (!result.ok) {
      throw new HTTPException(result.status, {
        message: result.status === 401 ? "unauthenticated" : "not found",
      });
    }
    return specId;
  }

  /** Runs one tree change and answers with the whole tree. */
  async function respond(
    c: Context,
    change: (specId: string, body: Record<string, unknown>) => Promise<SpecTicketTreeView>,
    withBody = true,
  ): Promise<Response> {
    const specId = await requireMember(c);
    const body = withBody ? await readJsonObject(c) : {};
    try {
      return c.json(treePayload(await change(specId, body)));
    } catch (error) {
      throw ticketHttpError(error);
    }
  }

  app.get("/api/v1/specs/:id/tickets", (c) =>
    respond(c, (specId) => deps.tickets.read(specId), false),
  );

  app.post("/api/v1/specs/:id/tickets", (c) =>
    respond(c, (specId, body) =>
      deps.tickets.addTicket({
        specId,
        parentId: optionalId(body["parentId"], "parentId"),
        index: optionalIndex(body["index"]),
        title: requiredText(body["title"], "title"),
        description: text(body["body"], "body") ?? "",
        sectionId: requiredText(body["sectionId"], "sectionId"),
      }),
    ),
  );

  app.patch("/api/v1/specs/:id/tickets/:ticketId", (c) =>
    respond(c, (specId, body) =>
      deps.tickets.updateTicket({
        specId,
        id: requireTicketId(c),
        title: text(body["title"], "title"),
        body: text(body["body"], "body"),
        sectionId: text(body["sectionId"], "sectionId"),
        dependsOn: optionalIdList(body["dependsOn"]),
      }),
    ),
  );

  app.delete("/api/v1/specs/:id/tickets/:ticketId", (c) =>
    respond(c, (specId) => deps.tickets.deleteTicket({ specId, id: requireTicketId(c) }), false),
  );

  // Drag to nest and drag to reorder are the same request: a new parent and a
  // position among its children.
  app.post("/api/v1/specs/:id/tickets/:ticketId/move", (c) =>
    respond(c, (specId, body) =>
      deps.tickets.moveTicket({
        specId,
        id: requireTicketId(c),
        parentId: optionalId(body["parentId"], "parentId"),
        index: optionalIndex(body["index"]),
      }),
    ),
  );

  app.post("/api/v1/specs/:id/tickets/:ticketId/split", (c) =>
    respond(c, (specId, body) =>
      deps.tickets.splitTicket({ specId, id: requireTicketId(c), parts: parts(body["parts"]) }),
    ),
  );

  app.post("/api/v1/specs/:id/tickets/:ticketId/merge", (c) =>
    respond(c, (specId, body) =>
      deps.tickets.mergeTickets({
        specId,
        targetId: requireTicketId(c),
        sourceIds: requiredIdList(body["sourceIds"], "sourceIds"),
        title: text(body["title"], "title"),
        body: text(body["body"], "body"),
      }),
    ),
  );

  return app;
}

function requireTicketId(c: Context): string {
  const ticketId = c.req.param("ticketId");
  if (typeof ticketId !== "string" || !UUID.test(ticketId)) {
    throw new HTTPException(404, { message: "not found" });
  }
  return ticketId;
}

export function treePayload(view: SpecTicketTreeView): SpecTicketTreePayload {
  return {
    specId: view.specId,
    checkpointId: view.checkpointId,
    docSeq: view.docSeq,
    publishedAt: view.publishedAt?.toISOString() ?? null,
    sections: view.sections.map((section) => ({ id: section.id, title: section.title })),
    tickets: view.tickets.map((ticket) => ({
      id: ticket.id,
      parentId: ticket.parentId,
      ordinal: ticket.ordinal,
      depth: ticket.depth,
      title: ticket.title,
      body: ticket.body,
      description: ticket.description,
      backlink: {
        sectionId: ticket.backlink.sectionId,
        sectionTitle: ticket.backlink.sectionTitle,
        href: ticket.backlink.href,
      },
      dependsOn: ticket.dependsOn,
      syncState: ticket.syncState,
      linearId: ticket.linearId,
      syncError: ticket.syncError,
      openQuestions: ticket.openQuestions.map((question) => ({
        id: question.id,
        sectionId: question.sectionId,
        text: question.text,
      })),
    })),
    unattachedQuestions: view.unattachedQuestions.map((question) => ({
      id: question.id,
      sectionId: question.sectionId,
      text: question.text,
    })),
  };
}

function text(value: unknown, field: string): string | undefined {
  if (value === undefined || value === null) return undefined;
  if (typeof value !== "string") {
    throw new HTTPException(400, { message: `${field} must be a string` });
  }
  return value;
}

function requiredText(value: unknown, field: string): string {
  const found = text(value, field);
  if (found === undefined || found.trim() === "") {
    throw new HTTPException(400, { message: `${field} is required` });
  }
  return found;
}

/** `null` is a real value here: it means the root. */
function optionalId(value: unknown, field: string): string | null {
  if (value === undefined || value === null) return null;
  if (typeof value !== "string" || !UUID.test(value)) {
    throw new HTTPException(400, { message: `${field} must be a ticket id` });
  }
  return value;
}

function optionalIndex(value: unknown): number | undefined {
  if (value === undefined || value === null) return undefined;
  if (typeof value !== "number" || !Number.isInteger(value) || value < 0) {
    throw new HTTPException(400, { message: "index must be a whole number" });
  }
  return value;
}

function optionalIdList(value: unknown): string[] | undefined {
  return value === undefined || value === null ? undefined : requiredIdList(value, "dependsOn");
}

function requiredIdList(value: unknown, field: string): string[] {
  if (!Array.isArray(value) || value.length === 0) {
    throw new HTTPException(400, { message: `${field} must name at least one ticket` });
  }
  return value.map((entry) => {
    if (typeof entry !== "string" || !UUID.test(entry)) {
      throw new HTTPException(400, { message: `${field} must be ticket ids` });
    }
    return entry;
  });
}

function parts(value: unknown): Array<{ title: string; body: string; sectionId?: string }> {
  if (!Array.isArray(value) || value.length < 2) {
    throw new HTTPException(400, { message: "a split needs at least two parts" });
  }
  return value.map((entry) => {
    if (!isRecord(entry)) throw new HTTPException(400, { message: "each part must be an object" });
    const sectionId = text(entry["sectionId"], "sectionId");
    return {
      title: requiredText(entry["title"], "title"),
      body: text(entry["body"], "body") ?? "",
      ...(sectionId === undefined ? {} : { sectionId }),
    };
  });
}

async function readJsonObject(c: Context): Promise<Record<string, unknown>> {
  let value: unknown;
  try {
    value = await c.req.json();
  } catch {
    throw new HTTPException(400, { message: "invalid JSON body" });
  }
  if (!isRecord(value)) throw new HTTPException(400, { message: "body must be an object" });
  return value;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}

function ticketHttpError(error: unknown): HTTPException {
  if (error instanceof HTTPException) return error;
  if (error instanceof SpecTicketError) {
    const body = { error: error.message, reason: error.code };
    switch (error.code) {
      case "not_found":
        return new HTTPException(404, { message: error.message });
      // The spec is readable, so the caller learns why the tree is not there.
      case "not_published":
      case "unknown_section":
      case "invalid":
        return httpJson(409, body);
    }
  }
  if (error instanceof SpecTicketTreeError) {
    if (error.code === "not_found") return new HTTPException(404, { message: error.message });
    // The rest are the person's mistake, not the server's: a drop onto one's
    // own child, a merge with nothing to fold in, a split of one.
    return httpJson(409, { error: error.message, reason: error.code });
  }
  throw error;
}

function httpJson(status: 404 | 409, body: unknown): HTTPException {
  return new HTTPException(status, {
    res: new Response(JSON.stringify(body), {
      status,
      headers: { "content-type": "application/json" },
    }),
  });
}
