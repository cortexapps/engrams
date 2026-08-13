import type {
  RestoreSectionStateUndo,
  SectionState,
  SectionStateValue,
} from "@engrams/spec-document";
import { Hono, type Context } from "hono";
import { HTTPException } from "hono/http-exception";
import type { Pool } from "pg";

import type { SpecTemplateLayer, SpecTemplateSection } from "../db/schema.ts";
import { proseMirrorDocument, type SpecDocumentService } from "../specs/doc-service.ts";
import {
  SectionStateConflictError,
  SectionStateReadOnlyError,
  type SectionStateService,
} from "../specs/section-state-service.ts";
import { SectionStateTransitionError } from "../specs/section-state.ts";
import type { GetSession, ResolveSpecMembership } from "./guard.ts";
import { makeSpecMemberHeaderGuard } from "./guard.ts";

const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[1-5][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/i;

export interface SpecRailMetadata {
  lifecycle: "draft" | "published";
  layers: SpecTemplateLayer[];
  sections: SpecTemplateSection[];
  states: ReadonlyMap<
    string,
    SectionStateValue & {
      settledBy?: { id: string; name: string } | null;
      stateChangedAt?: Date;
    }
  >;
  openQuestionCounts: ReadonlyMap<string, number>;
}

export interface SpecRailSection {
  id: string;
  templateKey: string;
  title: string;
  state: SectionState;
  naReason: string | null;
  allowNa: boolean;
  openQuestionCount: number;
  settledBy: { id: string; name: string } | null;
  stateChangedAt: string | null;
}

export interface SpecRail {
  sections: SpecRailSection[];
  completeness: { complete: number; total: number };
}

export interface SpecRailStore {
  readMetadata(specId: string): Promise<SpecRailMetadata | null>;
}

interface RailTemplateRow {
  lifecycle: string;
  layers: SpecTemplateLayer[];
  sections: SpecTemplateSection[];
}

interface RailStateRow {
  section_id: string;
  state: SectionState;
  na_reason: string | null;
  settled_by: string | null;
  settled_by_name: string | null;
  updated_at: Date;
}

interface OpenQuestionCountRow {
  section_id: string;
  count: string;
}

export class PostgresSpecRailStore implements SpecRailStore {
  constructor(private readonly pool: Pool) {}

  async readMetadata(specId: string): Promise<SpecRailMetadata | null> {
    const [templateResult, stateResult, questionResult] = await Promise.all([
      this.pool.query<RailTemplateRow>(
        `SELECT spec.lifecycle, template.layers, template.sections
           FROM spec
           JOIN spec_template AS template ON template.id = spec.template_id
          WHERE spec.id = $1`,
        [specId],
      ),
      this.pool.query<RailStateRow>(
        `SELECT state.section_id, state.state, state.na_reason, state.settled_by,
                member.name AS settled_by_name, state.updated_at
           FROM spec_section_state AS state
           LEFT JOIN "user" AS member ON member.id = state.settled_by
          WHERE state.spec_id = $1`,
        [specId],
      ),
      this.pool.query<OpenQuestionCountRow>(
        `SELECT section_id, count(*)::text AS count
           FROM spec_open_question
          WHERE spec_id = $1 AND state = 'open'
          GROUP BY section_id`,
        [specId],
      ),
    ]);
    const template = templateResult.rows[0];
    if (!template) return null;
    if (template.lifecycle !== "draft" && template.lifecycle !== "published") {
      throw new Error(`Spec ${specId} has an invalid lifecycle: ${template.lifecycle}`);
    }
    return {
      lifecycle: template.lifecycle,
      layers: template.layers,
      sections: template.sections,
      states: new Map(
        stateResult.rows.map((row) => [
          row.section_id,
          {
            state: row.state,
            naReason: row.na_reason,
            settledBy:
              row.state === "settled" && row.settled_by !== null
                ? { id: row.settled_by, name: row.settled_by_name ?? "Unknown member" }
                : null,
            stateChangedAt: row.updated_at,
          },
        ]),
      ),
      openQuestionCounts: new Map(
        questionResult.rows.map((row) => [row.section_id, Number(row.count)]),
      ),
    };
  }
}

export interface SpecRailRouteDeps {
  store: SpecRailStore;
  documents: Pick<SpecDocumentService, "syncFromLog">;
  sectionStates: Pick<SectionStateService, "transitionDeferred" | "undoDeferred">;
  resolveMembership: ResolveSpecMembership;
  getSession?: GetSession;
}

export function makeSpecRailRoute(deps: SpecRailRouteDeps): Hono {
  const app = new Hono();
  const authorize = makeSpecMemberHeaderGuard(deps.resolveMembership, deps.getSession);

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

  app.get("/api/v1/specs/:id/rail", async (c) => {
    const { specId } = await requireMember(c);
    const metadata = await requireMetadata(deps.store, specId);
    return c.json({ rail: await readRail(deps.documents, metadata, specId) });
  });

  app.post("/api/v1/specs/:id/sections/:sectionId/state", async (c) => {
    const { specId, userId } = await requireMember(c);
    const metadata = await requireMetadata(deps.store, specId);
    const sectionId = requireSectionId(c.req.param("sectionId"));
    const body = await readJsonObject(c);
    const actionId = requireActionId(body["actionId"]);
    const target = requireTargetState(body["state"]);
    const reason = body["reason"];
    if (reason !== undefined && typeof reason !== "string") {
      throw new HTTPException(400, { message: "reason must be a string" });
    }

    const rail = await readRail(deps.documents, metadata, specId);
    const context = stateContext(rail, specId, sectionId);
    try {
      const change = await deps.sectionStates.transitionDeferred({
        actionId: `rail:${specId}:${actionId}`,
        context,
        target,
        ...(reason === undefined ? {} : { naReason: reason }),
        actorUserId: userId,
      });
      const current = await requireMetadata(deps.store, specId);
      return c.json({
        chip: change.transcriptChip,
        rail: await readRail(deps.documents, current, specId),
      });
    } catch (error) {
      throw stateActionError(error);
    }
  });

  app.post("/api/v1/specs/:id/sections/:sectionId/undo", async (c) => {
    const { specId, userId } = await requireMember(c);
    const metadata = await requireMetadata(deps.store, specId);
    const sectionId = requireSectionId(c.req.param("sectionId"));
    const body = await readJsonObject(c);
    const actionId = requireActionId(body["actionId"]);
    const undo = requireUndo(body["undo"]);
    if (undo.specId !== specId || undo.sectionId !== sectionId) {
      throw new HTTPException(400, { message: "undo does not match this spec section" });
    }

    const rail = await readRail(deps.documents, metadata, specId);
    const context = stateContext(rail, specId, sectionId);
    try {
      const change = await deps.sectionStates.undoDeferred({
        actionId: `rail:${specId}:${actionId}`,
        context,
        undo,
        actorUserId: userId,
      });
      const current = await requireMetadata(deps.store, specId);
      return c.json({
        chip: change.transcriptChip,
        rail: await readRail(deps.documents, current, specId),
      });
    } catch (error) {
      throw stateActionError(error);
    }
  });

  return app;
}

async function readRail(
  documents: Pick<SpecDocumentService, "syncFromLog">,
  metadata: SpecRailMetadata,
  specId: string,
): Promise<SpecRail> {
  const loaded = await documents.syncFromLog(specId);
  const document = proseMirrorDocument(loaded.doc);
  const rules = new Map(metadata.sections.map((section) => [section.key, section]));
  const ordered: SpecRailSection[] = [];
  document.forEach((section) => {
    const id = section.attrs["id"];
    const templateKey = section.attrs["templateSectionKey"];
    if (typeof id !== "string" || typeof templateKey !== "string" || !section.firstChild) {
      throw new Error("Every spec section must keep its stable template identity.");
    }
    const rule = rules.get(templateKey);
    if (!rule) throw new Error(`Spec section ${id} has no template rule.`);
    const value = metadata.states.get(id);
    const entry: SpecRailSection = {
      id,
      templateKey,
      title: section.firstChild.textContent,
      state: value?.state ?? "open",
      naReason: value?.naReason ?? null,
      allowNa: rule.allowNa,
      openQuestionCount: metadata.openQuestionCounts.get(id) ?? 0,
      settledBy: value?.state === "settled" ? (value.settledBy ?? null) : null,
      stateChangedAt: value?.stateChangedAt?.toISOString() ?? null,
    };
    ordered.push(entry);
  });
  return {
    sections: ordered,
    completeness: {
      complete: ordered.filter(sectionIsComplete).length,
      total: ordered.length,
    },
  };
}

function stateContext(rail: SpecRail, specId: string, sectionId: string) {
  const section = rail.sections.find((candidate) => candidate.id === sectionId);
  if (!section) throw new HTTPException(404, { message: "section not found" });
  return {
    specId,
    sectionId,
    sectionTitle: section.title,
    allowsNa: section.allowNa,
  };
}

function sectionIsComplete(section: Pick<SpecRailSection, "state">): boolean {
  return section.state === "settled" || section.state === "n/a";
}

async function requireMetadata(store: SpecRailStore, specId: string): Promise<SpecRailMetadata> {
  const metadata = await store.readMetadata(specId);
  if (!metadata) throw new HTTPException(404, { message: "not found" });
  return metadata;
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

function requireSectionId(value: string): string {
  if (value.length === 0 || value.length > 200) {
    throw new HTTPException(404, { message: "section not found" });
  }
  return value;
}

function requireActionId(value: unknown): string {
  if (typeof value !== "string" || !UUID.test(value)) {
    throw new HTTPException(400, { message: "actionId must be a UUID" });
  }
  return value;
}

function requireTargetState(value: unknown): "proposed" | "settled" | "open" | "n/a" {
  if (value !== "proposed" && value !== "settled" && value !== "open" && value !== "n/a") {
    throw new HTTPException(400, { message: "state must be proposed, settled, open, or n/a" });
  }
  return value;
}

function requireUndo(value: unknown): RestoreSectionStateUndo {
  if (!isRecord(value) || value["kind"] !== "restore_section_state") {
    throw new HTTPException(400, { message: "undo must restore a section state" });
  }
  const specId = value["specId"];
  const sectionId = value["sectionId"];
  if (typeof specId !== "string" || !UUID.test(specId) || typeof sectionId !== "string") {
    throw new HTTPException(400, { message: "undo has an invalid section identity" });
  }
  return {
    kind: "restore_section_state",
    specId,
    sectionId,
    expected: requireStateValue(value["expected"]),
    restore: requireStateValue(value["restore"]),
  };
}

function requireStateValue(value: unknown): SectionStateValue {
  if (!isRecord(value)) throw new HTTPException(400, { message: "undo state is invalid" });
  const state = value["state"];
  const naReason = value["naReason"];
  if (
    (state !== "open" && state !== "proposed" && state !== "settled" && state !== "n/a") ||
    (naReason !== null && typeof naReason !== "string")
  ) {
    throw new HTTPException(400, { message: "undo state is invalid" });
  }
  return { state, naReason };
}

function stateActionError(error: unknown): HTTPException {
  if (error instanceof HTTPException) return error;
  if (error instanceof SectionStateTransitionError) {
    return new HTTPException(400, { message: error.message });
  }
  if (error instanceof SectionStateConflictError) {
    return new HTTPException(409, { message: error.message });
  }
  if (error instanceof SectionStateReadOnlyError) {
    return new HTTPException(409, { message: error.message });
  }
  throw error;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}
