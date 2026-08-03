/**
 * Native ApiKeyService — global + CLI API keys (ADR 0086).
 *
 * Orchestrator-native (like OrgSecretService), registered on the ConnectRouter
 * before the passthrough so it owns the ApiKeyService prefix.
 *
 * Two ownership shapes share the `apikey` table and every auth seam:
 *
 *   - GLOBAL keys (admin-only Create/List/Revoke): each owned by a dedicated
 *     SERVICE-ACCOUNT user row (`apikey+<uuid>@service.local`, no `account`
 *     row → structurally un-log-in-able) whose `role` ('admin'|'user') is the
 *     key's authorization level. Revocation deletes the service user; the key
 *     row dies by FK cascade.
 *
 *   - CLI keys (CreateCliKey/RevokeCliKey, any authenticated human): owned by
 *     the CALLING user directly, so a keyed request resolves to that user
 *     with their own role and ownership. This is the durable credential
 *     `engrams auth login` stores after the device-flow exchange. Revocation
 *     deletes ONLY the key row — the owner is a human.
 *
 * The @better-auth/api-key plugin resolves a valid keyed request into a
 * (mock) session for the owning user, so every existing seam (CASL ability,
 * policy map, `createdByUserId` ownership) works unchanged for both shapes.
 *
 * The key material is SHA-256-hashed at rest by the plugin; the plaintext is
 * returned exactly once from Create*. The plugin's own HTTP endpoints are
 * 404'd in better-auth.ts — this service is the only management path.
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
import { requireAdmin } from "./require.ts";
import { getDb } from "../db/client.ts";
import { apikey, user } from "../db/schema.ts";

export type GetSession = (
  headers: Headers,
) => Promise<{
  user: { id: string; role?: string | null; email?: string | null; name?: string | null };
} | null>;

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

/** One listed key: the apikey row joined with its owning user. */
export interface ApiKeyRow {
  id: string;
  name: string | null;
  role: string | null;
  start: string | null;
  prefix: string | null;
  createdAt: Date;
  expiresAt: Date | null;
  lastRequest: Date | null;
  /** Owner email — @service.local marks a global key; human = CLI key. */
  ownerEmail: string;
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
  createServiceUser(input: { name: string; role: string }): Promise<{ id: string; email: string }>;
  /** Delete a user row (cascade removes the key row). Idempotent. */
  deleteUser(userId: string): Promise<void>;
  /** Mint a key for `userId` via the plugin (hashes at rest, returns plaintext once). */
  mintKey(input: { userId: string; name: string; expiresIn?: number }): Promise<MintedKey>;
  /** All keys joined with their owning user (service-account or human). */
  listKeys(): Promise<ApiKeyRow[]>;
  /** The key row + owner email, or null. */
  findKey(id: string): Promise<{ referenceId: string; email: string } | null>;
  /** Delete just the key row (CLI-key revocation — the owner is a human). */
  deleteKeyRow(id: string): Promise<void>;
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
      const email = serviceEmail();
      // internalAdapter.createUser (the iap-bridge jitCreateSession precedent):
      // generates the id/timestamps and runs databaseHooks — the bootstrap-admin
      // hook is a no-op for @service.local emails. No `account` row is created,
      // so the service user has no sign-in path.
      const created = await ctx.internalAdapter.createUser({
        email,
        name,
        emailVerified: true,
        role,
      });
      return { id: created.id, email };
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
          ownerEmail: user.email,
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

    async deleteKeyRow(id) {
      await db.delete(apikey).where(eq(apikey.id, id));
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


/** Any authenticated HUMAN user (the CLI-key RPCs). A session resolved from a
 *  service-account key is rejected — a global key must not launder itself
 *  into user-owned keys that would survive its revocation. */
async function requireHumanUser(
  ctx: HandlerContext,
  getSession: GetSession,
): Promise<{ id: string; role: string }> {
  const session = await getSession(ctx.requestHeader);
  if (!session) throw new ConnectError("unauthenticated", Code.Unauthenticated);
  if (isServiceAccountEmail(session.user.email ?? "")) {
    throw new ConnectError(
      "CLI keys cannot be minted or revoked with a service-account key",
      Code.PermissionDenied,
    );
  }
  return { id: session.user.id, role: session.user.role ?? "user" };
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
    ownerEmail: row.ownerEmail,
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
          ownerEmail: serviceUser.email,
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
      if (isServiceAccountEmail(found.email)) {
        // Global key: the service account exists only to own it — delete the
        // user; the key row cascades.
        await backend().deleteUser(found.referenceId);
      } else {
        // User-owned CLI key: the owner is a HUMAN — delete only the key row.
        await backend().deleteKeyRow(req.id);
      }
      return { revoked: true };
    },

    async createCliKey(req, ctx) {
      const caller = await requireHumanUser(ctx, getSession);
      const name = req.name.trim() || "cli";
      // Non-expiring, like gh's stored OAuth token — revoke (logout / the
      // admin panel) is the kill switch.
      const minted = await backend().mintKey({ userId: caller.id, name });
      return {
        meta: metaOf({
          id: minted.id,
          name,
          role: caller.role,
          start: minted.start,
          prefix: minted.prefix,
          createdAt: minted.createdAt,
          expiresAt: minted.expiresAt,
          lastRequest: null,
          // Listing joins the real owner email; the mint response doesn't
          // need to re-fetch it — the caller knows who they are.
          ownerEmail: "",
        }),
        key: minted.key,
      };
    },

    async revokeCliKey(req, ctx) {
      const caller = await requireHumanUser(ctx, getSession);
      const found = await backend().findKey(req.id);
      // Absent and not-yours read identically (anti-enumeration), and the
      // response stays idempotent for logout retries.
      if (!found || found.referenceId !== caller.id) return { revoked: false };
      await backend().deleteKeyRow(req.id); // never the user — the owner is a human
      return { revoked: true };
    },

    async whoAmI(_req, ctx) {
      // Any authenticated principal — service accounts included (an admin CI
      // key legitimately asks "who am I acting as").
      const session = await getSession(ctx.requestHeader);
      if (!session) throw new ConnectError("unauthenticated", Code.Unauthenticated);
      const email = session.user.email ?? "";
      return {
        userId: session.user.id,
        email,
        name: session.user.name ?? "",
        role: session.user.role ?? "user",
        serviceAccount: isServiceAccountEmail(email),
      };
    },
  });
}
