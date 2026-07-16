/** Orchestrator-native PapercutService. */

import { ConnectError, Code } from "@connectrpc/connect";
import type { ConnectRouter, HandlerContext } from "@connectrpc/connect";
import { timestampFromDate } from "@bufbuild/protobuf/wkt";

import { PapercutService } from "../gen/engram/app/v1/papercut_pb.ts";
import type { Papercut } from "../gen/engram/app/v1/papercut_pb.ts";
import { getSessionFromHeaders } from "../auth/session.ts";
import { getDb } from "../db/client.ts";
import {
  makePapercutStore,
  type PapercutListRow,
  type PapercutRow,
  type PapercutStore,
} from "../db/papercuts.ts";
import { makeProfileStore, type ProfileStore } from "../db/profiles.ts";

export type GetSession = (
  headers: Headers,
) => Promise<{
  user: { id: string };
} | null>;

export interface PapercutDeps {
  getSession?: GetSession;
  papercuts?: PapercutStore;
  profiles?: ProfileStore;
  db?: ReturnType<typeof getDb>;
}

interface ProfileSnapshotFields {
  name: string;
  icon: string;
}

function headersOf(ctx: HandlerContext): Headers {
  return ctx.requestHeader;
}

async function requireUser(
  ctx: HandlerContext,
  getSession: GetSession,
): Promise<void> {
  const session = await getSession(headersOf(ctx));
  if (!session) throw new ConnectError("unauthenticated", Code.Unauthenticated);
}

function toProto(
  row: PapercutRow,
  profileFields?: ProfileSnapshotFields,
): Papercut {
  return {
    id: row.id,
    summary: row.summary,
    description: row.description,
    category: row.category,
    severity: row.severity ?? "",
    tags: row.tags,
    sessionId: row.sessionId,
    taskId: row.taskId ?? "",
    archived: row.archivedAt != null,
    createdAt: timestampFromDate(row.createdAt),
    ...(row.profileId != null
      ? {
          profile: {
            id: row.profileId,
            name: profileFields?.name ?? "",
            icon: profileFields?.icon ?? "",
            archived: false,
            imageUri: "",
            skills: [],
          },
        }
      : {}),
  } as Papercut;
}

function listRowToProto(row: PapercutListRow): Papercut {
  return toProto(row, {
    name: row.profileName ?? "",
    icon: row.profileIcon ?? "",
  });
}

export function registerPapercuts(router: ConnectRouter, deps?: PapercutDeps): void {
  const getSession: GetSession = deps?.getSession ?? getSessionFromHeaders;
  const getDbFn = (): ReturnType<typeof getDb> => deps?.db ?? getDb();

  let papercutStore = deps?.papercuts;
  const papercuts = (): PapercutStore =>
    (papercutStore ??= makePapercutStore(getDbFn()));
  let profileStore = deps?.profiles;
  const profiles = (): ProfileStore =>
    (profileStore ??= makeProfileStore(getDbFn()));

  async function mutationResponse(id: string, archived: boolean): Promise<Papercut> {
    const row = await papercuts().get(id);
    if (!row) throw new ConnectError("not found", Code.NotFound);
    await papercuts().setArchived(id, archived);
    const profile = row.profileId != null ? await profiles().get(row.profileId) : null;
    return toProto(
      { ...row, archivedAt: archived ? new Date() : null },
      profile ? { name: profile.name, icon: profile.icon } : undefined,
    );
  }

  router.service(PapercutService, {
    async listPapercuts(req, ctx) {
      await requireUser(ctx, getSession);
      const rows = await papercuts().list({
        includeArchived: req.includeArchived,
        limit: 500,
      });
      return { papercuts: rows.map(listRowToProto) };
    },

    async archivePapercut(req, ctx) {
      await requireUser(ctx, getSession);
      return { papercut: await mutationResponse(req.id, true) };
    },

    async unarchivePapercut(req, ctx) {
      await requireUser(ctx, getSession);
      return { papercut: await mutationResponse(req.id, false) };
    },
  });
}
