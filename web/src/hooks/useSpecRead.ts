import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import type {
  RestoreSectionStateUndo,
  SectionState,
  SectionStateTranscriptChip,
} from "@engrams/spec-document";

import { specRequest } from "@/lib/spec-api";

const ACTIVE_SPEC_REFETCH_INTERVAL_MS = 5_000;

export { SpecRequestError } from "@/lib/spec-api";

export interface SpecCheckpointSummary {
  id: string;
  label: string;
  author: { id: string; name: string } | null;
  reason: string;
  docSeq: string;
  createdAt: string;
}

export interface SpecCheckpoint {
  id: string;
  label: string;
  authorUserId: string | null;
  reason: string;
  docSeq: string;
  createdAt: string;
  markdown: string;
  sections: Array<{ id: string; title: string }>;
}

export interface SpecRailSection {
  id: string;
  templateKey: string;
  title: string;
  state: SectionState;
  naReason: string | null;
  allowNa: boolean;
  openQuestionCount: number;
  settledBy: { id: string; name: string } | null;
  stateChangedAt: string | null;
}

export interface SpecRail {
  sections: SpecRailSection[];
  completeness: { complete: number; total: number };
}

export interface SpecReadResponse {
  spec: {
    id: string;
    title: string;
    phase: "ideation" | "drafting" | "published";
    sessionId: string | null;
    viewerIsOwner: boolean;
    publishedCheckpointId: string | null;
    publishedAt: string | null;
    revision: string;
    /** The template the spec locked when its session started (ADR 0114 D3). */
    template: { id: string; name: string };
  };
  checkpoints: SpecCheckpointSummary[];
  publishedCheckpoint: SpecCheckpoint | null;
}

export function useSpecRead(specId: string) {
  return useQuery({
    queryKey: ["spec", specId],
    queryFn: () => specRequest<SpecReadResponse>(`/specs/${encodeURIComponent(specId)}`),
    enabled: specId.length > 0,
    refetchInterval: (query) =>
      query.state.data?.spec.phase !== "published" ? ACTIVE_SPEC_REFETCH_INTERVAL_MS : false,
    refetchIntervalInBackground: false,
  });
}

export interface StartSpecDraftingResult {
  phase: "drafting";
  started: boolean;
  promptId?: string;
}

export async function startSpecDrafting(specId: string): Promise<StartSpecDraftingResult> {
  const response = await specRequest<{
    phase: "drafting";
    started: boolean;
    prompt_id?: string;
  }>(`/specs/${encodeURIComponent(specId)}/start-drafting`, { method: "POST" });
  return {
    phase: response.phase,
    started: response.started,
    ...(response.prompt_id ? { promptId: response.prompt_id } : {}),
  };
}

export function useStartSpecDrafting(specId: string) {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: () => startSpecDrafting(specId),
    onMutate: async () => {
      await queryClient.cancelQueries({ queryKey: ["spec", specId] });
      const previous = queryClient.getQueryData<SpecReadResponse>(["spec", specId]);
      if (previous?.spec.phase === "ideation") {
        queryClient.setQueryData<SpecReadResponse>(["spec", specId], {
          ...previous,
          spec: { ...previous.spec, phase: "drafting" },
        });
      }
      return { previous };
    },
    onError: (_error, _variables, context) => {
      if (context?.previous) {
        queryClient.setQueryData<SpecReadResponse>(["spec", specId], context.previous);
      }
    },
    onSuccess: (result) => {
      queryClient.setQueryData<SpecReadResponse>(["spec", specId], (current) =>
        current ? { ...current, spec: { ...current.spec, phase: result.phase } } : current,
      );
    },
    onSettled: () => queryClient.invalidateQueries({ queryKey: ["spec", specId] }),
  });
}

export function useSpecRail(specId: string) {
  const queryClient = useQueryClient();
  return useQuery({
    queryKey: ["spec", specId, "rail"],
    queryFn: async () =>
      (await specRequest<{ rail: SpecRail }>(`/specs/${encodeURIComponent(specId)}/rail`)).rail,
    enabled: specId.length > 0,
    refetchInterval: () =>
      queryClient.getQueryData<SpecReadResponse>(["spec", specId])?.spec.phase === "published"
        ? false
        : ACTIVE_SPEC_REFETCH_INTERVAL_MS,
    refetchIntervalInBackground: false,
  });
}

export function useSpecCheckpoint(
  specId: string,
  checkpointId: string | null,
  initialCheckpoint?: SpecCheckpoint | null,
) {
  return useQuery({
    queryKey: ["spec", specId, "checkpoint", checkpointId],
    queryFn: async () =>
      (
        await specRequest<{ checkpoint: SpecCheckpoint }>(
          `/specs/${encodeURIComponent(specId)}/checkpoints/${encodeURIComponent(checkpointId!)}`,
        )
      ).checkpoint,
    enabled: specId.length > 0 && checkpointId !== null,
    initialData: initialCheckpoint?.id === checkpointId ? initialCheckpoint : undefined,
  });
}

export async function restoreSpecSection(
  specId: string,
  checkpointId: string,
  sectionId: string,
): Promise<
  | { applied: true; checkpoint: SpecCheckpoint; newRev: string }
  | { applied: false; checkpoint: null; newRev: string }
> {
  return specRequest(`/specs/${encodeURIComponent(specId)}/restore`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ checkpointId, sectionId }),
  });
}

export function useRestoreSpecSection(specId: string) {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: (input: { checkpointId: string; sectionId: string }) =>
      restoreSpecSection(specId, input.checkpointId, input.sectionId),
    onSuccess: async () => {
      await queryClient.invalidateQueries({ queryKey: ["spec", specId] });
    },
  });
}

export interface SpecSectionStateResult {
  chip: SectionStateTranscriptChip;
  rail: SpecRail;
}

export async function setSpecSectionState(
  specId: string,
  sectionId: string,
  state: SectionState,
  reason: string | undefined,
  actionId: string,
): Promise<SpecSectionStateResult> {
  return specRequest(
    `/specs/${encodeURIComponent(specId)}/sections/${encodeURIComponent(sectionId)}/state`,
    {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ actionId, state, ...(reason === undefined ? {} : { reason }) }),
    },
  );
}

export async function undoSpecSectionState(
  specId: string,
  sectionId: string,
  undo: RestoreSectionStateUndo,
  actionId: string,
): Promise<SpecSectionStateResult> {
  return specRequest(
    `/specs/${encodeURIComponent(specId)}/sections/${encodeURIComponent(sectionId)}/undo`,
    {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ actionId, undo }),
    },
  );
}

export function useSetSpecSectionState(specId: string) {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: (input: {
      sectionId: string;
      state: SectionState;
      reason?: string;
      actionId: string;
    }) => setSpecSectionState(specId, input.sectionId, input.state, input.reason, input.actionId),
    onSuccess: (result) => {
      queryClient.setQueryData<SpecRail>(["spec", specId, "rail"], result.rail);
    },
    onSettled: () => queryClient.invalidateQueries({ queryKey: ["spec", specId] }),
  });
}

export function useUndoSpecSectionState(specId: string) {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: (input: { sectionId: string; undo: RestoreSectionStateUndo; actionId: string }) =>
      undoSpecSectionState(specId, input.sectionId, input.undo, input.actionId),
    onSuccess: (result) => {
      queryClient.setQueryData<SpecRail>(["spec", specId, "rail"], result.rail);
    },
    onSettled: () => queryClient.invalidateQueries({ queryKey: ["spec", specId] }),
  });
}
