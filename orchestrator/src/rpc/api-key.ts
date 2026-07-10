/**
 * Native ApiKeyService — admin-managed global API keys (ADR 0086).
 *
 * Orchestrator-native (like OrgSecretService), registered on the ConnectRouter
 * before the passthrough so it owns the ApiKeyService prefix. ALL RPCs are
 * admin-only.
 *
 * Identity model: each key is owned by a dedicated SERVICE-ACCOUNT user row
 * (`apikey+<uuid>@service.local`, no `account` row → structurally
 * un-log-in-able) whose `role` ('admin'|'user') is the key's authorization
 * level. The @better-auth/api-key plugin resolves a valid keyed request into a
 * mock session for that user, so every existing seam (CASL ability, policy
 * map, `createdByUserId` ownership) works unchanged. Revocation deletes the
 * service user; the key row dies by FK cascade.
 *
 * The key material is SHA-256-hashed at rest by the plugin; the plaintext is
 * returned exactly once from CreateApiKey. The plugin's own HTTP endpoints
 * are 404'd in better-auth.ts — this service is the only management path.
 *
 * Injectable deps (getSession, backend) for tests.
 */

import { ConnectError, Code } from "@connectrpc/connect";
import type { ConnectRouter, HandlerContext } from "@connectrpc/connect";
import { and, eq, like, notExists } from "drizzle-orm";

import { ApiKeyService } from "../gen/engram/app/v1/api_key_pb.ts";
import type { ApiKeyMeta } from "../gen/engram/app/v1/api_key_pb.ts";
import { auth } from "../auth/better-auth.ts";
import { getSessionFromHeaders } from "../auth/session.ts";
import { getDb } from "../db/client.ts";
import { apikey, user } from "../db/schema.ts";

export type GetSession = (
  headers: Headers,
) => Promise<{ user: { id: string; role?: string | null } } | null>;

/** The two roles a key can carry — exactly the admin plugin's user roles. */
const ROLES = new Set(["admin", "user"]);

const SERVICE_EMAIL_DOMAIN = "@service.local";

/** Owner email convention for key service accounts. */
function serviceEmail(): string {
  return `apikey+${crypto.randomUUID()}${SERVICE_EMAIL_DOMAIN}`;
}

export function isServiceAccountEmail(email: string): boolean {
  return email.startsWith("apikey+") && email.endsWith(SERVICE_EMAIL_DOMAIN);
}

/** One listed key: the apikey row joined with its service user. */
export interface ApiKeyRow {
  id: string;
  name: string | null;
  role: string | null;
  start: string | null;
  prefix: string | null;
  createdAt: Date;
  expiresAt: Date | null;
  lastRequest: Date | null;
}

/** What the plugin returns from a server-side createApiKey. */
export interface MintedKey {
  id: string;
  key: string;
  start: string | null;
  prefix: string | null;
  createdAt: Date;
  expiresAt: Date | null;
}

/**
 * Backing operations — the slice of better-auth + drizzle this service uses.
 * The default hits the real DB/plugin; tests may stub it entirely.
 */
export interface ApiKeyBackend {
  /** Create the un-log-in-able service-account user owning a key. */
  createServiceUser(input: { name: string; role: string }): Promise<{ id: string }>;
  /** Delete a user row (cascade removes the key row). Idempotent. */
  deleteUser(userId: string): Promise<void>;
  /** Mint a key for `userId` via the plugin (hashes at rest, returns plaintext once). */
  mintKey(input: { userId: string; name: string; expiresIn?: number }): Promise<MintedKey>;
  /** All keys joined with their owning service user. */
  listKeys(): Promise<ApiKeyRow[]>;
  /** The key row + owner email, or null. */
  findKey(id: string): Promise<{ referenceId: string; email: string } | null>;
  /**
   * Delete service-account users whose key row is gone — the plugin
   * auto-deletes EXPIRED key rows at verify time, orphaning the owner.
   */
  sweepOrphanServiceUsers(): Promise<void>;
}

function defaultBackend(): ApiKeyBackend {
  const db = getDb();
  return {
    async createServiceUser({ name, role }) {
      const ctx = await auth.$context;
      // internalAdapter.createUser (the iap-bridge jitCreateSession precedent):
      // generates the id/timestamps and runs databaseHooks — the bootstrap-admin
      // hook is a no-op for @service.local emails. No `account` row is created,
      // so the service user has no sign-in path.
      const created = await ctx.internalAdapter.createUser({
        email: serviceEmail(),
        name,
        emailVerified: true,
        role,
      });
      return { id: created.id };
    },

    async deleteUser(userId) {
      await db.delete(user).where(eq(user.id, userId));
    },

    async mintKey({ userId, name, expiresIn }) {
      // Server-side plugin call. MUST NOT pass headers/request: the plugin
      // treats `ctx.request || ctx.headers` as a client call and rejects
      // body.userId (UNAUTHORIZED) — headerless is the sanctioned
      // create-for-arbitrary-user path.
      const minted = await auth.api.createApiKey({
        body: { name, userId, ...(expiresIn !== undefined ? { expiresIn } : {}) },
      });
      return {
        id: minted.id,
        key: minted.key,
        start: minted.start ?? null,
        prefix: minted.prefix ?? null,
        createdAt: minted.createdAt,
        expiresAt: minted.expiresAt ?? null,
      };
    },

    async listKeys() {
      const rows = await db
        .select({
          id: apikey.id,
          name: apikey.name,
          role: user.role,
          start: apikey.start,
          prefix: apikey.prefix,
          createdAt: apikey.createdAt,
          expiresAt: apikey.expiresAt,
          lastRequest: apikey.lastRequest,
        })
        .from(apikey)
        .innerJoin(user, eq(apikey.referenceId, user.id))
        .orderBy(apikey.createdAt);
      return rows;
    },

    async findKey(id) {
      const rows = await db
        .select({ referenceId: apikey.referenceId, email: user.email })
        .from(apikey)
        .innerJoin(user, eq(apikey.referenceId, user.id))
        .where(eq(apikey.id, id))
        .limit(1);
      return rows[0] ?? null;
    },

    async sweepOrphanServiceUsers() {
      await db.delete(user).where(
        and(
          like(user.email, `apikey+%${SERVICE_EMAIL_DOMAIN}`),
          notExists(db.select({ id: apikey.id }).from(apikey).where(eq(apikey.referenceId, user.id))),
        ),
      );
    },
  };
}

export interface ApiKeyDeps {
  getSession?: GetSession;
  backend?: ApiKeyBackend;
}

async function requireAdmin(ctx: HandlerContext, getSession: GetSession): Promise<void> {
  const session = await getSession(ctx.requestHeader);
  if (!session) throw new ConnectError("unauthenticated", Code.Unauthenticated);
  if ((session.user.role ?? "user") !== "admin") {
    throw new ConnectError("forbidden", Code.PermissionDenied);
  }
}

/** The plugin's expiration floor (keyExpiration.minExpiresIn) is 1 day. */
const MIN_EXPIRY_MS = 24 * 60 * 60 * 1000;

/** Parse + validate expires_at ("" = never) → seconds-from-now, or undefined. */
function expiresInSeconds(expiresAt: string): number | undefined {
  if (expiresAt === "") return undefined;
  const t = Date.parse(expiresAt);
  if (Number.isNaN(t)) {
    throw new ConnectError("expires_at is not a valid ISO-8601 date", Code.InvalidArgument);
  }
  const ms = t - Date.now();
  if (ms < MIN_EXPIRY_MS) {
    throw new ConnectError(
      "expires_at must be at least 1 day out (revoke covers immediate kills)",
      Code.InvalidArgument,
    );
  }
  return Math.ceil(ms / 1000);
}

function metaOf(row: ApiKeyRow): Omit<ApiKeyMeta, "$typeName"> {
  return {
    id: row.id,
    name: row.name ?? "",
    role: row.role ?? "user",
    start: row.start ?? "",
    createdAt: row.createdAt.toISOString(),
    expiresAt: row.expiresAt?.toISOString() ?? "",
    lastUsedAt: row.lastRequest?.toISOString() ?? "",
  };
}

export function registerApiKeys(router: ConnectRouter, deps?: ApiKeyDeps): void {
  const getSession: GetSession = deps?.getSession ?? getSessionFromHeaders;
  // Lazy: the default backend touches getDb() at first use, keeping this module
  // importable in tests without ORCHESTRATOR_DATABASE_URL set.
  let backend0: ApiKeyBackend | undefined = deps?.backend;
  const backend = (): ApiKeyBackend => (backend0 ??= defaultBackend());

  router.service(ApiKeyService, {
    async createApiKey(req, ctx) {
      await requireAdmin(ctx, getSession);
      const name = req.name.trim();
      if (!name) throw new ConnectError("name is required", Code.InvalidArgument);
      if (!ROLES.has(req.role)) {
        throw new ConnectError('role must be "admin" or "user"', Code.InvalidArgument);
      }
      const expiresIn = expiresInSeconds(req.expiresAt);

      const serviceUser = await backend().createServiceUser({ name, role: req.role });
      let minted: MintedKey;
      try {
        minted = await backend().mintKey({ userId: serviceUser.id, name, expiresIn });
      } catch (err) {
        // No orphaned service account on plugin failure.
        await backend().deleteUser(serviceUser.id);
        throw err;
      }

      return {
        meta: metaOf({
          id: minted.id,
          name,
          role: req.role,
          start: minted.start,
          prefix: minted.prefix,
          createdAt: minted.createdAt,
          expiresAt: minted.expiresAt,
          lastRequest: null,
        }),
        key: minted.key,
      };
    },

    async listApiKeys(_req, ctx) {
      await requireAdmin(ctx, getSession);
      // Expired keys are auto-deleted by the plugin at verify time, leaving
      // their service user behind — sweep those before listing.
      await backend().sweepOrphanServiceUsers();
      const rows = await backend().listKeys();
      return { keys: rows.map(metaOf) };
    },

    async revokeApiKey(req, ctx) {
      await requireAdmin(ctx, getSession);
      const found = await backend().findKey(req.id);
      if (!found) return { revoked: false }; // idempotent
      // Defense in depth: never delete a HUMAN user through this RPC, even if
      // a hand-crafted apikey row points at one.
      if (!isServiceAccountEmail(found.email)) {
        throw new ConnectError(
          "key is not owned by a service account",
          Code.FailedPrecondition,
        );
      }
      await backend().deleteUser(found.referenceId); // cascade removes the key row
      return { revoked: true };
    },
  });
}
