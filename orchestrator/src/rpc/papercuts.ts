/** Orchestrator-native PapercutService. */

import { ConnectError, Code } from "@connectrpc/connect";
import type { ConnectRouter, HandlerContext } from "@connectrpc/connect";
import { timestampFromDate } from "@bufbuild/protobuf/wkt";

import { PapercutService } from "../gen/engram/app/v1/papercut_pb.ts";
import type { Papercut } from "../gen/engram/app/v1/papercut_pb.ts";
import { abilityFor } from "../authz/ability.ts";
import { getSessionFromHeaders } from "../auth/session.ts";
import { isServiceAccountEmail } from "./api-key.ts";
import { getDb } from "../db/client.ts";
import {
  makePapercutStore,
  type PapercutListRow,
  type PapercutRow,
  type PapercutStore,
} from "../db/papercuts.ts";
import { makeProfileStore, type ProfileRow, type ProfileStore } from "../db/profiles.ts";
import { makeUserSecretStore, type UserSecretStore } from "../db/user-secrets.ts";
import type { PortExposureStore } from "../db/port-exposures.ts";
import { makeUserIdentityStore, type UserIdentityStore } from "../db/users.ts";
import { makeConnectorStore } from "../db/connectors.ts";
import type { CustomConnectorSource } from "../connectors/registry.ts";
import type { ImagesClient } from "./profiles.ts";
import {
  createTaskWithSession,
  type CreatedTask,
  type CreateTaskParams,
  type Db,
  type HarnessCatalogClient,
  type TaskSessionsClient,
} from "./task-create.ts";
import {
  sessions as defaultSessions,
  images as defaultImages,
  harnessCatalog as defaultHarnessCatalog,
} from "../control-plane/client.ts";

export type CreateTask = (params: CreateTaskParams) => Promise<CreatedTask>;

export type GetSession = (
  headers: Headers,
) => Promise<{
  user: { id: string; role?: string | null; email?: string | null };
} | null>;

export interface PapercutDeps {
  getSession?: GetSession;
  papercuts?: PapercutStore;
  profiles?: ProfileStore;
  sessions?: TaskSessionsClient;
  harnessCatalog?: HarnessCatalogClient;
  secrets?: UserSecretStore;
  images?: ImagesClient;
  connectors?: CustomConnectorSource;
  portExposures?: PortExposureStore;
  users?: UserIdentityStore;
  db?: Db;
  /** Injectable service seam; production always uses createTaskWithSession. */
  createTask?: CreateTask;
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
): Promise<{ id: string; role: string; serviceAccount: boolean }> {
  const session = await getSession(headersOf(ctx));
  if (!session) throw new ConnectError("unauthenticated", Code.Unauthenticated);
  return {
    id: session.user.id,
    role: session.user.role ?? "user",
    serviceAccount: isServiceAccountEmail(session.user.email ?? ""),
  };
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
    fixTaskId: row.fixTaskId ?? "",
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

function fixTaskPrompt(row: PapercutRow): string {
  const details = [
    `Summary: ${row.summary}`,
    `Category: ${row.category}`,
  ];
  const severity = row.severity?.trim();
  if (severity) details.push(`Severity: ${severity}`);
  const tags = row.tags.map((tag) => tag.trim()).filter(Boolean);
  if (tags.length > 0) details.push(`Tags: ${tags.join(", ")}`);
  details.push(`Details:\n${row.description}`);

  return [
    "You previously logged this papercut while working in this environment:",
    details.join("\n"),
    `(logged ${row.createdAt.toISOString()} from session ${row.sessionId})`,
    "See the context above and attempt to fix the underlying friction. If you can address it from this session, make the change and open a PR. If it cannot be fixed from within a session, investigate and report exactly what change is needed and where.",
  ].join("\n\n");
}

export function registerPapercuts(router: ConnectRouter, deps?: PapercutDeps): void {
  const getSession: GetSession = deps?.getSession ?? getSessionFromHeaders;
  const getDbFn = (): Db => deps?.db ?? getDb();

  let papercutStore = deps?.papercuts;
  const papercuts = (): PapercutStore =>
    (papercutStore ??= makePapercutStore(getDbFn()));
  let profileStore = deps?.profiles;
  const profiles = (): ProfileStore =>
    (profileStore ??= makeProfileStore(getDbFn()));

  const sessionsClient: TaskSessionsClient = deps?.sessions ?? {
    async createSession(req) {
      const created = await defaultSessions.createSession(req);
      return { sessionId: created.sessionId };
    },
    deleteSession: (req) => defaultSessions.deleteSession(req),
  };
  const imagesClient: ImagesClient = deps?.images ?? {
    async listEnabledImages(req) {
      const response = await defaultImages.listEnabledImages(req);
      return {
        images: response.images.map(({ id, imageUri }) => ({ id, imageUri })),
      };
    },
  };
  const harnessCatalogClient: HarnessCatalogClient = deps?.harnessCatalog ?? {
    listHarnesses: (req) => defaultHarnessCatalog.listHarnesses(req),
  };
  const connectors: CustomConnectorSource = deps?.connectors ?? {
    list: () => makeConnectorStore(getDbFn()).list(),
  };
  const createTask: CreateTask = deps?.createTask ??
    ((params) =>
      createTaskWithSession(
        {
          profiles: profiles(),
          images: imagesClient,
          connectors,
          harnessCatalog: harnessCatalogClient,
          sessions: sessionsClient,
          secrets: deps?.secrets ?? makeUserSecretStore(getDbFn()),
          ...(deps?.portExposures
            ? { portExposures: deps.portExposures }
            : {}),
          users: deps?.users ?? makeUserIdentityStore(getDbFn()),
          db: getDbFn(),
        },
        params,
      ));

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

    async startFixTask(req, ctx) {
      const user = await requireUser(ctx, getSession);
      if (!abilityFor(user).can("create", "Task")) {
        throw new ConnectError("forbidden", Code.PermissionDenied);
      }

      const papercut = await papercuts().get(req.id);
      if (!papercut) throw new ConnectError("not found", Code.NotFound);
      if (papercut.archivedAt != null) {
        throw new ConnectError("papercut is archived", Code.FailedPrecondition);
      }
      if (papercut.profileId == null) {
        throw new ConnectError("papercut has no profile", Code.FailedPrecondition);
      }

      const profile: ProfileRow | null = await profiles().getActive(papercut.profileId);
      if (!profile) {
        throw new ConnectError(
          "the papercut's profile is archived or gone — cannot start a fix task",
          Code.FailedPrecondition,
        );
      }

      const { taskId, sessionId } = await createTask({
        type: "chat",
        ownerUserId: user.id,
        ownerIsServiceAccount: user.serviceAccount,
        profileId: profile.id,
        title: `Fix papercut: ${papercut.summary}`,
        prompt: fixTaskPrompt(papercut),
      });

      await papercuts().setFixTask(papercut.id, taskId);
      return { taskId, sessionId };
    },
  });
}
