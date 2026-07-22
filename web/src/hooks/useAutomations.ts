import { createConnectQueryKey, useMutation, useQuery } from "@connectrpc/connect-query";
import { useQueryClient } from "@tanstack/react-query";

import {
  archiveAutomation,
  createAutomation,
  getAutomation,
  listAutomationRuns,
  listAutomations,
  listWebhookSamples,
  setAutomationEnabled,
  testRender,
  updateAutomation,
} from "@/gen/engram/app/v1/automation-AutomationService_connectquery";
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
  return useQuery(getAutomation, { id: id ?? "" }, { enabled: !!id });
}

export function useAutomationRuns(id: string | undefined, limit = 25) {
  return useQuery(
    listAutomationRuns,
    { automationId: id ?? "", limit },
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

export function useWebhookSamples(registrationId: string | undefined, limit = 25) {
  return useQuery(
    listWebhookSamples,
    { registrationId: registrationId ?? "", limit },
    { enabled: !!registrationId, staleTime: 5_000 },
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
        queryKey: createConnectQueryKey({ schema: listAutomationRuns, cardinality: "finite" }),
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

export function useUpdateAutomation() {
  const invalidate = useInvalidateAutomations();
  return useMutation(updateAutomation, { onSuccess: invalidate });
}

export function useArchiveAutomation() {
  const invalidate = useInvalidateAutomations();
  return useMutation(archiveAutomation, { onSuccess: invalidate });
}

export function useSetAutomationEnabled() {
  const invalidate = useInvalidateAutomations();
  return useMutation(setAutomationEnabled, { onSuccess: invalidate });
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
