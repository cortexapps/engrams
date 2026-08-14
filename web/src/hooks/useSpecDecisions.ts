import { useQuery } from "@tanstack/react-query";

import { specRequest } from "@/lib/spec-api";

export interface SpecDecisionActor {
  id: string | null;
  /** The person, or the server's label when nobody can be named. */
  name: string;
}

interface SpecDecisionBase {
  id: string;
  sectionId: string;
  sectionTitle: string;
  actor: SpecDecisionActor;
  decidedAt: string;
}

export type SpecDecision =
  | (SpecDecisionBase & { kind: "section_settled" })
  | (SpecDecisionBase & {
      kind: "question_resolved";
      question: string;
      resolutionLink: string;
    });

export function useSpecDecisions(specId: string, enabled = true) {
  return useQuery({
    queryKey: ["spec", specId, "decisions"],
    queryFn: async () =>
      (
        await specRequest<{ decisions: SpecDecision[] }>(
          `/specs/${encodeURIComponent(specId)}/decisions`,
        )
      ).decisions,
    enabled: enabled && specId.length > 0,
  });
}
