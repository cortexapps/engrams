import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";

import { specRequest } from "@/lib/spec-api";

export type SpecTicketSyncState = "draft" | "queued" | "syncing" | "synced" | "failed";

/** One ticket, as the tree route returns it. */
export interface SpecTicket {
  id: string;
  parentId: string | null;
  ordinal: number;
  depth: number;
  title: string;
  body: string;
  description: string;
  backlink: { sectionId: string; sectionTitle: string; href: string };
  dependsOn: string[];
  syncState: SpecTicketSyncState;
  linearId: string | null;
  syncError: string | null;
  openQuestions: Array<{ id: string; sectionId: string; text: string }>;
}

export interface SpecTicketTree {
  specId: string;
  checkpointId: string;
  docSeq: string;
  publishedAt: string | null;
  sections: Array<{ id: string; title: string }>;
  tickets: SpecTicket[];
  unattachedQuestions: Array<{ id: string; sectionId: string; text: string }>;
}

/** The Linear identity of a synced ticket (R43). */
export interface SpecTicketLinearIssue {
  id: string;
  identifier: string;
  url: string;
}

export interface SpecTicketSyncLedger {
  specId: string;
  connector: { provider: string; connected: boolean; reason: string | null };
  target: {
    teamId: string | null;
    teamName: string | null;
    projectId: string | null;
    projectName: string | null;
    labelIds: string[];
    labelNames: string[];
  };
  overridden: boolean;
  total: number;
  synced: number;
  failed: number;
  inFlight: number;
  rows: Array<{
    ticketId: string;
    title: string;
    syncState: SpecTicketSyncState;
    issue: SpecTicketLinearIssue | null;
    error: string | null;
  }>;
}

export function specTicketsKey(specId: string) {
  return ["spec", specId, "tickets"] as const;
}

export function specTicketSyncKey(specId: string) {
  return ["spec", specId, "tickets", "sync"] as const;
}

export function useSpecTickets(specId: string) {
  return useQuery({
    queryKey: specTicketsKey(specId),
    queryFn: () => specRequest<SpecTicketTree>(`/specs/${specId}/tickets`),
  });
}

/**
 * The ledger, polled while a batch is in flight.
 *
 * The batch is durable and runs outside this request, so the only way the
 * browser learns that a row landed is to ask again. Polling stops as soon as
 * nothing is queued or syncing.
 */
export function useSpecTicketSyncLedger(specId: string) {
  return useQuery({
    queryKey: specTicketSyncKey(specId),
    queryFn: () => specRequest<SpecTicketSyncLedger>(`/specs/${specId}/tickets/sync`),
    refetchInterval: (query) => (query.state.data?.inFlight ? 1_500 : false),
  });
}

export interface SpecTicketSyncTargetInput {
  teamId?: string | null;
  teamName?: string | null;
  projectId?: string | null;
  projectName?: string | null;
  labelIds?: string[];
  labelNames?: string[];
}

/**
 * Start a batch, or retry one row.
 *
 * Both are the same request, and both are safe to press twice: the ledger on
 * the server keeps a repeat from creating a second issue (N4).
 */
export function useSyncSpecTickets(specId: string) {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: (input: { ticketIds?: string[]; target?: SpecTicketSyncTargetInput }) =>
      specRequest<SpecTicketSyncLedger>(`/specs/${specId}/tickets/sync`, {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({
          ...(input.ticketIds === undefined ? {} : { ticketIds: input.ticketIds }),
          ...(input.target === undefined ? {} : { target: input.target }),
        }),
      }),
    onSuccess: (ledger) => {
      queryClient.setQueryData<SpecTicketSyncLedger>(specTicketSyncKey(specId), ledger);
    },
    onSettled: () => queryClient.invalidateQueries({ queryKey: specTicketsKey(specId) }),
  });
}
