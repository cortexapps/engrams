/**
 * Orchestrator database schema — ADR 0039 §3/§10.
 *
 * Task model only. better-auth tables join in Task 16.
 */

import {
  pgTable,
  text,
  jsonb,
  timestamp,
  primaryKey,
  index,
} from "drizzle-orm/pg-core";

export const task = pgTable("task", {
  id: text("id").primaryKey(), // nanoid/uuid
  type: text("type").notNull(), // 'chat' only for now
  title: text("title"),
  status: text("status").notNull().default("open"), // open|working|awaiting_review|done|failed
  createdByUserId: text("created_by_user_id"), // better-auth user id; null = automation (future)
  source: jsonb("source"), // type-specific trigger ref
  workflowRunId: text("workflow_run_id"), // DBOS run — null for chat (ADR §4)
  createdAt: timestamp("created_at").notNull().defaultNow(),
  updatedAt: timestamp("updated_at").notNull().defaultNow(),
});

export const taskSession = pgTable(
  "task_session",
  {
    taskId: text("task_id")
      .notNull()
      .references(() => task.id, { onDelete: "cascade" }),
    sessionId: text("session_id").notNull(), // control-plane session id
    role: text("role"), // nullable until multi-session types exist
    createdAt: timestamp("created_at").notNull().defaultNow(),
  },
  (t) => [
    primaryKey({ columns: [t.taskId, t.sessionId] }),
    index("task_session_session_idx").on(t.sessionId), // the authz join (ADR §6) hits this
  ],
);
