/**
 * Session-app data-access seam (ADR 0118, replacing ADR 0064's port-exposure
 * store).
 *
 * The injectable seam the apps CRUD route (routes/apps.ts), the create path
 * (rpc/task-create.ts), and the edge reverse-proxy (routes/preview-proxy.ts)
 * depend on, and that the seam tests fake. Drizzle-backed by default. Hard
 * delete — revoking an app must stop serving it immediately.
 *
 * `createMany` is the create-path primitive and the reason apps cost nothing at
 * boot: ONE multi-row insert, callable inside a caller-supplied transaction, so
 * the reservation folds into the transaction rpc/task-create.ts already runs.
 * It replaces ADR 0064's three serial round trips per port, which additionally
 * could not run until after the session was live.
 */

import { and, desc, eq, inArray } from "drizzle-orm";

import { getDb } from "./client.ts";
import { sessionApp } from "./schema.ts";
import { generateSessionSlug, appHostLabel } from "../apps/hostname.ts";

/** "org" = any authenticated principal; "private" = owner + admin only. */
export type Visibility = "org" | "private";

export interface SessionAppRow {
  hostLabel: string;
  sessionId: string;
  name: string;
  port: number;
  ownerUserId: string;
  visibility: Visibility;
  createdAt: Date;
}

/** One app to reserve. `name` and `port` must already be validated. */
export interface SessionAppSpec {
  name: string;
  port: number;
  visibility?: Visibility;
}

/** Any Drizzle handle — the real db, or a transaction from `db.transaction`. */
type DbLike = Pick<ReturnType<typeof getDb>, "insert" | "select" | "delete">;

export interface SessionAppStore {
  /**
   * Reserve every app of a session in ONE insert. Idempotent per
   * `(sessionId, name)` and `(sessionId, port)`: a re-run inserts nothing and
   * returns the rows that are actually stored, so a retried create converges on
   * one set of hostnames.
   *
   * `tx` lets the caller fold this into a transaction it is already running.
   */
  createMany(
    sessionId: string,
    ownerUserId: string,
    apps: SessionAppSpec[],
    tx?: DbLike,
  ): Promise<SessionAppRow[]>;
  /** Reserve one app (the ad-hoc route), returning the existing row if any. */
  createOne(
    sessionId: string,
    ownerUserId: string,
    app: SessionAppSpec,
  ): Promise<SessionAppRow>;
  /** All apps of a session, newest first. */
  listBySession(sessionId: string): Promise<SessionAppRow[]>;
  /** Resolve a host label → app (the edge proxy's lookup), or null. */
  getByHostLabel(hostLabel: string): Promise<SessionAppRow | null>;
  /** Hard-delete one app by host label; true if a row was removed. */
  deleteByHostLabel(hostLabel: string): Promise<boolean>;
  /** Hard-delete every app of a session (teardown). Returns the count. */
  deleteBySession(sessionId: string): Promise<number>;
}

function toRow(r: typeof sessionApp.$inferSelect): SessionAppRow {
  return {
    hostLabel: r.hostLabel,
    sessionId: r.sessionId,
    name: r.name,
    port: r.port,
    ownerUserId: r.ownerUserId,
    visibility: r.visibility as Visibility,
    createdAt: r.createdAt,
  };
}

export function makeSessionAppStore(
  db: ReturnType<typeof getDb> = getDb(),
): SessionAppStore {
  async function selectBySession(sessionId: string): Promise<SessionAppRow[]> {
    const rows = await db
      .select()
      .from(sessionApp)
      .where(eq(sessionApp.sessionId, sessionId))
      .orderBy(desc(sessionApp.createdAt));
    return rows.map(toRow);
  }

  const store: SessionAppStore = {
    async createMany(sessionId, ownerUserId, apps, tx) {
      if (apps.length === 0) return [];
      const handle = tx ?? db;
      // One slug for the whole session: N apps cost one random draw, and their
      // hostnames read as a related set (`web-…`, `api-…`).
      const slug = generateSessionSlug();
      const inserted = await handle
        .insert(sessionApp)
        .values(
          apps.map((a) => ({
            hostLabel: appHostLabel(a.name, slug),
            sessionId,
            name: a.name,
            port: a.port,
            ownerUserId,
            visibility: a.visibility ?? "org",
          })),
        )
        // A retried create (or a duplicate name/port inside one declaration)
        // must not fail the session — the rows that are already there are the
        // truth, and the select below reports them.
        .onConflictDoNothing()
        .returning();

      if (inserted.length === apps.length) return inserted.map(toRow);
      // Something already existed. Report what is actually stored for the names
      // asked for, so the caller builds env from real hostnames.
      const names = apps.map((a) => a.name);
      const rows = await handle
        .select()
        .from(sessionApp)
        .where(and(eq(sessionApp.sessionId, sessionId), inArray(sessionApp.name, names)));
      return rows.map(toRow);
    },

    async createOne(sessionId, ownerUserId, app) {
      const [row] = await store.createMany(sessionId, ownerUserId, [app]);
      if (row) return row;
      // createMany returned nothing only if the row exists under a different
      // name/port pairing than asked for; surface the stored one.
      const existing = (await selectBySession(sessionId)).find(
        (r) => r.name === app.name || r.port === app.port,
      );
      if (!existing) throw new Error(`session app: could not reserve "${app.name}"`);
      return existing;
    },

    listBySession: selectBySession,

    async getByHostLabel(hostLabel) {
      const rows = await db
        .select()
        .from(sessionApp)
        .where(eq(sessionApp.hostLabel, hostLabel))
        .limit(1);
      return rows[0] ? toRow(rows[0]) : null;
    },

    async deleteByHostLabel(hostLabel) {
      const deleted = await db
        .delete(sessionApp)
        .where(eq(sessionApp.hostLabel, hostLabel))
        .returning({ hostLabel: sessionApp.hostLabel });
      return deleted.length > 0;
    },

    async deleteBySession(sessionId) {
      const deleted = await db
        .delete(sessionApp)
        .where(eq(sessionApp.sessionId, sessionId))
        .returning({ hostLabel: sessionApp.hostLabel });
      return deleted.length;
    },
  };

  return store;
}
