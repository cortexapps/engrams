/**
 * The post-publish ticket tree (ADR 0114 D6, R39-R40).
 *
 * Every mutation answers with the whole tree, so each hook writes the reply
 * straight into the cache instead of invalidating and refetching. A drag that
 * needed a second round trip to learn the new ordinals would feel like a form,
 * and this is the one v1 surface where hand editing leads.
 */

import { useMutation, useQuery, useQueryClient, type QueryClient } from "@tanstack/react-query";

import type { SpecTicketSyncState } from "@engrams/spec-document";

import { specRequest } from "@/lib/spec-api";

export interface SpecTicketBacklink {
  sectionId: string;
  sectionTitle: string;
  href: string;
}

export interface SpecTicketQuestion {
  id: string;
  sectionId: string;
  text: string;
}

export interface SpecTicket {
  id: string;
  parentId: string | null;
  ordinal: number;
  depth: number;
  title: string;
  /** The description a person edits, without its opening backlink line. */
  body: string;
  description: string;
  backlink: SpecTicketBacklink;
  dependsOn: string[];
  syncState: SpecTicketSyncState;
  linearId: string | null;
  syncError: string | null;
  openQuestions: SpecTicketQuestion[];
}

export interface SpecTicketTree {
  specId: string;
  checkpointId: string;
  /** The pinned revision, for the "spec vN pinned" chip. */
  docSeq: string;
  publishedAt: string | null;
  sections: Array<{ id: string; title: string }>;
  tickets: SpecTicket[];
  unattachedQuestions: SpecTicketQuestion[];
}

export function specTicketsKey(specId: string): readonly unknown[] {
  return ["spec", specId, "tickets"];
}

const JSON_HEADERS = { "content-type": "application/json" };

export function useSpecTickets(specId: string, enabled = true) {
  return useQuery({
    queryKey: specTicketsKey(specId),
    enabled: enabled && specId.length > 0,
    queryFn: () => specRequest<SpecTicketTree>(`/api/v1/specs/${specId}/tickets`),
  });
}

/** Every ticket command, as the tree editor issues them. */
export type SpecTicketCommand =
  | {
      kind: "add";
      parentId: string | null;
      index?: number;
      title: string;
      body: string;
      sectionId: string;
    }
  | { kind: "update"; id: string; title?: string; body?: string; sectionId?: string }
  | { kind: "delete"; id: string }
  | { kind: "move"; id: string; parentId: string | null; index?: number }
  | { kind: "split"; id: string; parts: Array<{ title: string; body: string; sectionId?: string }> }
  | { kind: "merge"; targetId: string; sourceIds: string[] };

export function useSpecTicketCommand(specId: string) {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: (command: SpecTicketCommand) => runCommand(specId, command),
    onSuccess: (tree) => writeTree(queryClient, specId, tree),
  });
}

export function writeTree(queryClient: QueryClient, specId: string, tree: SpecTicketTree): void {
  queryClient.setQueryData(specTicketsKey(specId), tree);
}

function runCommand(specId: string, command: SpecTicketCommand): Promise<SpecTicketTree> {
  const base = `/api/v1/specs/${specId}/tickets`;
  switch (command.kind) {
    case "add":
      return specRequest<SpecTicketTree>(base, {
        method: "POST",
        headers: JSON_HEADERS,
        body: JSON.stringify({
          parentId: command.parentId,
          index: command.index,
          title: command.title,
          body: command.body,
          sectionId: command.sectionId,
        }),
      });
    case "update":
      return specRequest<SpecTicketTree>(`${base}/${command.id}`, {
        method: "PATCH",
        headers: JSON_HEADERS,
        body: JSON.stringify({
          title: command.title,
          body: command.body,
          sectionId: command.sectionId,
        }),
      });
    case "delete":
      return specRequest<SpecTicketTree>(`${base}/${command.id}`, { method: "DELETE" });
    case "move":
      return specRequest<SpecTicketTree>(`${base}/${command.id}/move`, {
        method: "POST",
        headers: JSON_HEADERS,
        body: JSON.stringify({ parentId: command.parentId, index: command.index }),
      });
    case "split":
      return specRequest<SpecTicketTree>(`${base}/${command.id}/split`, {
        method: "POST",
        headers: JSON_HEADERS,
        body: JSON.stringify({ parts: command.parts }),
      });
    case "merge":
      return specRequest<SpecTicketTree>(`${base}/${command.targetId}/merge`, {
        method: "POST",
        headers: JSON_HEADERS,
        body: JSON.stringify({ sourceIds: command.sourceIds }),
      });
  }
}
