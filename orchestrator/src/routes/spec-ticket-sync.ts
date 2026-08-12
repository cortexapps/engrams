/**
 * The Linear sync routes (ADR 0114 D6, R41-R45).
 *
 * Four verbs, all over the same ledger: read it, sync the whole tree, sync one
 * row, and choose where the tickets land. A sync request records the intent and
 * returns the ledger — it never waits for Linear, because the batch outlives the
 * request that asked for it.
 *
 * The read is deliberately not gated on a connected Linear. R42 says a missing
 * connector must never block the tree, so the ledger answers with
 * `connector.connected = false` and the reason, and the UI turns its sync
 * button into a "Connect Linear" call to action.
 */

import { Hono, type Context } from "hono";
import { HTTPException } from "hono/http-exception";

import type { LinearWorkspace } from "../integrations/linear-issues.ts";
import { LinearError } from "../integrations/linear-issues.ts";
import { SpecTicketSyncError } from "../specs/ticket-sync.ts";
import type {
  SpecTicketSyncOverrides,
  SpecTicketSyncTarget,
} from "../specs/ticket-sync.ts";
import type {
  SpecTicketSyncService,
  SpecTicketSyncView,
} from "../specs/ticket-sync-service.ts";
import type { GetSession, ResolveSpecMembership } from "./guard.ts";
import { makeSpecMemberHeaderGuard } from "./guard.ts";

const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[1-5][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/i;

export interface SpecTicketSyncPayload {
  specId: string;
  connector: { provider: string; connected: boolean; reason: string | null };
  target: {
    teamId: string | null;
    teamName: string | null;
    projectId: string | null;
    projectName: string | null;
    labelIds: string[];
    labelNames: string[];
  };
  overridden: boolean;
  total: number;
  synced: number;
  failed: number;
  inFlight: number;
  rows: Array<{
    ticketId: string;
    title: string;
    syncState: string;
    issue: { id: string; identifier: string; url: string } | null;
    error: string | null;
  }>;
}

export interface SpecTicketSyncRouteDeps {
  sync: SpecTicketSyncService;
  resolveMembership: ResolveSpecMembership;
  getSession?: GetSession;
  /** The team/project/label picker (R45). Absent means no picker is offered. */
  readWorkspace?: () => Promise<LinearWorkspace>;
}

export function makeSpecTicketSyncRoute(deps: SpecTicketSyncRouteDeps): Hono {
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

  async function respond(
    c: Context,
    read: (specId: string, body: Record<string, unknown>) => Promise<SpecTicketSyncView>,
    withBody = true,
  ): Promise<Response> {
    const specId = await requireMember(c);
    const body = withBody ? await readJsonObject(c) : {};
    try {
      return c.json(syncPayload(await read(specId, body)));
    } catch (error) {
      throw syncHttpError(error);
    }
  }

  app.get("/api/v1/specs/:id/tickets/sync", (c) =>
    respond(c, (specId) => deps.sync.read(specId), false),
  );

  app.post("/api/v1/specs/:id/tickets/sync", (c) =>
    respond(c, (specId, body) =>
      deps.sync.start({
        specId,
        ticketIds: optionalIdList(body["ticketIds"]),
        overrides: overrides(body["target"]),
      }),
    ),
  );

  // One row. This is the Retry button of the failed row in the ledger.
  app.post("/api/v1/specs/:id/tickets/:ticketId/sync", (c) =>
    respond(
      c,
      (specId) => deps.sync.start({ specId, ticketIds: [requireTicketId(c)] }),
      false,
    ),
  );

  // The picker. It calls Linear, so it is the one route here that can be slow.
  app.get("/api/v1/specs/:id/tickets/sync/targets", async (c) => {
    await requireMember(c);
    if (!deps.readWorkspace) {
      return c.json({ teams: [], projects: [], labels: [] });
    }
    try {
      return c.json(await deps.readWorkspace());
    } catch (error) {
      throw syncHttpError(error);
    }
  });

  return app;
}

function requireTicketId(c: Context): string {
  const ticketId = c.req.param("ticketId");
  if (typeof ticketId !== "string" || !UUID.test(ticketId)) {
    throw new HTTPException(404, { message: "not found" });
  }
  return ticketId;
}

export function syncPayload(view: SpecTicketSyncView): SpecTicketSyncPayload {
  return {
    specId: view.specId,
    connector: view.connector,
    target: targetPayload(view.target),
    overridden: view.overridden,
    total: view.total,
    synced: view.synced,
    failed: view.failed,
    inFlight: view.inFlight,
    rows: view.rows.map((row) => ({
      ticketId: row.ticketId,
      title: row.title,
      syncState: row.syncState,
      issue: row.issue ? { ...row.issue } : null,
      error: row.error,
    })),
  };
}

function targetPayload(target: SpecTicketSyncTarget): SpecTicketSyncPayload["target"] {
  return {
    teamId: target.teamId,
    teamName: target.teamName,
    projectId: target.projectId,
    projectName: target.projectName,
    labelIds: [...target.labelIds],
    labelNames: [...target.labelNames],
  };
}

/** The per-spec override a person chose at sync time (R45). */
function overrides(value: unknown): SpecTicketSyncOverrides | undefined {
  if (value === undefined || value === null) return undefined;
  if (!isRecord(value)) throw new HTTPException(400, { message: "target must be an object" });
  const labelIds = value["labelIds"];
  const labelNames = value["labelNames"];
  return {
    ...(value["teamId"] === undefined ? {} : { teamId: nullableText(value["teamId"], "teamId") }),
    ...(value["teamName"] === undefined
      ? {}
      : { teamName: nullableText(value["teamName"], "teamName") }),
    ...(value["projectId"] === undefined
      ? {}
      : { projectId: nullableText(value["projectId"], "projectId") }),
    ...(value["projectName"] === undefined
      ? {}
      : { projectName: nullableText(value["projectName"], "projectName") }),
    ...(labelIds === undefined ? {} : { labelIds: textList(labelIds, "labelIds") }),
    ...(labelNames === undefined ? {} : { labelNames: textList(labelNames, "labelNames") }),
  };
}

function nullableText(value: unknown, field: string): string | null {
  if (value === null) return null;
  if (typeof value !== "string") {
    throw new HTTPException(400, { message: `${field} must be a string or null` });
  }
  return value === "" ? null : value;
}

function textList(value: unknown, field: string): string[] {
  if (!Array.isArray(value)) throw new HTTPException(400, { message: `${field} must be a list` });
  return value.map((entry) => {
    if (typeof entry !== "string") {
      throw new HTTPException(400, { message: `${field} must be a list of strings` });
    }
    return entry;
  });
}

function optionalIdList(value: unknown): string[] | undefined {
  if (value === undefined || value === null) return undefined;
  if (!Array.isArray(value) || value.length === 0) {
    throw new HTTPException(400, { message: "ticketIds must name at least one ticket" });
  }
  return value.map((entry) => {
    if (typeof entry !== "string" || !UUID.test(entry)) {
      throw new HTTPException(400, { message: "ticketIds must be ticket ids" });
    }
    return entry;
  });
}

async function readJsonObject(c: Context): Promise<Record<string, unknown>> {
  const raw = await c.req.text();
  if (raw.trim() === "") return {};
  let value: unknown;
  try {
    value = JSON.parse(raw);
  } catch {
    throw new HTTPException(400, { message: "invalid JSON body" });
  }
  if (!isRecord(value)) throw new HTTPException(400, { message: "body must be an object" });
  return value;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}

/**
 * A refusal a person can act on.
 *
 * "Linear is not connected" and "no team is chosen" are both 409 with a reason
 * code, because the request was well formed and the spec is readable — what is
 * missing is a setup step the UI can point at.
 */
function syncHttpError(error: unknown): HTTPException {
  if (error instanceof HTTPException) return error;
  if (error instanceof SpecTicketSyncError) {
    if (error.code === "not_found") return new HTTPException(404, { message: error.message });
    return httpJson(409, { error: error.message, reason: error.code });
  }
  if (error instanceof LinearError) {
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
