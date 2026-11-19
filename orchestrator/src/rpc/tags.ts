/** Orchestrator-native TagService: installation-wide tags, managed by admins
 * (Settings → Tags). Every verb is admin-only. */

import { create } from "@bufbuild/protobuf";
import { Code, ConnectError } from "@connectrpc/connect";
import type { ConnectRouter } from "@connectrpc/connect";

import { getSessionFromHeaders } from "../auth/session.ts";
import { makeTagStore, type TagRow, type TagStore } from "../db/tags.ts";
import { TagSchema, TagService, type Tag as ProtoTag } from "../gen/engram/app/v1/tag_pb.ts";
import { normalizeTagColor, normalizeTagDescription, normalizeTagName } from "../tags/rules.ts";
import { requireAdmin, type GetSession } from "./require.ts";

export interface TagDeps {
  getSession?: GetSession;
  store?: TagStore;
}

export function toProtoTag(row: TagRow): ProtoTag {
  return create(TagSchema, { id: row.id, name: row.name, color: row.color, description: row.description });
}

/** The value of a field that passed its rule, or InvalidArgument with the rule. */
function valid(value: string | null, rule: string): string {
  if (value === null) throw new ConnectError(rule, Code.InvalidArgument);
  return value;
}

const tagName = (raw: string) =>
  valid(normalizeTagName(raw), "a tag name must be 1 to 32 characters, with no control characters");
const tagDescription = (raw: string) =>
  valid(normalizeTagDescription(raw), "a tag description must be at most 100 characters, on one line");
const tagColor = (raw: string) => valid(normalizeTagColor(raw), "a tag color must be a hex color like #1f6feb");

const nameTaken = () => new ConnectError("a tag with this name already exists", Code.AlreadyExists);
const tagNotFound = () => new ConnectError("tag not found", Code.NotFound);

export function registerTags(router: ConnectRouter, deps?: TagDeps): void {
  const getSession: GetSession = deps?.getSession ?? getSessionFromHeaders;
  let store = deps?.store;
  const tags = (): TagStore => (store ??= makeTagStore());

  router.service(TagService, {
    async listTags(_req, ctx) {
      await requireAdmin(ctx, getSession);
      const rows = await tags().list();
      return {
        tags: rows.map((row) => ({ tag: toProtoTag(row.tag), automationCount: row.automationCount })),
      };
    },

    async createTag(req, ctx) {
      await requireAdmin(ctx, getSession);
      const result = await tags().create({
        name: tagName(req.name),
        color: tagColor(req.color),
        description: tagDescription(req.description),
      });
      if ("refused" in result) throw nameTaken();
      return { tag: toProtoTag(result.tag) };
    },

    async updateTag(req, ctx) {
      await requireAdmin(ctx, getSession);
      const result = await tags().update(req.id, {
        ...(req.name !== undefined ? { name: tagName(req.name) } : {}),
        ...(req.color !== undefined ? { color: tagColor(req.color) } : {}),
        ...(req.description !== undefined ? { description: tagDescription(req.description) } : {}),
      });
      if ("refused" in result) throw result.refused === "name_taken" ? nameTaken() : tagNotFound();
      return { tag: toProtoTag(result.tag) };
    },

    async deleteTag(req, ctx) {
      await requireAdmin(ctx, getSession);
      if (!(await tags().delete(req.id))) throw tagNotFound();
      return {};
    },
  });
}
