/**
 * Native ProfileService implementation (ADR 0052).
 *
 * Orchestrator-native (like TaskService): registered on the ConnectRouter,
 * NEVER proxied. Self-gates with the same CASL machinery as everything else.
 *
 * Field-level filtering (ADR §6): non-admin callers get env_vars stripped;
 * include_archived is admin-only. Mutations are admin-only.
 *
 * Image integrity (ADR §3): create/update validate image_id against the
 * coordinator's enabled-image catalog and reject an absent/disabled id.
 *
 * Injectable deps (getSession, store, images) for tests.
 */

import { ConnectError, Code } from "@connectrpc/connect";
import type { ConnectRouter, HandlerContext } from "@connectrpc/connect";

import { ProfileService } from "../gen/engram/app/v1/profile_pb.ts";
import type { Profile } from "../gen/engram/app/v1/profile_pb.ts";

import { abilityFor } from "../authz/ability.ts";
import { auth } from "../auth/better-auth.ts";
import { getDb } from "../db/client.ts";
import { makeProfileStore, type ProfileRow, type ProfileStore } from "../db/profiles.ts";
import { images as defaultImages } from "../control-plane/client.ts";
import {
  defaultCatalog,
  selectableSkillNames,
  type MountCatalogClient,
} from "../skills/catalog.ts";
import { parseCapability, grantsCapability } from "../connectors/registry.ts";

/** Subset of ImageService client used here (catalog validation). */
export interface ImagesClient {
  listEnabledImages(req: Record<string, never>): Promise<{
    images: Array<{ id: string; imageUri: string }>;
  }>;
}

export type GetSession = (
  headers: Headers,
) => Promise<{ user: { id: string; role?: string | null; email?: string | null } } | null>;

export interface ProfileDeps {
  getSession?: GetSession;
  store?: ProfileStore;
  images?: ImagesClient;
  mountCatalog?: MountCatalogClient;
}

function headersOf(ctx: HandlerContext): Headers {
  return ctx.requestHeader;
}

async function requireUser(ctx: HandlerContext, getSession: GetSession): Promise<{ id: string; role: string }> {
  const session = await getSession(headersOf(ctx));
  if (!session) throw new ConnectError("unauthenticated", Code.Unauthenticated);
  return { id: session.user.id, role: session.user.role ?? "user" };
}

/** Map a ProfileRow to the proto Profile. env_vars included only when admin. */
function toProto(row: ProfileRow, isAdmin: boolean): Profile {
  return {
    id: row.id,
    name: row.name,
    description: row.description,
    icon: row.icon,
    imageId: row.imageId,
    includeUserTokens: row.includeUserTokens,
    envVars: isAdmin ? row.envVars : {},
    // ADR 0055: skills are not sensitive (they describe granted tooling), so
    // they are surfaced to members too — unlike env_vars.
    skills: row.skills,
    // ADR 0056: capabilities likewise describe granted access (not secrets),
    // so they are member-visible.
    capabilities: row.capabilities,
    archived: row.deletedAt != null,
    createdAt: row.createdAt.toISOString(),
    updatedAt: row.updatedAt.toISOString(),
  } as Profile;
}

export function registerProfiles(router: ConnectRouter, deps?: ProfileDeps): void {
  const getSession: GetSession =
    deps?.getSession ??
    ((headers) => auth.api.getSession({ headers } as Parameters<typeof auth.api.getSession>[0]));
  const store: ProfileStore = deps?.store ?? makeProfileStore(getDb());
  const images: ImagesClient = deps?.images ?? (defaultImages as unknown as ImagesClient);
  const mountCatalog: MountCatalogClient = deps?.mountCatalog ?? defaultCatalog();

  /** Validate image_id against the live catalog; throw InvalidArgument if absent. */
  async function assertImageEnabled(imageId: string): Promise<void> {
    const resp = await images.listEnabledImages({});
    if (!resp.images.some((i) => i.id === imageId)) {
      throw new ConnectError("image_id is not an enabled image", Code.InvalidArgument);
    }
  }

  /**
   * ADR 0055 P2: validate selected skills against builtins ∪ the live upload
   * catalog (like image_id). The coordinator re-checks at session-create, but
   * failing here keeps a profile from ever referencing a skill the editor
   * wouldn't have offered.
   */
  async function assertSkillsValid(skills: string[]): Promise<void> {
    if (skills.length === 0) return;
    const allowed = await selectableSkillNames(mountCatalog);
    const unknown = skills.filter((s) => !allowed.has(s));
    if (unknown.length > 0) {
      throw new ConnectError(
        `unknown skill(s): ${unknown.join(", ")}`,
        Code.InvalidArgument,
      );
    }
  }

  /**
   * ADR 0056 (B′): validate each capability is (1) a well-formed
   * `provider:action[@resource]` string (mirrors
   * engram_core::types::Capability::parse) and (2) actually *granted* by a
   * connector — the editor offers only what a connector grants. The coordinator
   * re-validates authoritatively at session-create; rejecting here keeps a
   * profile from ever storing a grant no connector backs.
   */
  function assertCapabilitiesValid(capabilities: string[]): void {
    for (const c of capabilities) {
      const parsed = parseCapability(c);
      if (!parsed) {
        throw new ConnectError(
          `invalid capability "${c}": expected "provider:action[@resource]"`,
          Code.InvalidArgument,
        );
      }
      if (!grantsCapability(parsed.provider, parsed.action)) {
        throw new ConnectError(
          `capability "${c}" is not granted by any connector ` +
            `(no operation grants "${parsed.provider}:${parsed.action}")`,
          Code.InvalidArgument,
        );
      }
    }
  }

  router.service(ProfileService, {
    async listProfiles(req, ctx) {
      const user = await requireUser(ctx, getSession);
      const ability = abilityFor(user);
      if (!ability.can("read", "Profile")) throw new ConnectError("forbidden", Code.PermissionDenied);
      const isAdmin = user.role === "admin";
      // include_archived is admin-only; silently forced false for members.
      const includeArchived = isAdmin && req.includeArchived;
      const rows = await store.list({ includeArchived });
      return { profiles: rows.map((r) => toProto(r, isAdmin)) };
    },

    async getProfile(req, ctx) {
      const user = await requireUser(ctx, getSession);
      const ability = abilityFor(user);
      if (!ability.can("read", "Profile")) throw new ConnectError("forbidden", Code.PermissionDenied);
      const isAdmin = user.role === "admin";
      const row = await store.get(req.id);
      // Members never see archived profiles.
      if (!row || (row.deletedAt != null && !isAdmin)) {
        throw new ConnectError("not found", Code.NotFound);
      }
      return { profile: toProto(row, isAdmin) };
    },

    async createProfile(req, ctx) {
      const user = await requireUser(ctx, getSession);
      const ability = abilityFor(user);
      if (!ability.can("manage", "Profile")) throw new ConnectError("forbidden", Code.PermissionDenied);
      if (!req.name.trim()) throw new ConnectError("name is required", Code.InvalidArgument);
      await assertImageEnabled(req.imageId);
      await assertSkillsValid(req.skills ?? []);
      assertCapabilitiesValid(req.capabilities ?? []);
      const row = await store.create({
        name: req.name,
        description: req.description,
        icon: req.icon || "Bot",
        imageId: req.imageId,
        includeUserTokens: req.includeUserTokens,
        envVars: req.envVars ?? {},
        skills: req.skills ?? [],
        capabilities: req.capabilities ?? [],
      });
      return { profile: toProto(row, true) };
    },

    async updateProfile(req, ctx) {
      const user = await requireUser(ctx, getSession);
      const ability = abilityFor(user);
      if (!ability.can("manage", "Profile")) throw new ConnectError("forbidden", Code.PermissionDenied);
      if (!req.name.trim()) throw new ConnectError("name is required", Code.InvalidArgument);
      await assertImageEnabled(req.imageId);
      await assertSkillsValid(req.skills ?? []);
      assertCapabilitiesValid(req.capabilities ?? []);
      const row = await store.update(req.id, {
        name: req.name,
        description: req.description,
        icon: req.icon || "Bot",
        imageId: req.imageId,
        includeUserTokens: req.includeUserTokens,
        envVars: req.envVars ?? {},
        skills: req.skills ?? [],
        capabilities: req.capabilities ?? [],
      });
      if (!row) throw new ConnectError("not found", Code.NotFound);
      return { profile: toProto(row, true) };
    },

    async deleteProfile(req, ctx) {
      const user = await requireUser(ctx, getSession);
      const ability = abilityFor(user);
      if (!ability.can("manage", "Profile")) throw new ConnectError("forbidden", Code.PermissionDenied);
      await store.softDelete(req.id);
      return {};
    },
  });
}
