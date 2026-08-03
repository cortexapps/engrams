/**
 * Artifact service layer — the single implementation behind BOTH the
 * ArtifactService RPC (web/CLI) and the in-session `Artifact` tool, so
 * every surface runs the exact same CASL checks.
 *
 * Authorization model (authz/ability.ts):
 *   - owner: manage (read / update / share / delete)
 *   - visibility "org": read for any authenticated user
 *   - admin: manage("all")
 * Anti-enumeration: an artifact the actor cannot read surfaces as
 * NotFound, never PermissionDenied.
 *
 * Bytes live coordinator-side. publish/update pull them out of the guest
 * with the existing `CreateArtifactFromPath` exec-`cat` stream (tool
 * payloads are JSON-only and must never carry file bodies), then record
 * the registry rows here with the session owner's identity.
 */

import { randomUUID } from "node:crypto";

import { Code, ConnectError } from "@connectrpc/connect";
import { subject } from "@casl/ability";

import { abilityFor, type AbilityUser } from "../authz/ability.ts";
import type {
  ArtifactListRow,
  ArtifactStore,
  ArtifactVisibility,
  ArtifactWithVersions,
} from "../db/artifacts.ts";

/** Media types that may be published as artifacts. Generic files are
 * `share-file`'s job; widen deliberately, not incidentally. */
export const ARTIFACT_MEDIA_TYPES: ReadonlySet<string> = new Set([
  "text/html",
  "text/markdown",
]);

/** Append-only history bound — far above real use, a runaway-loop stop. */
export const MAX_ARTIFACT_VERSIONS = 100;

const MAX_TITLE_CHARS = 200;

export type ListScope = "" | "mine" | "shared" | "all";

/** Subset of the coordinator SessionService the publish path needs. */
export interface ArtifactPullClient {
  createArtifactFromPath(req: {
    sessionId: string;
    path: string;
    suppressEvent?: boolean;
  }): Promise<{ artifactId: string; mediaType: string; sizeBytes: bigint | number }>;
}

export interface PublishInput {
  sessionId: string;
  taskId: string | null;
  /** The session owner — stamped by the caller from resolved identity,
   * never from request data. */
  ownerUserId: string | null;
  filePath: string;
  title?: string;
}

function abilityOf(actor: AbilityUser) {
  return abilityFor(actor);
}

function canRead(actor: AbilityUser, row: { ownerUserId: string | null; visibility: string }) {
  return abilityOf(actor).can("read", subject("Artifact", { ...row }));
}

function notFound(): ConnectError {
  return new ConnectError("artifact not found", Code.NotFound);
}

function baseName(filePath: string): string {
  const base = filePath.split(/[/\\]/).at(-1) ?? "";
  if (base === "") {
    throw new ConnectError("file_path has no file name", Code.InvalidArgument);
  }
  return base;
}

function sanitizeTitle(title: string | undefined, fallback: string): string {
  const cleaned = (title ?? "")
    .split("")
    .filter((ch) => ch >= " " || ch === "\t")
    .join("")
    .trim()
    .slice(0, MAX_TITLE_CHARS);
  return cleaned === "" ? fallback : cleaned;
}

/** Reject a stamped media type outside the artifact set. The pulled
 * bytes already sit in blob storage — that is the status quo for any
 * shared file and harmless (GC-exempt, unreferenced). */
function requireArtifactMediaType(mediaType: string): void {
  if (!ARTIFACT_MEDIA_TYPES.has(mediaType)) {
    throw new ConnectError(
      `only HTML and Markdown may be published as artifacts (got ${mediaType}); ` +
        "use share-file for other file types",
      Code.InvalidArgument,
    );
  }
}

export interface ArtifactServiceDeps {
  store: ArtifactStore;
  pull: ArtifactPullClient;
  /** Injectable id mint (tests pass a fixed sequence). */
  mintId?: () => string;
}

export function makeArtifactService(deps: ArtifactServiceDeps) {
  const mintId = deps.mintId ?? (() => randomUUID());

  return {
    /** ListTasks scope semantics: empty = mine for a member, all for an
     * admin; "shared" = org-visible rows from anyone; "all" admin-only. */
    async list(
      actor: AbilityUser,
      opts: { scope: string; page: number; pageSize: number },
    ): Promise<{ rows: ArtifactListRow[]; totalCount: number }> {
      const scope = opts.scope as ListScope;
      if (!["", "mine", "shared", "all"].includes(scope)) {
        throw new ConnectError("invalid scope", Code.InvalidArgument);
      }
      const isAdmin = actor.role === "admin";
      if (scope === "all" && !isAdmin) {
        throw new ConnectError("forbidden", Code.PermissionDenied);
      }
      const effective: ListScope =
        scope === "" ? (isAdmin ? "all" : "mine") : scope;

      const listOpts = {
        page: opts.page,
        pageSize: opts.pageSize,
        ...(effective === "mine" ? { ownerUserId: actor.id } : {}),
        ...(effective === "shared" ? { visibility: "org" as ArtifactVisibility } : {}),
      };
      const { rows, totalCount } = await deps.store.list(listOpts);
      // Belt-and-braces under the SQL scoping.
      return { rows: rows.filter((row) => canRead(actor, row)), totalCount };
    },

    async get(actor: AbilityUser, id: string): Promise<ArtifactWithVersions> {
      const row = await deps.store.get(id);
      if (!row || !canRead(actor, row)) throw notFound();
      return row;
    },

    async setVisibility(
      actor: AbilityUser,
      id: string,
      visibility: string,
    ): Promise<ArtifactWithVersions> {
      if (visibility !== "private" && visibility !== "org") {
        throw new ConnectError("invalid visibility", Code.InvalidArgument);
      }
      const row = await deps.store.get(id);
      // Anti-enumeration: unreadable → NotFound. A row the actor can
      // already read but not act on → PermissionDenied.
      if (!row || !canRead(actor, row)) throw notFound();
      if (!abilityOf(actor).can("share", subject("Artifact", { ...row }))) {
        throw new ConnectError("forbidden", Code.PermissionDenied);
      }
      await deps.store.setVisibility(id, visibility);
      const updated = await deps.store.get(id);
      if (!updated) throw notFound();
      return updated;
    },

    async delete(actor: AbilityUser, id: string): Promise<void> {
      const row = await deps.store.get(id);
      if (!row || !canRead(actor, row)) throw notFound();
      if (!abilityOf(actor).can("delete", subject("Artifact", { ...row }))) {
        throw new ConnectError("forbidden", Code.PermissionDenied);
      }
      await deps.store.delete(id);
    },

    /** Publish a new artifact from a guest file. The caller resolved the
     * session's owner; the actor here IS that owner (tool path) or an
     * operator publishing on the session's behalf. */
    async publish(input: PublishInput): Promise<ArtifactWithVersions> {
      const fileName = baseName(input.filePath);
      const pulled = await deps.pull.createArtifactFromPath({
        sessionId: input.sessionId,
        path: input.filePath,
        suppressEvent: true,
      });
      requireArtifactMediaType(pulled.mediaType);
      return deps.store.create(
        {
          id: mintId(),
          ownerUserId: input.ownerUserId,
          title: sanitizeTitle(input.title, fileName),
          fileName,
        },
        {
          sessionId: input.sessionId,
          taskId: input.taskId,
          coordArtifactId: pulled.artifactId,
          mediaType: pulled.mediaType,
          sizeBytes: Number(pulled.sizeBytes),
        },
      );
    },

    /** Publish a new version at an existing artifact id (same stable
     * URL). Owner-or-admin. */
    async update(
      actor: AbilityUser,
      input: { artifactId: string; sessionId: string; taskId: string | null; filePath: string; title?: string },
    ): Promise<ArtifactWithVersions> {
      const row = await deps.store.get(input.artifactId);
      if (!row || !canRead(actor, row)) throw notFound();
      if (!abilityOf(actor).can("update", subject("Artifact", { ...row }))) {
        throw new ConnectError("forbidden", Code.PermissionDenied);
      }
      if (row.currentVersion >= MAX_ARTIFACT_VERSIONS) {
        throw new ConnectError(
          `artifact has reached the version limit (${MAX_ARTIFACT_VERSIONS})`,
          Code.InvalidArgument,
        );
      }
      const fileName = baseName(input.filePath);
      const pulled = await deps.pull.createArtifactFromPath({
        sessionId: input.sessionId,
        path: input.filePath,
        suppressEvent: true,
      });
      requireArtifactMediaType(pulled.mediaType);
      const updated = await deps.store.appendVersion(
        input.artifactId,
        {
          sessionId: input.sessionId,
          taskId: input.taskId,
          coordArtifactId: pulled.artifactId,
          mediaType: pulled.mediaType,
          sizeBytes: Number(pulled.sizeBytes),
        },
        {
          fileName,
          ...(input.title !== undefined
            ? { title: sanitizeTitle(input.title, fileName) }
            : {}),
        },
      );
      if (!updated) throw notFound();
      return updated;
    },
  };
}

export type ArtifactService = ReturnType<typeof makeArtifactService>;
