/**
 * Port-exposure data-access seam (ADR 0064).
 *
 * The injectable seam the ports CRUD route (routes/ports.ts) and the edge
 * reverse-proxy (P2b) depend on, and that the seam tests fake. Drizzle-backed by
 * default. Hard delete (revoking must stop serving immediately).
 */

import { and, desc, eq } from "drizzle-orm";

import { getDb } from "./client.ts";
import { portExposure } from "./schema.ts";
import { generateSlug } from "../ports/slug.ts";

export type Visibility = "private" | "shared";

export interface PortExposureRow {
  slug: string;
  sessionId: string;
  port: number;
  label: string;
  ownerUserId: string;
  visibility: Visibility;
  shareToken: string | null;
  createdAt: Date;
  expiresAt: Date | null;
}

export interface PortExposureInput {
  sessionId: string;
  port: number;
  label: string;
  ownerUserId: string;
  visibility: Visibility;
}

/** The seam injected into the ports route + the edge proxy. */
export interface PortExposureStore {
  /**
   * Create an exposure, or return the existing one for this `(sessionId, port)`
   * (idempotent re-expose). Mints a fresh slug, retrying on the rare PK
   * collision; mints a `shareToken` iff `visibility === "shared"`.
   */
  createOrGet(input: PortExposureInput): Promise<PortExposureRow>;
  /** All exposures for a session, newest first. */
  listBySession(sessionId: string): Promise<PortExposureRow[]>;
  /** Resolve a slug → exposure (the edge proxy's lookup), or null. */
  getBySlug(slug: string): Promise<PortExposureRow | null>;
  /** Hard-delete one exposure by slug; true if a row was removed. */
  deleteBySlug(slug: string): Promise<boolean>;
}

function toRow(r: typeof portExposure.$inferSelect): PortExposureRow {
  return {
    slug: r.slug,
    sessionId: r.sessionId,
    port: r.port,
    label: r.label,
    ownerUserId: r.ownerUserId,
    visibility: r.visibility as Visibility,
    shareToken: r.shareToken,
    createdAt: r.createdAt,
    expiresAt: r.expiresAt,
  };
}

/** Postgres unique-violation SQLSTATE — distinguishes a slug/(session,port)
 * collision (retry/idempotent) from a real failure. */
function isUniqueViolation(e: unknown): boolean {
  return typeof e === "object" && e !== null && (e as { code?: string }).code === "23505";
}

/** A strong, URL-safe capability token for `visibility = "shared"` links. */
function mintShareToken(): string {
  return Buffer.from(crypto.getRandomValues(new Uint8Array(24))).toString("base64url");
}

export function makePortExposureStore(
  db: ReturnType<typeof getDb> = getDb(),
): PortExposureStore {
  async function getBySessionPort(
    sessionId: string,
    port: number,
  ): Promise<PortExposureRow | null> {
    const rows = await db
      .select()
      .from(portExposure)
      .where(and(eq(portExposure.sessionId, sessionId), eq(portExposure.port, port)))
      .limit(1);
    return rows[0] ? toRow(rows[0]) : null;
  }

  const store: PortExposureStore = {
    async createOrGet(input) {
      // Idempotent: an existing (session, port) exposure wins (same slug).
      const existing = await getBySessionPort(input.sessionId, input.port);
      if (existing) return existing;

      const shareToken = input.visibility === "shared" ? mintShareToken() : null;

      for (let attempt = 0; attempt < 5; attempt++) {
        const slug = generateSlug();
        try {
          await db.insert(portExposure).values({
            slug,
            sessionId: input.sessionId,
            port: input.port,
            label: input.label,
            ownerUserId: input.ownerUserId,
            visibility: input.visibility,
            shareToken,
          });
          const row = await this.getBySlug(slug);
          return row!;
        } catch (e) {
          if (!isUniqueViolation(e)) throw e;
          // Either a slug PK collision (retry with a fresh slug) or a concurrent
          // insert of the same (session, port) (return the winner).
          const raced = await getBySessionPort(input.sessionId, input.port);
          if (raced) return raced;
        }
      }
      throw new Error("port exposure: could not mint a unique slug after 5 attempts");
    },

    async listBySession(sessionId) {
      const rows = await db
        .select()
        .from(portExposure)
        .where(eq(portExposure.sessionId, sessionId))
        .orderBy(desc(portExposure.createdAt));
      return rows.map(toRow);
    },

    async getBySlug(slug) {
      const rows = await db
        .select()
        .from(portExposure)
        .where(eq(portExposure.slug, slug))
        .limit(1);
      return rows[0] ? toRow(rows[0]) : null;
    },

    async deleteBySlug(slug) {
      const deleted = await db
        .delete(portExposure)
        .where(eq(portExposure.slug, slug))
        .returning({ slug: portExposure.slug });
      return deleted.length > 0;
    },
  };

  return store;
}
