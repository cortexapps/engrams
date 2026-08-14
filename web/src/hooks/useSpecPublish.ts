import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";

import { specRequest, SpecRequestError } from "@/lib/spec-api";

export interface SpecPublishQuestion {
  id: string;
  sectionId: string;
  sectionTitle: string;
  text: string;
}

export interface SpecPublishRecord {
  state: "requested" | "pinned" | "artifact_published" | "complete" | "blocked";
  checkpointId: string;
  artifactId: string;
  artifactVersion: number | null;
  acknowledgedQuestionCount: number;
  requestedAt: string;
  pinnedAt: string | null;
  completedAt: string | null;
  lastError: string | null;
}

export interface SpecPublishStatus {
  phase: "ideation" | "drafting" | "published";
  canPublish: boolean;
  openQuestions: SpecPublishQuestion[];
  publish: SpecPublishRecord | null;
}

/** Why the server refused, with its confirmation state at refusal time. */
export type SpecPublishRefusalReason =
  | "not_owner"
  | "acknowledgment_required"
  | "already_published"
  | "ideation"
  | "no_session";

export interface SpecPublishRefusal {
  reason: SpecPublishRefusalReason;
  message: string;
  status: SpecPublishStatus | null;
}

export function specPublishKey(specId: string) {
  return ["spec", specId, "publish"] as const;
}

/** Poll while the durable publish scanner advances the recorded request. */
export function useSpecPublish(specId: string) {
  return useQuery({
    queryKey: specPublishKey(specId),
    queryFn: () => specRequest<SpecPublishStatus>(`/specs/${specId}/publish`),
    refetchInterval: (query) => {
      const publish = query.state.data?.publish;
      const advancing =
        publish !== null &&
        publish !== undefined &&
        publish.state !== "complete" &&
        publish.state !== "blocked";
      return advancing ? 2_000 : false;
    },
  });
}

export interface PublishSpecInput {
  acknowledgeOpenQuestions: boolean;
}

export function usePublishSpec(specId: string) {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: (input: PublishSpecInput) =>
      specRequest<SpecPublishStatus>(`/specs/${specId}/publish`, {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({
          actionId: crypto.randomUUID(),
          acknowledgeOpenQuestions: input.acknowledgeOpenQuestions,
        }),
      }),
    onSuccess: (status) => {
      queryClient.setQueryData<SpecPublishStatus>(specPublishKey(specId), status);
    },
    onSettled: () => queryClient.invalidateQueries({ queryKey: ["spec", specId] }),
  });
}

/** Read a refusal. The server can include the current questions with it. */
export function specPublishRefusal(error: unknown): SpecPublishRefusal | null {
  if (!(error instanceof SpecRequestError)) return null;
  const body = error.body;
  if (typeof body !== "object" || body === null || !("reason" in body)) return null;
  const reason = (body as { reason: unknown }).reason;
  if (!isRefusalReason(reason)) return null;
  const status = (body as { status?: unknown }).status;
  return {
    reason,
    message: error.message,
    status: isStatus(status) ? status : null,
  };
}

function isRefusalReason(value: unknown): value is SpecPublishRefusalReason {
  return (
    value === "not_owner" ||
    value === "acknowledgment_required" ||
    value === "already_published" ||
    value === "ideation" ||
    value === "no_session"
  );
}

function isStatus(value: unknown): value is SpecPublishStatus {
  return (
    typeof value === "object" &&
    value !== null &&
    "phase" in value &&
    "canPublish" in value &&
    "openQuestions" in value &&
    Array.isArray((value as { openQuestions?: unknown }).openQuestions)
  );
}
