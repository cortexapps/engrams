import { Code, ConnectError } from "@connectrpc/connect";
import { Hono, type Context } from "hono";
import { HTTPException } from "hono/http-exception";
import type { Pool } from "pg";
import * as Y from "yjs";

import { getSessionFromHeaders } from "../auth/session.ts";
import { abilityFor } from "../authz/ability.ts";
import type { GetSession, ResolveSpecMembership } from "./guard.ts";
import { makeSpecMemberHeaderGuard } from "./guard.ts";
import {
  type RestoreSectionResult,
  type SpecCheckpointRecord,
  type SpecCheckpointService,
  type SpecCheckpointStore,
} from "../specs/checkpoints.ts";
import type { CreateSpecRequest, CreateSpecResult } from "../specs/create.ts";
import { proseMirrorDocument, SpecDocumentReadOnlyError } from "../specs/doc-service.ts";

const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[1-5][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/i;
const SPEC_CHECKPOINT_LIST_LIMIT = 100;
const SPEC_TITLE_MAX_CHARS = 200;
const SPEC_PROBLEM_MAX_CHARS = 20_000;
const IDEMPOTENCY_KEY_MAX_CHARS = 200;

export interface SpecReadRecord {
  id: string;
  title: string;
  lifecycle: "draft" | "published";
  ownerUserId: string | null;
  sessionId: string | null;
  publishedCheckpointId: string | null;
  publishedAt: Date | null;
  currentSemanticDocSeq: bigint;
  /** The template the spec locked at creation (ADR 0114 D3). */
  templateId: string;
  templateName: string;
}

export interface SpecCheckpointSummary {
  id: string;
  label: string;
  authorUserId: string | null;
  authorName: string | null;
  reason: string;
  docSeq: bigint;
  createdAt: Date;
}

export interface SpecReadStore {
  readSpec(specId: string): Promise<SpecReadRecord | null>;
  listCheckpoints(specId: string): Promise<SpecCheckpointSummary[]>;
}

interface SpecRow {
  id: string;
  title: string;
  lifecycle: string;
  owner_user_id: string | null;
  session_id: string | null;
  published_checkpoint_id: string | null;
  published_at: Date | null;
  current_semantic_doc_seq: string;
  template_id: string;
  template_name: string;
}

interface CheckpointSummaryRow {
  id: string;
  label: string;
  author_user_id: string | null;
  author_name: string | null;
  reason: string;
  doc_seq: string;
  created_at: Date;
}

/** Read spec metadata and history without contacting the coordinator. */
export class PostgresSpecReadStore implements SpecReadStore {
  constructor(private readonly pool: Pool) {}

  async readSpec(specId: string): Promise<SpecReadRecord | null> {
    const result = await this.pool.query<SpecRow>(
      `SELECT spec.id, spec.title, spec.lifecycle, spec.owner_user_id, spec.session_id,
              spec.published_checkpoint_id, spec.published_at, spec.current_semantic_doc_seq,
              spec.template_id, spec_template.name AS template_name
         FROM spec
         JOIN spec_template ON spec_template.id = spec.template_id
        WHERE spec.id = $1`,
      [specId],
    );
    const row = result.rows[0];
    if (!row) return null;
    if (row.lifecycle !== "draft" && row.lifecycle !== "published") {
      throw new Error(`Spec ${specId} has an invalid lifecycle: ${row.lifecycle}`);
    }
    return {
      id: row.id,
      title: row.title,
      lifecycle: row.lifecycle,
      ownerUserId: row.owner_user_id,
      sessionId: row.session_id,
      publishedCheckpointId: row.published_checkpoint_id,
      publishedAt: row.published_at,
      currentSemanticDocSeq: BigInt(row.current_semantic_doc_seq),
      templateId: row.template_id,
      templateName: row.template_name,
    };
  }

  async listCheckpoints(specId: string): Promise<SpecCheckpointSummary[]> {
    const result = await this.pool.query<CheckpointSummaryRow>(
      `SELECT checkpoint.id, checkpoint.label, checkpoint.author_user_id,
              author.name AS author_name, checkpoint.reason,
              checkpoint.doc_seq, checkpoint.created_at
         FROM spec_checkpoint AS checkpoint
         LEFT JOIN "user" AS author ON author.id = checkpoint.author_user_id
        WHERE checkpoint.spec_id = $1
        ORDER BY checkpoint.created_at DESC, checkpoint.id DESC
        LIMIT $2`,
      [specId, SPEC_CHECKPOINT_LIST_LIMIT],
    );
    return result.rows.map((row) => ({
      id: row.id,
      label: row.label,
      authorUserId: row.author_user_id,
      authorName: row.author_name,
      reason: row.reason,
      docSeq: BigInt(row.doc_seq),
      createdAt: row.created_at,
    }));
  }
}

export interface SpecsRouteDeps {
  store: SpecReadStore;
  checkpointStore: SpecCheckpointStore;
  checkpoints: Pick<SpecCheckpointService, "restoreSection">;
  resolveMembership: ResolveSpecMembership;
  /** The organization that owns every spec this deployment serves. */
  orgId: string;
  create: (request: CreateSpecRequest) => Promise<CreateSpecResult>;
  getSession?: GetSession;
}

export function makeSpecsRoute(deps: SpecsRouteDeps): Hono {
  const app = new Hono();
  const authorize = makeSpecMemberHeaderGuard(deps.resolveMembership, deps.getSession);
  const getSession = deps.getSession ?? getSessionFromHeaders;

  async function requireMember(c: Context): Promise<{ specId: string; userId: string }> {
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
    return { specId, userId: result.user.id };
  }

  /**
   * Create a spec and the session that drafts it (R2-R6).
   *
   * Creation is REST, next to the rest of this surface, and not an rpc on the
   * read service: `rpc/specs.ts` must never activate a session, and a test
   * holds that property.
   */
  app.post("/api/v1/specs", async (c) => {
    const session = await getSession(c.req.raw.headers);
    if (!session) throw new HTTPException(401, { message: "unauthenticated" });
    const actor = { id: session.user.id, role: session.user.role ?? "user" };
    if (!abilityFor(actor).can("create", "Spec")) {
      throw new HTTPException(403, { message: "forbidden" });
    }

    let body: unknown;
    try {
      body = await c.req.json();
    } catch {
      throw new HTTPException(400, { message: "invalid JSON body" });
    }

    let result: CreateSpecResult;
    try {
      result = await deps.create({
        orgId: deps.orgId,
        ownerUserId: actor.id,
        ...createSpecInput(body),
      });
    } catch (error) {
      throw createSpecException(error);
    }

    return c.json(
      {
        spec: {
          id: result.specId,
          title: result.title,
          sessionId: result.sessionId,
          templateId: result.templateId,
          lifecycle: "draft",
        },
      },
      result.created ? 201 : 200,
    );
  });

  app.get("/api/v1/specs/:id", async (c) => {
    const { specId, userId } = await requireMember(c);
    const record = await deps.store.readSpec(specId);
    if (!record) throw new HTTPException(404, { message: "not found" });

    const summaries = await deps.store.listCheckpoints(specId);
    let publishedCheckpoint: SpecCheckpointRecord | null = null;
    if (record.lifecycle === "published") {
      if (!record.publishedCheckpointId) {
        throw new HTTPException(500, { message: "published spec has no pinned checkpoint" });
      }
      publishedCheckpoint = await deps.checkpointStore.readCheckpoint(
        specId,
        record.publishedCheckpointId,
      );
      if (!publishedCheckpoint) {
        throw new HTTPException(500, { message: "published checkpoint is missing" });
      }
    }

    return c.json({
      spec: {
        id: record.id,
        title: record.title,
        lifecycle: record.lifecycle,
        sessionId: record.ownerUserId === userId ? record.sessionId : null,
        publishedCheckpointId: record.publishedCheckpointId,
        publishedAt: record.publishedAt?.toISOString() ?? null,
        revision: record.currentSemanticDocSeq.toString(),
        // The spec locked this template when its session started (R3). The web
        // renders it as a disabled control with that reason.
        template: { id: record.templateId, name: record.templateName },
      },
      checkpoints: summaries.map(checkpointSummaryJson),
      publishedCheckpoint: publishedCheckpoint ? checkpointJson(publishedCheckpoint) : null,
    });
  });

  app.get("/api/v1/specs/:id/checkpoints/:checkpointId", async (c) => {
    const { specId } = await requireMember(c);
    const checkpointId = c.req.param("checkpointId");
    if (!UUID.test(checkpointId)) throw new HTTPException(404, { message: "not found" });
    const checkpoint = await deps.checkpointStore.readCheckpoint(specId, checkpointId);
    if (!checkpoint) throw new HTTPException(404, { message: "not found" });
    return c.json({ checkpoint: checkpointJson(checkpoint) });
  });

  app.post("/api/v1/specs/:id/restore", async (c) => {
    const { specId, userId } = await requireMember(c);
    const record = await deps.store.readSpec(specId);
    if (!record) throw new HTTPException(404, { message: "not found" });
    if (record.lifecycle !== "draft") {
      throw new HTTPException(409, { message: "published specs are read-only" });
    }

    let body: { checkpointId?: unknown; sectionId?: unknown };
    try {
      body = (await c.req.json()) as { checkpointId?: unknown; sectionId?: unknown };
    } catch {
      throw new HTTPException(400, { message: "invalid JSON body" });
    }
    const checkpointId = body.checkpointId;
    const sectionId = body.sectionId;
    if (typeof checkpointId !== "string" || !UUID.test(checkpointId)) {
      throw new HTTPException(400, { message: "checkpointId must be a UUID" });
    }
    if (typeof sectionId !== "string" || sectionId.length === 0 || sectionId.length > 200) {
      throw new HTTPException(400, { message: "sectionId must not be empty" });
    }

    let result: RestoreSectionResult;
    try {
      result = await deps.checkpoints.restoreSection(specId, checkpointId, sectionId, userId);
    } catch (error) {
      if (error instanceof SpecDocumentReadOnlyError) {
        throw new HTTPException(409, { message: "published specs are read-only" });
      }
      if (error instanceof Error && error.message.startsWith("Unknown spec checkpoint:")) {
        throw new HTTPException(404, { message: "not found" });
      }
      if (
        error instanceof Error &&
        error.message.startsWith("Checkpoint does not contain section:")
      ) {
        throw new HTTPException(400, { message: error.message });
      }
      throw error;
    }

    return c.json({
      applied: result.applied,
      checkpoint: result.applied ? checkpointJson(result.checkpointBeforeRestore) : null,
      newRev: (result.applied ? result.update.seq : result.docSeq).toString(),
    });
  });

  return app;
}

type CreateSpecInput = Omit<CreateSpecRequest, "orgId" | "ownerUserId">;

/** Validate the create body. Every rejection is a 400 with the failing field. */
function createSpecInput(body: unknown): CreateSpecInput {
  if (typeof body !== "object" || body === null || Array.isArray(body)) {
    throw new HTTPException(400, { message: "body must be an object" });
  }
  const input = body as Record<string, unknown>;
  const title = input["title"];
  if (title !== undefined && (typeof title !== "string" || title.length > SPEC_TITLE_MAX_CHARS)) {
    throw new HTTPException(400, {
      message: `title must be text of at most ${SPEC_TITLE_MAX_CHARS} characters`,
    });
  }
  return {
    templateId: uuidField(input["templateId"], "templateId"),
    profileId: uuidField(input["profileId"], "profileId"),
    problemStatement: problemStatement(input["problemStatement"]),
    idempotencyKey: idempotencyKey(input["idempotencyKey"]),
    ...(title === undefined ? {} : { title }),
  };
}

function uuidField(value: unknown, field: string): string {
  if (typeof value !== "string" || !UUID.test(value)) {
    throw new HTTPException(400, { message: `${field} must be a UUID` });
  }
  return value;
}

function problemStatement(value: unknown): string {
  if (typeof value !== "string" || value.trim() === "") {
    throw new HTTPException(400, { message: "problemStatement must not be empty" });
  }
  if (value.length > SPEC_PROBLEM_MAX_CHARS) {
    throw new HTTPException(400, {
      message: `problemStatement must be at most ${SPEC_PROBLEM_MAX_CHARS} characters`,
    });
  }
  return value;
}

function idempotencyKey(value: unknown): string {
  if (typeof value !== "string" || value === "" || value.length > IDEMPOTENCY_KEY_MAX_CHARS) {
    throw new HTTPException(400, {
      message: `idempotencyKey must be text of at most ${IDEMPOTENCY_KEY_MAX_CHARS} characters`,
    });
  }
  return value;
}

/** Map the create path's Connect codes onto this REST surface. An unknown
 *  failure keeps its own type, so it still reaches the 500 handler. */
function createSpecException(error: unknown): Error {
  if (error instanceof HTTPException) return error;
  if (!(error instanceof ConnectError)) {
    return error instanceof Error ? error : new Error(String(error));
  }
  switch (error.code) {
    case Code.NotFound:
      return new HTTPException(404, { message: error.rawMessage });
    case Code.AlreadyExists:
      return new HTTPException(409, { message: error.rawMessage });
    case Code.InvalidArgument:
      return new HTTPException(400, { message: error.rawMessage });
    case Code.FailedPrecondition:
      return new HTTPException(422, { message: error.rawMessage });
    case Code.PermissionDenied:
      return new HTTPException(403, { message: error.rawMessage });
    default:
      return error;
  }
}

function checkpointSummaryJson(checkpoint: SpecCheckpointSummary) {
  return {
    id: checkpoint.id,
    label: checkpoint.label,
    author: checkpoint.authorUserId
      ? { id: checkpoint.authorUserId, name: checkpoint.authorName ?? "Unknown member" }
      : null,
    reason: checkpoint.reason,
    docSeq: checkpoint.docSeq.toString(),
    createdAt: checkpoint.createdAt.toISOString(),
  };
}

function checkpointJson(checkpoint: SpecCheckpointRecord) {
  const doc = new Y.Doc();
  try {
    Y.applyUpdate(doc, checkpoint.state);
    const document = proseMirrorDocument(doc);
    const sections: Array<{ id: string; title: string }> = [];
    document.forEach((section) => {
      const id = section.attrs["id"];
      if (typeof id !== "string" || !section.firstChild) return;
      sections.push({ id, title: section.firstChild.textContent });
    });
    return {
      id: checkpoint.id,
      label: checkpoint.label,
      authorUserId: checkpoint.authorUserId,
      reason: checkpoint.reason,
      docSeq: checkpoint.docSeq.toString(),
      createdAt: checkpoint.createdAt.toISOString(),
      markdown: checkpoint.renderedMarkdown,
      sections,
    };
  } finally {
    doc.destroy();
  }
}
