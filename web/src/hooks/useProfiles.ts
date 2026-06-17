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

/** Invalidate every listProfiles variant (archived + active) after a mutation. */
function useInvalidateProfiles() {
  const qc = useQueryClient();
  return () =>
    qc.invalidateQueries({
      queryKey: createConnectQueryKey({ schema: listProfiles, cardinality: "finite" }),
    });
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
