import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import {
  disableImage,
  enableImage,
  fetchEnabledImages,
  refreshEnabledImage,
} from '../api';

const KEY = ['enabled-images'] as const;

/** List of operator-enabled OCI image URIs. The coordinator stores a
 * snapshot of each URI's manifest.toml on enable, so this list is the
 * authoritative source of "what sessions can reference." */
export function useEnabledImages(enabled = true) {
  return useQuery({
    queryKey: KEY,
    queryFn: fetchEnabledImages,
    enabled,
    refetchOnWindowFocus: true,
    staleTime: 10_000,
  });
}

/** Mutation: enable an image (ADR 0036: async). The POST validates
 * the URI with a cheap metadata pull and returns 202 + an EnableJob;
 * progress arrives via `useEnableJobs`' polling. Errors from the
 * validation pull propagate verbatim — the panel renders them inline. */
export function useEnableImage() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (imageUri: string) => enableImage(imageUri),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ['enable-jobs'] });
    },
  });
}

/** Mutation: disable. The artifact in the registry is untouched —
 * only the Postgres row is removed, so future sessions can't
 * reference the URI. */
export function useDisableImage() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (imageUri: string) => disableImage(imageUri),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: KEY });
    },
  });
}

/** Mutation: refresh — re-runs the enable pipeline for an already-
 * enabled URI (ADR 0036: async, returns 202 + an EnableJob). Useful
 * when a moved tag (e.g. `:latest`) now resolves to a new digest.
 * The row's `id` and `created_at` are preserved by the upsert. */
export function useRefreshEnabledImage() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (imageUri: string) => refreshEnabledImage(imageUri),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ['enable-jobs'] });
    },
  });
}
