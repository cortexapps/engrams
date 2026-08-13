/**
 * Create a spec and the session that drafts it (ADR 0114 D3).
 *
 * Three properties shape this module.
 *
 * **The template is locked at creation.** The spec stores `template_id`, and
 * the session gets the deep-cloned snapshot that `snapshotForNewSpec` returns.
 * A later edit of the template therefore cannot change a running spec: its
 * structure, its done criteria, and its process stages are the ones that the
 * person chose.
 *
 * **A spec session is an ordinary session.** This module does not fork the
 * create path; it calls the one primitive (`createTaskWithSession`) through the
 * injected `startSession`, with the task type "spec". That type alone selects
 * the spec tool manifest and the spec-mode system prompt.
 *
 * **A repeated create does not create a second spec.** The `spec` primary key
 * is the idempotency ledger. Every reserved id — the spec, its task, and its
 * session — is derived from the caller's organization and idempotency key, so a
 * retry reaches the same row, `ON CONFLICT DO NOTHING` refuses the second
 * insert, and the caller reads back the first result. `spec.session_id` is a
 * logical reference with no foreign key, so the reserved session id is written
 * with the row: a concurrent duplicate never observes a spec without a session.
 * This copies the ledger discipline of `tools/coordination.ts` (reserve ids,
 * insert-or-nothing, re-read, refuse a reused key with different arguments)
 * without its table, whose primary key needs a caller session that a create
 * from the Tech Specs page does not have.
 */

import { createHash } from "node:crypto";
import { Code, ConnectError } from "@connectrpc/connect";
import { createTemplateDocument } from "@engrams/spec-document";
import { eq } from "drizzle-orm";

import { spec } from "../db/schema.ts";
import { log as rootLog } from "../log.ts";
import { truncatePrompt, type Db } from "../rpc/task-create.ts";
import { encodeProseMirrorDocument, type SpecDocumentService } from "./doc-service.ts";
import type { SpecTemplateCatalog, SpecTemplateSnapshot } from "./template-catalog.ts";

const log = rootLog.child({ component: "spec-create" });

/** Shown when the person gives neither a title nor a usable problem statement. */
export const UNTITLED_SPEC = "Untitled spec";

/** The row that a create reserves before the session boots. */
export interface ReservedSpecRow {
  id: string;
  orgId: string;
  ownerUserId: string;
  sessionId: string;
  templateId: string;
  title: string;
}

/** The parts of an existing row that a replayed create compares and returns. */
export interface ExistingSpecRow {
  id: string;
  ownerUserId: string | null;
  sessionId: string | null;
  templateId: string;
  title: string;
}

export interface SpecCreateStore {
  /** Insert the reserved row. False means the id was already taken. */
  insertIfAbsent(row: ReservedSpecRow): Promise<boolean>;
  read(specId: string): Promise<ExistingSpecRow | null>;
  /** Release a reservation whose session never booted. */
  delete(specId: string): Promise<void>;
}

/** What the session leg needs. `startSession` owns the ordinary create path. */
export interface SpecSessionInput {
  taskId: string;
  sessionId: string;
  ownerUserId: string;
  /** The owner is a service-account principal (an ADR 0086 API key). It picks
   *  the programmatic credential, because such a principal has no per-user
   *  harness token (ADR 0063 B4). */
  ownerIsServiceAccount?: boolean;
  profileId: string;
  title: string;
  prompt: string;
  specTemplate: SpecTemplateSnapshot;
}

export interface SpecCreationDeps {
  store: SpecCreateStore;
  catalog: Pick<SpecTemplateCatalog, "snapshotForNewSpec">;
  documents: Pick<SpecDocumentService, "applyUpdate">;
  startSession: (input: SpecSessionInput) => Promise<void>;
}

export interface CreateSpecRequest {
  orgId: string;
  ownerUserId: string;
  /** The caller is a service-account principal. Carried to the session so an
   *  API-key create compiles the programmatic credential path. */
  ownerIsServiceAccount?: boolean;
  /** The client's stable key for this create. It derives every reserved id. */
  idempotencyKey: string;
  profileId: string;
  templateId: string;
  problemStatement: string;
  title?: string;
}

export interface CreateSpecResult {
  specId: string;
  sessionId: string;
  title: string;
  templateId: string;
  /** False when the call replayed a create that the same key already made. */
  created: boolean;
}

/**
 * Create one spec, its initial document, and its drafting session.
 *
 * The order is deliberate. The reservation comes first, because it is the
 * idempotency gate. The document comes next, so the projection has content to
 * publish when the sandbox boots. The session comes last, because it is the
 * only step that leaves state outside this orchestrator. A failure after the
 * reservation releases the row, which lets a retry with the same key start
 * again; a release that itself fails is logged and never masks the original
 * error, exactly as `createTaskWithSession` handles its own compensation.
 */
export async function createSpec(
  deps: SpecCreationDeps,
  request: CreateSpecRequest,
): Promise<CreateSpecResult> {
  const snapshot = await deps.catalog.snapshotForNewSpec(request.orgId, request.templateId);
  if (!snapshot) {
    throw new ConnectError("spec template not found", Code.NotFound);
  }

  const title = specTitle(request.title, request.problemStatement);
  const specId = derivedUuid("spec", request.orgId, request.idempotencyKey);
  const taskId = derivedUuid("spec-task", request.orgId, request.idempotencyKey);
  const sessionId = derivedUuid("spec-session", request.orgId, request.idempotencyKey);

  const reserved = await deps.store.insertIfAbsent({
    id: specId,
    orgId: request.orgId,
    ownerUserId: request.ownerUserId,
    sessionId,
    templateId: snapshot.templateId,
    title,
  });
  if (!reserved) {
    return replayed(deps, specId, request, snapshot.templateId, title);
  }

  try {
    await deps.documents.applyUpdate(specId, initialDocument(specId, snapshot), null);
    await deps.startSession({
      taskId,
      sessionId,
      ownerUserId: request.ownerUserId,
      ...(request.ownerIsServiceAccount ? { ownerIsServiceAccount: true } : {}),
      profileId: request.profileId,
      title,
      prompt: request.problemStatement,
      specTemplate: snapshot,
    });
  } catch (error) {
    await releaseReservation(deps, specId);
    throw error;
  }

  return { specId, sessionId, title, templateId: snapshot.templateId, created: true };
}

/**
 * Return the result that the first create with this key produced.
 *
 * The stored row carries the durable part of the original request, so a key
 * reused for a different spec is refused rather than answered with the wrong
 * spec. This is the `request_hash` rule of the coordination ledger, applied to
 * the fields that the `spec` row itself holds.
 */
async function replayed(
  deps: SpecCreationDeps,
  specId: string,
  request: CreateSpecRequest,
  templateId: string,
  title: string,
): Promise<CreateSpecResult> {
  const existing = await deps.store.read(specId);
  if (!existing) {
    throw new ConnectError("the reserved spec disappeared during creation", Code.Aborted);
  }
  if (
    existing.ownerUserId !== request.ownerUserId ||
    existing.templateId !== templateId ||
    existing.title !== title
  ) {
    throw new ConnectError(
      "idempotency key was already used with different arguments",
      Code.AlreadyExists,
    );
  }
  if (!existing.sessionId) {
    throw new ConnectError("the spec has no session", Code.FailedPrecondition);
  }
  return {
    specId,
    sessionId: existing.sessionId,
    title: existing.title,
    templateId: existing.templateId,
    created: false,
  };
}

async function releaseReservation(deps: SpecCreationDeps, specId: string): Promise<void> {
  try {
    await deps.store.delete(specId);
  } catch (error) {
    log.error(
      { specId, err: error },
      "spec-create: failed to release the reserved spec row — manual cleanup needed",
    );
  }
}

/**
 * The document that the template defines, with a stable id for each section.
 *
 * ADR 0114 D2 anchors on node identity, so the ids are assigned here, once.
 * They are derived from the spec id and the section key, which keeps a replayed
 * seed byte-identical to the first one.
 */
function initialDocument(specId: string, snapshot: SpecTemplateSnapshot): Uint8Array {
  return encodeProseMirrorDocument(
    createTemplateDocument({
      sections: snapshot.sections.map((section) => ({
        id: derivedUuid("spec-section", specId, section.key),
        key: section.key,
        title: section.title,
      })),
    }),
  );
}

/** The spec title, which the session title follows (R4). The problem statement
 *  names the spec when the person gives no title. */
function specTitle(title: string | undefined, problemStatement: string): string {
  const given = truncatePrompt(title);
  return given ?? truncatePrompt(problemStatement) ?? UNTITLED_SPEC;
}

/**
 * A version-5-shaped UUID over the given parts.
 *
 * Each part carries its length, so no two different part lists can produce the
 * same input string.
 */
function derivedUuid(...parts: readonly string[]): string {
  const digest = createHash("sha256")
    .update(parts.map((part) => `${part.length}:${part}`).join(" "))
    .digest();
  digest[6] = (digest[6]! & 0x0f) | 0x50;
  digest[8] = (digest[8]! & 0x3f) | 0x80;
  const hex = digest.subarray(0, 16).toString("hex");
  return [
    hex.slice(0, 8),
    hex.slice(8, 12),
    hex.slice(12, 16),
    hex.slice(16, 20),
    hex.slice(20, 32),
  ].join("-");
}

export function makeSpecCreateStore(db: Db): SpecCreateStore {
  return {
    async insertIfAbsent(row) {
      const inserted = await db
        .insert(spec)
        .values({ ...row, phase: "ideation" })
        .onConflictDoNothing()
        .returning({ id: spec.id });
      return inserted.length === 1;
    },

    async read(specId) {
      const rows = await db
        .select({
          id: spec.id,
          ownerUserId: spec.ownerUserId,
          sessionId: spec.sessionId,
          templateId: spec.templateId,
          title: spec.title,
        })
        .from(spec)
        .where(eq(spec.id, specId))
        .limit(1);
      return rows[0] ?? null;
    },

    async delete(specId) {
      await db.delete(spec).where(eq(spec.id, specId));
    },
  };
}
