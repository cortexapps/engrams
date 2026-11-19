/** An in-memory TagStore for RPC tests, with the outcomes of the Postgres
 * store (tag-store.test.ts pins those against a live database). */

import type { AddToAutomationOutcome, TagRow, TagStore, TagSummaryRow } from "../../db/tags.ts";

const AT = new Date("2026-11-19T12:00:00Z");

function byName(a: TagRow, b: TagRow): number {
  return a.name.toLowerCase().localeCompare(b.name.toLowerCase());
}

export function makeMemoryTagStore(opts?: {
  /** Whether an automation exists and is not archived. Default: always. */
  isActiveAutomation?: (id: string) => boolean;
}): TagStore & { tags: Map<string, TagRow>; links: Map<string, Set<string>> } {
  const isActive = opts?.isActiveAutomation ?? (() => true);
  const tags = new Map<string, TagRow>();
  /** automation id → tag ids */
  const links = new Map<string, Set<string>>();
  let sequence = 0;

  const nameTaken = (name: string, exceptId?: string) =>
    [...tags.values()].some((t) => t.id !== exceptId && t.name.toLowerCase() === name.toLowerCase());

  return {
    tags,
    links,

    async list(): Promise<TagSummaryRow[]> {
      return [...tags.values()].sort(byName).map((tag) => ({
        tag,
        automationCount: [...links.entries()].filter(([automationId, ids]) => ids.has(tag.id) && isActive(automationId))
          .length,
      }));
    },

    async create(input) {
      if (nameTaken(input.name)) return { refused: "name_taken" };
      const tag: TagRow = { id: `tag-${++sequence}`, ...input, createdAt: AT, updatedAt: AT };
      tags.set(tag.id, tag);
      return { tag };
    },

    async update(id, patch) {
      const current = tags.get(id);
      if (!current) return { refused: "not_found" };
      if (patch.name !== undefined && nameTaken(patch.name, id)) return { refused: "name_taken" };
      const tag: TagRow = {
        ...current,
        ...(patch.name !== undefined ? { name: patch.name } : {}),
        ...(patch.color !== undefined ? { color: patch.color } : {}),
        ...(patch.description !== undefined ? { description: patch.description } : {}),
      };
      tags.set(id, tag);
      return { tag };
    },

    async delete(id) {
      for (const ids of links.values()) ids.delete(id);
      return tags.delete(id);
    },

    async tagsForAutomations(automationIds) {
      const result = new Map<string, TagRow[]>();
      for (const automationId of automationIds) {
        const linked = [...(links.get(automationId) ?? [])].map((id) => tags.get(id)).filter((t) => t !== undefined);
        if (linked.length > 0) result.set(automationId, linked.sort(byName));
      }
      return result;
    },

    async addToAutomation(automationId, tagId, maxTags): Promise<AddToAutomationOutcome> {
      if (!isActive(automationId)) return "automation_missing";
      const ids = links.get(automationId) ?? new Set<string>();
      if (ids.has(tagId)) return "added";
      if (ids.size >= maxTags) return "full";
      if (!tags.has(tagId)) return "tag_missing";
      ids.add(tagId);
      links.set(automationId, ids);
      return "added";
    },

    async removeFromAutomation(automationId, tagId) {
      if (!isActive(automationId)) return "automation_missing";
      links.get(automationId)?.delete(tagId);
      return "removed";
    },
  };
}
