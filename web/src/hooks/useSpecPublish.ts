import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";

import { specRequest, SpecRequestError } from "@/lib/spec-api";

export type SpecPublishBlockerReason = "empty" | "drafted" | "na_without_reason";

export interface SpecPublishBlocker {
  sectionId: string;
  sectionTitle: string;
  layerKey: string;
  state: string;
  reason: SpecPublishBlockerReason;
}

export interface SpecPublishQuestion {
  id: string;
  sectionId: string;
  sectionTitle: string;
  text: string;
}

export interface SpecPublishGate {
  ready: boolean;
  settledRequiredCount: number;
  requiredCount: number;
  acknowledgmentRequired: boolean;
  gapCheckRunRequired: boolean;
  blockers: SpecPublishBlocker[];
  openQuestions: SpecPublishQuestion[];
}

export interface SpecPublishRecord {
  state: "requested" | "pinned" | "artifact_published" | "complete";
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
  lifecycle: "draft" | "published";
  canPublish: boolean;
  publishedAt: string | null;
  gate: SpecPublishGate;
  gapCheck: { stale: boolean; runId: string | null; ranAt: string | null; gates: boolean };
  publish: SpecPublishRecord | null;
}

/** Why the server refused, with the gate as it stood at refusal time. */
export type SpecPublishRefusalReason =
  | "not_owner"
  | "blocked"
  | "acknowledgment_required"
  | "gap_check_stale"
  | "gap_check_failed"
  | "already_published"
  | "no_session";

export interface SpecPublishRefusal {
  reason: SpecPublishRefusalReason;
  message: string;
  status: SpecPublishStatus | null;
}

export function specPublishKey(specId: string) {
  return ["spec", specId, "publish"] as const;
}

/**
 * The gate. While a publish is in flight the scanner is still advancing it, so
 * the status is polled until it completes; the steps a person watches are the
 * server's own, not an optimistic guess.
 */
export function useSpecPublish(specId: string) {
  return useQuery({
    queryKey: specPublishKey(specId),
    queryFn: () => specRequest<SpecPublishStatus>(`/specs/${specId}/publish`),
    refetchInterval: (query) => {
      const publish = query.state.data?.publish;
      return publish && publish.state !== "complete" ? 2_000 : false;
    },
  });
}

export interface PublishSpecInput {
  acknowledgeOpenQuestions: boolean;
  runGapCheck: boolean;
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
          runGapCheck: input.runGapCheck,
        }),
      }),
    onSuccess: (status) => {
      queryClient.setQueryData<SpecPublishStatus>(specPublishKey(specId), status);
    },
    onSettled: () => queryClient.invalidateQueries({ queryKey: ["spec", specId] }),
  });
}

/**
 * Read a refusal. The server sends the gate with it, so the dialog shows the
 * new blockers or the questions without a second request.
 */
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
    value === "blocked" ||
    value === "acknowledgment_required" ||
    value === "gap_check_stale" ||
    value === "gap_check_failed" ||
    value === "already_published" ||
    value === "no_session"
  );
}

function isStatus(value: unknown): value is SpecPublishStatus {
  return typeof value === "object" && value !== null && "gate" in value && "lifecycle" in value;
}
