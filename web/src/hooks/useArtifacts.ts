import { createConnectQueryKey, useMutation, useQuery } from "@connectrpc/connect-query";
import { useQueryClient } from "@tanstack/react-query";

import {
  deleteArtifact,
  getArtifactRecord,
  listArtifacts,
  setArtifactVisibility,
} from "../gen/engram/app/v1/artifact-ArtifactService_connectquery";

export type ArtifactScope = "" | "mine" | "shared" | "all";

/** The registry list for one scope. Unpaginated (the server clamps a
 * page to 200; registries are document libraries, not event streams). */
export function useArtifacts(scope: ArtifactScope = "") {
  return useQuery(listArtifacts, { scope, page: 0, pageSize: 0 }, { staleTime: 15_000 });
}

/** One artifact + its version history (and a fresh raw_url token). The
 * token is short-lived, so refetch on focus keeps an open page's iframe
 * URL mintable; the record itself only changes on update/share. */
export function useArtifact(id: string | undefined) {
  return useQuery(getArtifactRecord, { id: id ?? "" }, { enabled: !!id, staleTime: 60_000 });
}

function useInvalidateArtifacts() {
  const qc = useQueryClient();
  return () => {
    qc.invalidateQueries({
      queryKey: createConnectQueryKey({ schema: listArtifacts, cardinality: "finite" }),
    });
    qc.invalidateQueries({
      queryKey: createConnectQueryKey({ schema: getArtifactRecord, cardinality: "finite" }),
    });
  };
}

/** Share ("org") / revoke ("private") — the page's Share toggle. */
export function useSetArtifactVisibility() {
  const invalidate = useInvalidateArtifacts();
  return useMutation(setArtifactVisibility, { onSuccess: invalidate });
}

export function useDeleteArtifact() {
  const invalidate = useInvalidateArtifacts();
  return useMutation(deleteArtifact, { onSuccess: invalidate });
}
