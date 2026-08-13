/** Read-only data access for the Tech Specs list. */

import { and, count, desc, eq, inArray, isNull, or, sql } from "drizzle-orm";
import type { NodePgDatabase } from "drizzle-orm/node-postgres";

import { getDb } from "./client.ts";
import * as schema from "./schema.ts";
import {
  spec,
  specOpenQuestion,
  specParticipant,
  specTemplate,
  task,
  taskSession,
  user,
  type TaskLaunchPolicy,
} from "./schema.ts";

export type SpecPhase = "ideation" | "drafting" | "published";
export type TicketSyncState = "none" | "pending" | "synced" | "failed";

const SPEC_PARTICIPANT_SAMPLE_SIZE = 3;

export interface SpecListParticipant {
  id: string;
  name: string;
  email: string;
}

export interface SpecListRow {
  id: string;
  title: string;
  templateName: string;
  repo: string | null;
  phase: SpecPhase;
  participants: SpecListParticipant[];
  activeParticipantCount: number;
  openQuestionCount: number;
  ticketSyncState: TicketSyncState;
  updatedAt: Date;
}

export interface SpecListOptions {
  orgId: string;
  phase?: SpecPhase;
  page: number;
  pageSize: number;
}

export interface SpecListStore {
  isMember(userId: string): Promise<boolean>;
  list(options: SpecListOptions): Promise<{ rows: SpecListRow[]; totalCount: number }>;
}

interface ParticipantSampleRow {
  [key: string]: unknown;
  spec_id: string;
  id: string;
  name: string;
  email: string;
  active_participant_count: number;
}

function repoLabel(policy: TaskLaunchPolicy | null): string | null {
  const repo = policy?.repos[0];
  if (!repo) return null;
  if (repo.remote) return `${repo.remote.owner}/${repo.remote.name}`;
  const pathName = repo.path.split("/").filter(Boolean).at(-1);
  return pathName ?? null;
}

export function makeSpecListStore(
  db: NodePgDatabase<typeof schema> = getDb(),
  now: () => Date = () => new Date(),
): SpecListStore {
  return {
    async isMember(userId) {
      const rows = await db
        .select({ id: user.id })
        .from(user)
        .where(and(eq(user.id, userId), or(eq(user.banned, false), isNull(user.banned))))
        .limit(1);
      return rows.length === 1;
    },

    async list(options) {
      const conditions = [
        eq(spec.orgId, options.orgId),
        ...(options.phase ? [eq(spec.phase, options.phase)] : []),
      ];
      const where = and(...conditions);
      const totalRows = await db.select({ value: count() }).from(spec).where(where);
      const totalCount = Number(totalRows[0]?.value ?? 0);

      const specRows = await db
        .select({
          id: spec.id,
          title: spec.title,
          templateName: specTemplate.name,
          sessionId: spec.sessionId,
          phase: spec.phase,
          updatedAt: spec.updatedAt,
        })
        .from(spec)
        .innerJoin(specTemplate, eq(specTemplate.id, spec.templateId))
        .where(where)
        .orderBy(desc(spec.updatedAt), desc(spec.id))
        .limit(options.pageSize)
        .offset((options.page - 1) * options.pageSize);

      if (specRows.length === 0) return { rows: [], totalCount };

      const specIds = specRows.map((row) => row.id);
      const sessionIds = specRows
        .map((row) => row.sessionId)
        .filter((id): id is string => id != null);
      const activeAt = now();

      const [participantResult, questionRows, launchRows] = await Promise.all([
        db.execute<ParticipantSampleRow>(sql`
          WITH active_participant AS (
            SELECT DISTINCT ON (participant.spec_id, participant.user_id)
                   participant.spec_id,
                   member.id,
                   member.name,
                   member.email,
                   participant.connected_at AS first_connected_at
              FROM spec_participant AS participant
              JOIN "user" AS member ON member.id = participant.user_id
             WHERE participant.spec_id IN (
               ${sql.join(
                 specIds.map((specId) => sql`${specId}`),
                 sql`, `,
               )}
             )
               AND participant.disconnected_at IS NULL
               AND participant.lease_expires_at > ${activeAt}
               AND (member.banned = false OR member.banned IS NULL)
             ORDER BY participant.spec_id,
                      participant.user_id,
                      participant.connected_at,
                      participant.client_id
          ), ranked_participant AS (
            SELECT spec_id,
                   id,
                   name,
                   email,
                   count(*) OVER (PARTITION BY spec_id)::int AS active_participant_count,
                   row_number() OVER (
                     PARTITION BY spec_id
                     ORDER BY first_connected_at, id
                   ) AS sample_rank
              FROM active_participant
          )
          SELECT spec_id, id, name, email, active_participant_count
            FROM ranked_participant
           WHERE sample_rank <= ${SPEC_PARTICIPANT_SAMPLE_SIZE}
           ORDER BY spec_id, sample_rank
        `),
        db
          .select({ specId: specOpenQuestion.specId, value: count() })
          .from(specOpenQuestion)
          .where(and(inArray(specOpenQuestion.specId, specIds), eq(specOpenQuestion.state, "open")))
          .groupBy(specOpenQuestion.specId),
        sessionIds.length === 0
          ? Promise.resolve([])
          : db
              .select({
                sessionId: taskSession.sessionId,
                launchPolicy: task.launchPolicy,
              })
              .from(taskSession)
              .innerJoin(task, eq(task.id, taskSession.taskId))
              .where(inArray(taskSession.sessionId, sessionIds)),
      ]);

      const participants = new Map<string, SpecListParticipant[]>();
      const activeParticipantCounts = new Map<string, number>();
      for (const row of participantResult.rows) {
        const sample = participants.get(row.spec_id) ?? [];
        sample.push({ id: row.id, name: row.name, email: row.email });
        participants.set(row.spec_id, sample);
        activeParticipantCounts.set(row.spec_id, row.active_participant_count);
      }
      const questionCounts = new Map(questionRows.map((row) => [row.specId, Number(row.value)]));
      const repos = new Map(launchRows.map((row) => [row.sessionId, repoLabel(row.launchPolicy)]));

      return {
        rows: specRows.map((row) => ({
          id: row.id,
          title: row.title,
          templateName: row.templateName,
          repo: row.sessionId ? (repos.get(row.sessionId) ?? null) : null,
          phase: specPhase(row.phase, row.id),
          participants: participants.get(row.id) ?? [],
          activeParticipantCount: activeParticipantCounts.get(row.id) ?? 0,
          openQuestionCount: questionCounts.get(row.id) ?? 0,
          // The ticket-tree issue adds its storage. The list contract is ready
          // now, and specs without ticket drafts have no sync state.
          ticketSyncState: "none",
          updatedAt: row.updatedAt,
        })),
        totalCount,
      };
    },
  };
}

function specPhase(value: string, specId: string): SpecPhase {
  if (value === "ideation" || value === "drafting" || value === "published") return value;
  throw new Error(`Spec ${specId} has an invalid phase: ${value}`);
}
