import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";

import { API_BASE } from "@/lib/base";

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

export interface SpecReadResponse {
  spec: {
    id: string;
    title: string;
    lifecycle: "draft" | "published";
    sessionId: string | null;
    publishedCheckpointId: string | null;
    publishedAt: string | null;
  };
  checkpoints: SpecCheckpointSummary[];
  publishedCheckpoint: SpecCheckpoint | null;
}

async function specRequest<T>(path: string, init?: RequestInit): Promise<T> {
  const response = await fetch(`${API_BASE}${path}`, {
    credentials: "include",
    headers: { Accept: "application/json", ...init?.headers },
    ...init,
  });
  if (!response.ok) throw new Error(`${path} → ${response.status}`);
  return response.json() as Promise<T>;
}

export function useSpecRead(specId: string) {
  return useQuery({
    queryKey: ["spec", specId],
    queryFn: () => specRequest<SpecReadResponse>(`/specs/${encodeURIComponent(specId)}`),
    enabled: specId.length > 0,
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
