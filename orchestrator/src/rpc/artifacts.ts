/**
 * Native ArtifactService — the cross-session artifact registry surface
 * (web + CLI). Orchestrator-native (like TaskService), registered on the
 * ConnectRouter before the passthrough; never proxied.
 *
 * All authorization lives in the shared service layer
 * (artifacts/service.ts) — the same instance the in-session Artifact
 * tool calls, so the two surfaces cannot drift. This module only maps
 * identity (requireUser), rows → proto records, and mints the
 * short-lived raw_url capability for the current version.
 *
 * Injectable deps (getSession, store, identities, now) for tests.
 */

import type { ConnectRouter, HandlerContext } from "@connectrpc/connect";

import { ArtifactService } from "../gen/engram/app/v1/artifact_pb.ts";
import type {
  ArtifactRecord,
  ArtifactVersionRecord,
} from "../gen/engram/app/v1/artifact_pb.ts";
import { getSessionFromHeaders } from "../auth/session.ts";
import { requireUser, type RpcUser } from "./require.ts";
import { getDb } from "../db/client.ts";
import { makeArtifactStore } from "../db/artifacts.ts";
import type {
  ArtifactListRow,
  ArtifactStore,
  ArtifactWithVersions,
} from "../db/artifacts.ts";
import { makeUserIdentityStore, type UserIdentityStore } from "../db/users.ts";
import {
  makeArtifactService,
  type ArtifactService as ArtifactServiceLayer,
} from "../artifacts/service.ts";
import { mintRawToken, rawArtifactPath } from "../crypto/raw-token.ts";

export type GetSession = (
  headers: Headers,
) => Promise<{
  user: { id: string; role?: string | null; email?: string | null };
} | null>;

export interface ArtifactRpcDeps {
  getSession?: GetSession;
  store?: ArtifactStore;
  identities?: UserIdentityStore;
  /** The shared service layer; defaults to one over `store`. The publish
   * client is not needed here (publish/update are tool-only surfaces). */
  service?: ArtifactServiceLayer;
  now?: () => Date;
}

/** List page-size ceiling; 0 = unpaginated (small registries). */
const MAX_PAGE_SIZE = 200;

function versionToProto(
  v: ArtifactWithVersions["versions"][number],
): ArtifactVersionRecord {
  return {
    version: v.version,
    sessionId: v.sessionId,
    ...(v.taskId != null ? { taskId: v.taskId } : {}),
    mediaType: v.mediaType,
    sizeBytes: BigInt(v.sizeBytes),
    createdAt: v.createdAt.toISOString(),
  } as ArtifactVersionRecord;
}

function toRecord(
  row: ArtifactListRow | ArtifactWithVersions,
  identities: Map<string, { name: string; email: string }>,
  now: Date,
): ArtifactRecord {
  const identity = row.ownerUserId != null ? identities.get(row.ownerUserId) : undefined;
  return {
    id: row.id,
    title: row.title,
    fileName: row.fileName,
    ...(row.ownerUserId != null ? { ownerUserId: row.ownerUserId } : {}),
    ...(row.ownerUserId != null && identity
      ? { createdBy: { id: row.ownerUserId, name: identity.name, email: identity.email } }
      : {}),
    visibility: row.visibility,
    currentVersion: row.currentVersion,
    mediaType: row.mediaType,
    sizeBytes: BigInt(row.sizeBytes),
    createdAt: row.createdAt.toISOString(),
    updatedAt: row.updatedAt.toISOString(),
    rawUrl: rawArtifactPath(row.id, mintRawToken(row.id, now)),
    versions: "versions" in row ? row.versions.map(versionToProto) : [],
  } as ArtifactRecord;
}

export function registerArtifacts(router: ConnectRouter, deps?: ArtifactRpcDeps): void {
  const getSession: GetSession = deps?.getSession ?? getSessionFromHeaders;
  let store = deps?.store;
  const storeOf = (): ArtifactStore => (store ??= makeArtifactStore());
  let identityStore = deps?.identities;
  const identitiesOf = (): UserIdentityStore =>
    (identityStore ??= makeUserIdentityStore(getDb()));
  let serviceLayer = deps?.service;
  const serviceOf = (): ArtifactServiceLayer =>
    (serviceLayer ??= makeArtifactService({
      store: storeOf(),
      pull: {
        // The RPC surface never publishes; the Artifact tool does.
        createArtifactFromPath() {
          return Promise.reject(new Error("publish is tool-only"));
        },
      },
    }));
  const now = deps?.now ?? (() => new Date());

  async function actorOf(ctx: HandlerContext): Promise<RpcUser> {
    return requireUser(ctx, getSession);
  }

  async function identityMapFor(
    rows: Array<{ ownerUserId: string | null }>,
  ): Promise<Map<string, { name: string; email: string }>> {
    const ids = [
      ...new Set(
        rows.map((r) => r.ownerUserId).filter((id): id is string => id != null),
      ),
    ];
    return identitiesOf().getIdentities(ids);
  }

  router.service(ArtifactService, {
    async listArtifacts(req, ctx) {
      const actor = await actorOf(ctx);
      const pageSize = Math.min(Math.max(req.pageSize, 0), MAX_PAGE_SIZE);
      const { rows, totalCount } = await serviceOf().list(actor, {
        scope: req.scope,
        page: Math.max(req.page, 1),
        pageSize,
      });
      const identities = await identityMapFor(rows);
      const at = now();
      return {
        artifacts: rows.map((row) => toRecord(row, identities, at)),
        totalCount,
      };
    },

    async getArtifactRecord(req, ctx) {
      const actor = await actorOf(ctx);
      const row = await serviceOf().get(actor, req.id);
      const identities = await identityMapFor([row]);
      return { artifact: toRecord(row, identities, now()) };
    },

    async setArtifactVisibility(req, ctx) {
      const actor = await actorOf(ctx);
      const row = await serviceOf().setVisibility(actor, req.id, req.visibility);
      const identities = await identityMapFor([row]);
      return { artifact: toRecord(row, identities, now()) };
    },

    async deleteArtifact(req, ctx) {
      const actor = await actorOf(ctx);
      await serviceOf().delete(actor, req.id);
      return { deleted: true };
    },
  });
}
