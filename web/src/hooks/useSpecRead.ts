import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import type {
  RestoreSectionStateUndo,
  SectionState,
  SectionStateTranscriptChip,
  SpecAlternativesStage,
} from "@engrams/spec-document";

import { specRequest } from "@/lib/spec-api";

const DRAFT_REFETCH_INTERVAL_MS = 5_000;

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
  provisional: boolean;
  frontier: boolean;
}

export interface SpecRail {
  layers: Array<{
    key: string;
    title: string;
    description: string | null;
    sections: SpecRailSection[];
  }>;
  completeness: { complete: number; total: number };
  frontierSectionId: string | null;
}

export interface SpecReadResponse {
  spec: {
    id: string;
    title: string;
    lifecycle: "draft" | "published";
    sessionId: string | null;
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
      query.state.data?.spec.lifecycle === "draft" ? DRAFT_REFETCH_INTERVAL_MS : false,
    refetchIntervalInBackground: false,
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
      queryClient.getQueryData<SpecReadResponse>(["spec", specId])?.spec.lifecycle === "published"
        ? false
        : DRAFT_REFETCH_INTERVAL_MS,
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
  state: Exclude<SectionState, "empty">,
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

/** The alternatives stage in the canvas (ADR 0114 D6, requirement R20). */
export function useSpecAlternatives(specId: string, enabled: boolean) {
  return useQuery({
    queryKey: ["spec", specId, "alternatives"],
    queryFn: async () =>
      (
        await specRequest<{ stage: SpecAlternativesStage | null }>(
          `/specs/${encodeURIComponent(specId)}/alternatives`,
        )
      ).stage,
    enabled: enabled && specId.length > 0,
    refetchInterval: DRAFT_REFETCH_INTERVAL_MS,
    refetchIntervalInBackground: false,
  });
}

export async function decideSpecAlternative(
  specId: string,
  input: { setId: string; optionKey: string | null; reason: string },
): Promise<{ stage: SpecAlternativesStage; applied: boolean }> {
  return specRequest(`/specs/${encodeURIComponent(specId)}/alternatives/decide`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(input),
  });
}

export function useDecideSpecAlternative(specId: string) {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: (input: { setId: string; optionKey: string | null; reason: string }) =>
      decideSpecAlternative(specId, input),
    onSuccess: (result) => {
      queryClient.setQueryData<SpecAlternativesStage>(
        ["spec", specId, "alternatives"],
        result.stage,
      );
    },
    onSettled: () => queryClient.invalidateQueries({ queryKey: ["spec", specId] }),
  });
}

export function useSetSpecSectionState(specId: string) {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: (input: {
      sectionId: string;
      state: Exclude<SectionState, "empty">;
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
