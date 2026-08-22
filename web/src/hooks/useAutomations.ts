import { createConnectQueryKey, useMutation, useQuery } from "@connectrpc/connect-query";
import { useQueryClient } from "@tanstack/react-query";

import {
  archiveAutomation,
  createAutomation,
  duplicateAutomation,
  getAutomation,
  listAutomations,
  listEventSamples,
  runNow,
  saveVersion,
  setAutomationEnabled,
  setBlockOverrides,
  setInputs,
  testRender,
  updateAutomationMeta,
} from "@/gen/engram/app/v1/automation-AutomationService_connectquery";
import { listRuns } from "@/gen/engram/app/v1/automation-AutomationRunService_connectquery";
import {
  createWebhookRegistration,
  deleteWebhookRegistration,
  listWebhookEvents,
  listWebhookRegistrations,
} from "@/gen/engram/app/v1/automation-WebhookRegistrationService_connectquery";

export function useAutomations(includeArchived = false) {
  return useQuery(listAutomations, { includeArchived }, { staleTime: 10_000 });
}

export function useAutomation(id: string | undefined) {
  return useQuery(getAutomation, { lookup: { case: "id", value: id ?? "" } }, { enabled: !!id });
}

/** A built-in by its stable key (e.g. "pr_review") — resolves to null rather
 * than erroring when the built-in has not been seeded yet. */
export function useBuiltinAutomation(builtinKey: string) {
  return useQuery(
    getAutomation,
    { lookup: { case: "builtinKey", value: builtinKey } },
    { staleTime: 30_000, retry: false },
  );
}

export function useAutomationRuns(id: string | undefined, limit = 25) {
  return useQuery(
    listRuns,
    { automationId: id ?? "", limit, includeFiltered: true },
    { enabled: !!id, staleTime: 5_000 },
  );
}

export function useWebhookRegistrations() {
  return useQuery(listWebhookRegistrations, {}, { staleTime: 10_000 });
}

export function useWebhookEvents(registrationId: string | undefined) {
  return useQuery(
    listWebhookEvents,
    { registrationId: registrationId ?? "" },
    { enabled: !!registrationId, staleTime: 10_000 },
  );
}

/** Stored deliveries for a saved automation's trigger source. */
export function useEventSamples(automationId: string | undefined, limit = 25) {
  return useQuery(
    listEventSamples,
    { automationId: automationId ?? "", limit },
    { enabled: !!automationId, staleTime: 5_000 },
  );
}

function useInvalidateAutomations() {
  const queryClient = useQueryClient();
  return async () => {
    await Promise.all([
      queryClient.invalidateQueries({
        queryKey: createConnectQueryKey({ schema: listAutomations, cardinality: "finite" }),
      }),
      queryClient.invalidateQueries({
        queryKey: createConnectQueryKey({ schema: getAutomation, cardinality: "finite" }),
      }),
      queryClient.invalidateQueries({
        queryKey: createConnectQueryKey({ schema: listRuns, cardinality: "finite" }),
      }),
    ]);
  };
}

function useInvalidateRegistrations() {
  const queryClient = useQueryClient();
  return async () => {
    await Promise.all([
      queryClient.invalidateQueries({
        queryKey: createConnectQueryKey({
          schema: listWebhookRegistrations,
          input: {},
          cardinality: "finite",
        }),
      }),
      queryClient.invalidateQueries({
        queryKey: createConnectQueryKey({ schema: listWebhookEvents, cardinality: "finite" }),
      }),
    ]);
  };
}

export function useCreateAutomation() {
  const invalidate = useInvalidateAutomations();
  return useMutation(createAutomation, { onSuccess: invalidate });
}

export function useSaveVersion() {
  const invalidate = useInvalidateAutomations();
  return useMutation(saveVersion, { onSuccess: invalidate });
}

export function useUpdateAutomationMeta() {
  const invalidate = useInvalidateAutomations();
  return useMutation(updateAutomationMeta, { onSuccess: invalidate });
}

export function useArchiveAutomation() {
  const invalidate = useInvalidateAutomations();
  return useMutation(archiveAutomation, { onSuccess: invalidate });
}

export function useSetAutomationEnabled() {
  const invalidate = useInvalidateAutomations();
  return useMutation(setAutomationEnabled, { onSuccess: invalidate });
}

export function useDuplicateAutomation() {
  const invalidate = useInvalidateAutomations();
  return useMutation(duplicateAutomation, { onSuccess: invalidate });
}

export function useSetInputs() {
  const invalidate = useInvalidateAutomations();
  return useMutation(setInputs, { onSuccess: invalidate });
}

export function useSetBlockOverrides() {
  const invalidate = useInvalidateAutomations();
  return useMutation(setBlockOverrides, { onSuccess: invalidate });
}

export function useRunNow() {
  const invalidate = useInvalidateAutomations();
  return useMutation(runNow, { onSuccess: invalidate });
}

export function useTestAutomationRender() {
  return useMutation(testRender);
}

export function useCreateWebhookRegistration() {
  const invalidate = useInvalidateRegistrations();
  return useMutation(createWebhookRegistration, { onSuccess: invalidate });
}

export function useDeleteWebhookRegistration() {
  const invalidate = useInvalidateRegistrations();
  return useMutation(deleteWebhookRegistration, { onSuccess: invalidate });
}
