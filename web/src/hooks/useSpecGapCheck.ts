import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import type {
  GapFindingKind,
  GapFindingSeverity,
  GapProposedDiff,
  TraceabilityMatrix,
} from "@engrams/spec-document";

import { specRequest } from "@/lib/spec-api";

export type GapFindingDisposition = "pending" | "question_opened" | "diff_accepted" | "dismissed";

export type GapDispositionAction = "open_question" | "accept_diff" | "dismiss";

export interface SpecGapFinding {
  id: string;
  kind: GapFindingKind;
  severity: GapFindingSeverity;
  layerKey: string;
  sectionId: string;
  sectionTitle: string;
  requirementId: string | null;
  summary: string;
  detail: string;
  proposedDiff: GapProposedDiff | null;
  disposition: GapFindingDisposition;
  openQuestionId: string | null;
  disposedAt: string | null;
}

export interface SpecGapCheckRun {
  id: string;
  specId: string;
  /** The document revision the pass covered. */
  rev: string;
  /** Set when a fatal finding halted the pass outside-in (R32). */
  stoppedAtLayerKey: string | null;
  suppressedCount: number;
  matrix: TraceabilityMatrix;
  findings: SpecGapFinding[];
  createdAt: string;
}

export interface SpecGapCheckStatus {
  stale: boolean;
  rev: string;
  run: SpecGapCheckRun | null;
}

export function specGapCheckKey(specId: string) {
  return ["spec", specId, "gap-check"] as const;
}

export function useSpecGapCheck(specId: string) {
  return useQuery({
    queryKey: specGapCheckKey(specId),
    queryFn: () => specRequest<SpecGapCheckStatus>(`/specs/${specId}/gap-check`),
  });
}

export function useRunSpecGapCheck(specId: string) {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: (input: { actionId: string }) =>
      specRequest<{ run: SpecGapCheckRun }>(`/specs/${specId}/gap-check`, {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ actionId: input.actionId }),
      }),
    onSuccess: (result) => {
      queryClient.setQueryData<SpecGapCheckStatus>(specGapCheckKey(specId), {
        stale: false,
        rev: result.run.rev,
        run: result.run,
      });
    },
    onSettled: () => queryClient.invalidateQueries({ queryKey: ["spec", specId] }),
  });
}

/**
 * Land one finding. This is the only path from a gap-check finding to the
 * document, and it always needs a person (R28).
 */
export function useDisposeSpecGapFinding(specId: string) {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: (input: { runId: string; findingId: string; action: GapDispositionAction }) =>
      specRequest<{ run: SpecGapCheckRun }>(
        `/specs/${specId}/gap-check/${input.runId}/findings/${encodeURIComponent(input.findingId)}`,
        {
          method: "POST",
          headers: { "content-type": "application/json" },
          body: JSON.stringify({ action: input.action }),
        },
      ),
    onSuccess: (result) => {
      queryClient.setQueryData<SpecGapCheckStatus>(specGapCheckKey(specId), (previous) => ({
        stale: previous?.stale ?? false,
        rev: previous?.rev ?? result.run.rev,
        run: result.run,
      }));
    },
    onSettled: () => queryClient.invalidateQueries({ queryKey: ["spec", specId] }),
  });
}
