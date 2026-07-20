import { useMutation, useQuery, createConnectQueryKey } from "@connectrpc/connect-query";
import { useQueryClient } from "@tanstack/react-query";
import {
  listProfiles,
  getProfile,
  createProfile,
  updateProfile,
  deleteProfile,
} from "../gen/engram/app/v1/profile-ProfileService_connectquery";

/** Active profiles (the picker menu). Admins can pass includeArchived. */
export function useProfiles(includeArchived = false) {
  return useQuery(listProfiles, { includeArchived }, { staleTime: 10_000 });
}

export function useProfile(id: string | undefined) {
  return useQuery(getProfile, { id: id ?? "" }, { enabled: !!id });
}

/**
 * Invalidate profile reads after a mutation: every listProfiles variant
 * (archived + active) AND every getProfile(id) — otherwise an open editor keeps
 * a stale snapshot and can silently re-assert an out-of-date designation.
 */
function useInvalidateProfiles() {
  const qc = useQueryClient();
  return async () => {
    await Promise.all([
      qc.invalidateQueries({
        queryKey: createConnectQueryKey({ schema: listProfiles, cardinality: "finite" }),
      }),
      qc.invalidateQueries({
        queryKey: createConnectQueryKey({ schema: getProfile, cardinality: "finite" }),
      }),
    ]);
  };
}

export function useCreateProfile() {
  const invalidate = useInvalidateProfiles();
  return useMutation(createProfile, { onSuccess: invalidate });
}

export function useUpdateProfile() {
  const invalidate = useInvalidateProfiles();
  return useMutation(updateProfile, { onSuccess: invalidate });
}

export function useDeleteProfile() {
  const invalidate = useInvalidateProfiles();
  return useMutation(deleteProfile, { onSuccess: invalidate });
}
