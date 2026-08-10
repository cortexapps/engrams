/** Read-only data access for the Tech Specs list. */

import { and, count, desc, eq, inArray, isNull, or } from "drizzle-orm";
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

export type SpecLifecycle = "draft" | "published";
export type TicketSyncState = "none" | "pending" | "synced" | "failed";

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
  lifecycle: SpecLifecycle;
  participants: SpecListParticipant[];
  openQuestionCount: number;
  ticketSyncState: TicketSyncState;
  updatedAt: Date;
}

export interface SpecListOptions {
  orgId: string;
  lifecycle?: SpecLifecycle;
  page: number;
  pageSize: number;
}

export interface SpecListStore {
  isMember(userId: string): Promise<boolean>;
  list(options: SpecListOptions): Promise<{ rows: SpecListRow[]; totalCount: number }>;
}

function repoLabel(policy: TaskLaunchPolicy | null): string | null {
  const repo = policy?.repos[0];
  if (!repo) return null;
  if (repo.remote) return `${repo.remote.owner}/${repo.remote.name}`;
  const pathName = repo.path.split("/").filter(Boolean).at(-1);
  return pathName ?? null;
}

export function makeSpecListStore(db: NodePgDatabase<typeof schema> = getDb()): SpecListStore {
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
        ...(options.lifecycle ? [eq(spec.lifecycle, options.lifecycle)] : []),
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
          lifecycle: spec.lifecycle,
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

      const [participantRows, questionRows, launchRows] = await Promise.all([
        db
          .select({
            specId: specParticipant.specId,
            id: user.id,
            name: user.name,
            email: user.email,
            connectedAt: specParticipant.connectedAt,
          })
          .from(specParticipant)
          .innerJoin(user, eq(user.id, specParticipant.userId))
          .where(
            and(
              inArray(specParticipant.specId, specIds),
              isNull(specParticipant.disconnectedAt),
              or(eq(user.banned, false), isNull(user.banned)),
            ),
          )
          .orderBy(specParticipant.connectedAt),
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

      const participants = new Map<string, Map<string, SpecListParticipant>>();
      for (const row of participantRows) {
        const byUser = participants.get(row.specId) ?? new Map();
        byUser.set(row.id, { id: row.id, name: row.name, email: row.email });
        participants.set(row.specId, byUser);
      }
      const questionCounts = new Map(questionRows.map((row) => [row.specId, Number(row.value)]));
      const repos = new Map(launchRows.map((row) => [row.sessionId, repoLabel(row.launchPolicy)]));

      return {
        rows: specRows.map((row) => ({
          id: row.id,
          title: row.title,
          templateName: row.templateName,
          repo: row.sessionId ? (repos.get(row.sessionId) ?? null) : null,
          lifecycle: row.lifecycle === "published" ? "published" : "draft",
          participants: [...(participants.get(row.id)?.values() ?? [])],
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
