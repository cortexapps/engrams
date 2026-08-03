/**
 * Native MountCatalogService implementation (ADR 0055 P2).
 *
 * Orchestrator-native (like ProfileService / TaskService), registered on the
 * ConnectRouter — NOT a coordinator passthrough, because the orchestrator gates
 * upload/delete to admins and stamps the catalog `owner` from the authenticated
 * session before delegating to the coordinator's MountCatalogService. List/Get
 * are open to any authenticated user (the catalog is org-shared). The web uses
 * the generated Connect client; a file upload rides the `payload_tar` bytes
 * field (no multipart, no bespoke HTTP route).
 *
 * Injectable deps (getSession, mountCatalog) for tests.
 */

import type { ConnectRouter } from "@connectrpc/connect";

import { MountCatalogService } from "../gen/engram/app/v1/mount_catalog_pb.ts";
import { getSessionFromHeaders } from "../auth/session.ts";
import { requireAdmin, requireUser } from "./require.ts";
import { mountCatalog as defaultMountCatalog } from "../control-plane/client.ts";
import type { MountCatalogClient } from "../skills/catalog.ts";

export type GetSession = (
  headers: Headers,
) => Promise<{ user: { id: string; role?: string | null } } | null>;

export interface MountCatalogDeps {
  getSession?: GetSession;
  mountCatalog?: MountCatalogClient;
}


export function registerMountCatalog(router: ConnectRouter, deps?: MountCatalogDeps): void {
  const getSession: GetSession =
    deps?.getSession ??
    getSessionFromHeaders;
  const catalog: MountCatalogClient =
    deps?.mountCatalog ?? (defaultMountCatalog as unknown as MountCatalogClient);

  router.service(MountCatalogService, {
    // Org-shared catalog: any authenticated user may list/get the uploaded skills
    // (builtins are merged in by the web from the fleet's known set).
    async listSkills(_req, ctx) {
      await requireUser(ctx, getSession);
      const resp = await catalog.listSkills({});
      return { skills: resp.skills };
    },

    async getSkill(req, ctx) {
      await requireUser(ctx, getSession);
      const resp = await catalog.getSkill({ name: req.name });
      return { skill: resp.skill };
    },

    // Admin-only. The owner is stamped from the authenticated session — the
    // request's `owner` is ignored, so the web can never spoof attribution.
    async registerSkill(req, ctx) {
      const user = await requireAdmin(ctx, getSession);
      const resp = await catalog.registerSkill({
        name: req.name,
        description: req.description,
        owner: user.id,
        payloadTar: req.payloadTar,
        // ADR 0058 uploaded-binary arm: PATH binaries the bundle declares.
        bins: req.bins,
      });
      return { skill: resp.skill };
    },

    async deleteSkill(req, ctx) {
      await requireAdmin(ctx, getSession);
      const resp = await catalog.deleteSkill({ name: req.name });
      return { deleted: resp.deleted };
    },
  });
}
