import { Hono, type Context } from "hono";
import { HTTPException } from "hono/http-exception";
import type { Pool } from "pg";

import type { GetSession, ResolveSpecMembership } from "./guard.ts";
import { makeSpecMemberHeaderGuard } from "./guard.ts";

const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[1-5][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/i;
export const UNKNOWN_ACTOR_NAME = "actor unknown";

export interface SpecDecisionActor {
  id: string | null;
  /**
   * The person who made the decision, or `UNKNOWN_ACTOR_NAME`.
   *
   * Two different situations produce an unknown actor, and the schema cannot
   * tell them apart: a decision made before the attribution columns existed,
   * and a decision whose account was deleted — both foreign keys are
   * ON DELETE SET NULL, so removing an account erases the id as well. The
   * label therefore claims only what is true of both, that nobody can be
   * named. Snapshotting the name at write time, the way spec_chat_message
   * does, is what would keep the credit through a deletion.
   */
  name: string;
}

export type SpecDecision =
  | {
      id: string;
      kind: "section_settled";
      sectionId: string;
      sectionTitle: string;
      actor: SpecDecisionActor;
      decidedAt: Date;
    }
  | {
      id: string;
      kind: "question_resolved";
      sectionId: string;
      sectionTitle: string;
      question: string;
      resolutionLink: string;
      actor: SpecDecisionActor;
      decidedAt: Date;
    };

export interface SpecDecisionStore {
  listPublishedDecisions(specId: string): Promise<SpecDecision[]>;
}

interface SpecDecisionRow {
  id: string;
  kind: "section_settled" | "question_resolved";
  section_id: string;
  section_title: string;
  question: string | null;
  resolution_link: string | null;
  actor_user_id: string | null;
  actor_name: string;
  decided_at: Date;
}

/** Read settles and question resolutions that are part of the published pin. */
export class PostgresSpecDecisionStore implements SpecDecisionStore {
  constructor(private readonly pool: Pool) {}

  async listPublishedDecisions(specId: string): Promise<SpecDecision[]> {
    const result = await this.pool.query<SpecDecisionRow>(
      `WITH published AS (
         SELECT checkpoint.created_at AS cutoff
           FROM spec
           JOIN spec_checkpoint AS checkpoint
             ON checkpoint.id = spec.published_checkpoint_id
            AND checkpoint.spec_id = spec.id
          WHERE spec.id = $1
            AND spec.phase = 'published'
       ), decisions AS (
         SELECT action.id,
                'section_settled'::text AS kind,
                action.section_id,
                action.chip->>'sectionTitle' AS section_title,
                NULL::text AS question,
                NULL::text AS resolution_link,
                coalesce(action.actor_user_id, settle.settled_by) AS actor_user_id,
                coalesce(actor.name, settler.name, $2) AS actor_name,
                action.created_at AS decided_at
           FROM spec_transcript_action AS action
           JOIN published ON action.created_at <= published.cutoff
           LEFT JOIN "user" AS actor ON actor.id = action.actor_user_id
           -- spec_section_state.settled_by has recorded the settling person
           -- since the section-state table existed, while action.actor_user_id
           -- arrived later and was not backfilled. Read the older column when
           -- the newer one is empty, or the decisions card claims nobody
           -- settled a section that the section list credits by name. Only the
           -- LATEST settle carries this fallback: settled_by is current state,
           -- not per-action history, so attributing an earlier settle to it
           -- could name the wrong person.
           LEFT JOIN LATERAL (
             SELECT state.settled_by
               FROM spec_section_state AS state
              WHERE state.spec_id = action.spec_id
                AND state.section_id = action.section_id
                AND state.state = 'settled'
                AND action.id = (
                  SELECT latest.id
                    FROM spec_transcript_action AS latest
                   WHERE latest.spec_id = action.spec_id
                     AND latest.section_id = action.section_id
                     AND latest.chip->>'kind' = 'spec_section_state_changed'
                     AND latest.chip->'after'->>'state' = 'settled'
                     AND latest.created_at <= published.cutoff
                   ORDER BY latest.created_at DESC, latest.id DESC
                   LIMIT 1
                )
           ) AS settle ON true
           LEFT JOIN "user" AS settler ON settler.id = settle.settled_by
          WHERE action.spec_id = $1
            AND action.chip->>'kind' = 'spec_section_state_changed'
            AND action.chip->'after'->>'state' = 'settled'
         UNION ALL
         SELECT question.id::text,
                'question_resolved'::text AS kind,
                question.section_id,
                coalesce(section_action.section_title, question.section_id) AS section_title,
                question.text AS question,
                question.resolution_note AS resolution_link,
                question.resolved_by AS actor_user_id,
                coalesce(actor.name, $2) AS actor_name,
                question.resolved_at AS decided_at
           FROM spec_open_question AS question
           JOIN published
             ON question.resolved_at IS NOT NULL
            AND question.resolved_at <= published.cutoff
           LEFT JOIN "user" AS actor ON actor.id = question.resolved_by
           LEFT JOIN LATERAL (
             SELECT action.chip->>'sectionTitle' AS section_title
               FROM spec_transcript_action AS action
              WHERE action.spec_id = question.spec_id
                AND action.section_id = question.section_id
                AND action.chip->>'kind' = 'spec_section_state_changed'
                AND action.created_at <= published.cutoff
              ORDER BY action.created_at DESC, action.id DESC
              LIMIT 1
           ) AS section_action ON true
          WHERE question.spec_id = $1
            AND question.state = 'resolved'
       )
       SELECT id, kind, section_id, section_title, question, resolution_link,
              actor_user_id, actor_name, decided_at
         FROM decisions
        ORDER BY decided_at, kind, id`,
      [specId, UNKNOWN_ACTOR_NAME],
    );
    return result.rows.map(decisionFromRow);
  }
}

export interface SpecDecisionsRouteDeps {
  store: SpecDecisionStore;
  resolveMembership: ResolveSpecMembership;
  getSession?: GetSession;
}

export function makeSpecDecisionsRoute(deps: SpecDecisionsRouteDeps): Hono {
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

  app.get("/api/v1/specs/:id/decisions", async (c) => {
    const decisions = await deps.store.listPublishedDecisions(await requireMember(c));
    return c.json({
      decisions: decisions.map((decision) => ({
        id: decision.id,
        kind: decision.kind,
        sectionId: decision.sectionId,
        sectionTitle: decision.sectionTitle,
        ...(decision.kind === "question_resolved"
          ? { question: decision.question, resolutionLink: decision.resolutionLink }
          : {}),
        actor: decision.actor,
        decidedAt: decision.decidedAt.toISOString(),
      })),
    });
  });

  return app;
}

function decisionFromRow(row: SpecDecisionRow): SpecDecision {
  const common = {
    id: row.id,
    sectionId: row.section_id,
    sectionTitle: row.section_title,
    actor: { id: row.actor_user_id, name: row.actor_name },
    decidedAt: row.decided_at,
  };
  if (row.kind === "section_settled") {
    return { ...common, kind: row.kind };
  }
  return {
    ...common,
    kind: row.kind,
    question: row.question ?? "",
    resolutionLink: row.resolution_link ?? "",
  };
}
