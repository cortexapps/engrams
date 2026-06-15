import { useMutation, useQuery, createConnectQueryKey } from "@connectrpc/connect-query";
import { useQueryClient } from "@tanstack/react-query";
import {
  listEnabledImages,
  enableImage,
  disableImage,
  refreshImage,
  listEnableJobs,
} from "../gen/engram/app/v1/image-ImageService_connectquery";
import type { EnabledImageSummary } from "../lib/types";
import type { EnabledImageSummary as ProtoEnabledImageSummary } from "../gen/engram/app/v1/image_pb";

function protoImageToLegacy(img: ProtoEnabledImageSummary): EnabledImageSummary {
  return {
    id: img.id,
    image_uri: img.imageUri,
    manifest_digest: img.manifestDigest,
    manifest_name: img.manifestName ?? null,
    manifest_description: img.manifestDescription ?? null,
    harness_name: img.harnessName ?? null,
    last_refreshed_at: img.lastRefreshedAt,
    created_at: img.createdAt,
  };
}

/** List of operator-enabled OCI image URIs. The coordinator stores a
 * snapshot of each URI's manifest.toml on enable, so this list is the
 * authoritative source of "what sessions can reference." */
export function useEnabledImages(enabled = true) {
  return useQuery(
    listEnabledImages,
    {},
    {
      select: (data) => data.images.map(protoImageToLegacy),
      enabled,
      refetchOnWindowFocus: true,
      staleTime: 10_000,
    },
  );
}

/** Mutation: enable an image (ADR 0036: async). The POST validates
 * the URI with a cheap metadata pull and returns 202 + an EnableJob;
 * progress arrives via `useEnableJobs`' polling. Errors from the
 * validation pull propagate verbatim — the panel renders them inline. */
export function useEnableImage() {
  const qc = useQueryClient();
  return useMutation(enableImage, {
    onSuccess: () => {
      qc.invalidateQueries({
        queryKey: createConnectQueryKey({
          schema: listEnableJobs,
          input: {},
          cardinality: "finite",
        }),
      });
    },
  });
}

/** Mutation: disable. The artifact in the registry is untouched —
 * only the Postgres row is removed, so future sessions can't
 * reference the URI. */
export function useDisableImage() {
  const qc = useQueryClient();
  return useMutation(disableImage, {
    onSuccess: () => {
      qc.invalidateQueries({
        queryKey: createConnectQueryKey({
          schema: listEnabledImages,
          input: {},
          cardinality: "finite",
        }),
      });
    },
  });
}

/** Mutation: refresh — re-runs the enable pipeline for an already-
 * enabled URI (ADR 0036: async, returns 202 + an EnableJob). Useful
 * when a moved tag (e.g. `:latest`) now resolves to a new digest.
 * The row's `id` and `created_at` are preserved by the upsert. */
export function useRefreshEnabledImage() {
  const qc = useQueryClient();
  return useMutation(refreshImage, {
    onSuccess: () => {
      qc.invalidateQueries({
        queryKey: createConnectQueryKey({
          schema: listEnableJobs,
          input: {},
          cardinality: "finite",
        }),
      });
    },
  });
}
