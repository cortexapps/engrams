/** Editor-side hooks (ADR 0119 phase 3.3). Kept apart from useAutomations.ts
 * (the list page's hooks) so the two surfaces evolve independently. */

import { createConnectQueryKey, useMutation, useQuery } from "@connectrpc/connect-query";
import { useQueryClient } from "@tanstack/react-query";

import {
  archiveAutomation,
  createAutomation,
  duplicateAutomation,
  getAutomation,
  listActionCatalog,
  listAutomations,
  listEventCatalog,
  listVersions,
  saveVersion,
  setAutomationEnabled,
  setBlockOverrides,
  updateAutomationMeta,
} from "@/gen/engram/app/v1/automation-AutomationService_connectquery";
import {
  createWebhookRegistration,
  listWebhookRegistrations,
} from "@/gen/engram/app/v1/automation-WebhookRegistrationService_connectquery";

export function useEditorAutomation(id: string | undefined) {
  return useQuery(getAutomation, { lookup: { case: "id", value: id ?? "" } }, { enabled: !!id });
}

export function useEventCatalog(provider: string | undefined) {
  return useQuery(
    listEventCatalog,
    { provider: provider ?? "" },
    { enabled: !!provider, staleTime: 60_000 },
  );
}

export function useActionCatalog(provider: string | undefined) {
  return useQuery(
    listActionCatalog,
    { provider: provider ?? "" },
    { enabled: !!provider, staleTime: 60_000 },
  );
}

/** Every version of an automation, newest first (phase 3.8 Settings). */
export function useAutomationVersions(automationId: string | undefined) {
  return useQuery(
    listVersions,
    { automationId: automationId ?? "" },
    { enabled: !!automationId, staleTime: 10_000 },
  );
}

export function useEditorWebhookRegistrations() {
  return useQuery(listWebhookRegistrations, {}, { staleTime: 10_000 });
}

function useInvalidateEditor() {
  const queryClient = useQueryClient();
  return () => {
    void queryClient.invalidateQueries({
      queryKey: createConnectQueryKey({ schema: getAutomation, cardinality: "finite" }),
    });
    void queryClient.invalidateQueries({
      queryKey: createConnectQueryKey({ schema: listAutomations, cardinality: "finite" }),
    });
  };
}

export function useCreateAutomationV2() {
  const invalidate = useInvalidateEditor();
  return useMutation(createAutomation, { onSuccess: invalidate });
}

export function useSaveVersionV2() {
  const invalidate = useInvalidateEditor();
  return useMutation(saveVersion, { onSuccess: invalidate });
}

export function useSetBlockOverrides() {
  const invalidate = useInvalidateEditor();
  return useMutation(setBlockOverrides, { onSuccess: invalidate });
}

export function useUpdateAutomationMetaV2() {
  const invalidate = useInvalidateEditor();
  return useMutation(updateAutomationMeta, { onSuccess: invalidate });
}

export function useSetAutomationEnabledV2() {
  const invalidate = useInvalidateEditor();
  return useMutation(setAutomationEnabled, { onSuccess: invalidate });
}

export function useDuplicateAutomation() {
  const invalidate = useInvalidateEditor();
  return useMutation(duplicateAutomation, { onSuccess: invalidate });
}

export function useArchiveAutomationV2() {
  const invalidate = useInvalidateEditor();
  return useMutation(archiveAutomation, { onSuccess: invalidate });
}

export function useCreateWebhookRegistrationV2() {
  const queryClient = useQueryClient();
  return useMutation(createWebhookRegistration, {
    onSuccess: () =>
      void queryClient.invalidateQueries({
        queryKey: createConnectQueryKey({
          schema: listWebhookRegistrations,
          cardinality: "finite",
        }),
      }),
  });
}
