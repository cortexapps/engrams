import { useMutation, useQuery, createConnectQueryKey } from "@connectrpc/connect-query";
import { useQueryClient } from "@tanstack/react-query";
import {
  listEnabledImages,
  enableImage,
  updateImage,
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
    name: img.config?.name ?? null,
    description: img.config?.description ?? null,
    env: img.config?.env ?? {},
    workdir: img.config?.workdir ?? null,
    suggested_vcpus: img.config?.resources?.suggestedVcpus ?? null,
    suggested_memory_mib: img.config?.resources?.suggestedMemoryMib ?? null,
    suggested_disk_gib: img.config?.resources?.suggestedDiskGib ?? null,
    warm_command: img.config?.warm?.command ?? [],
    // uint64 on the wire; any plausible hook timeout fits a JS number, and
    // the edit form round-trips it back through BigInt() losslessly.
    warm_timeout_secs:
      img.config?.warm?.timeoutSecs != null ? Number(img.config.warm.timeoutSecs) : null,
    warm_workdir: img.config?.warm?.workdir ?? null,
    warm_network: img.config?.warm?.network
      ? {
          // The proto carries a plain string; the writer (this form + the
          // profile editor) only ever sends "deny" | "allow".
          default: img.config.warm.network.default === "allow" ? "allow" : "deny",
          allow_hosts: img.config.warm.network.allowHosts,
          allow_host_patterns: img.config.warm.network.allowHostPatterns,
        }
      : null,
    last_refreshed_at: img.lastRefreshedAt,
    created_at: img.createdAt,
    // Flatten the proto `value` oneof: secretRef → "secret_ref", everything
    // else (literal, or an unset case) → "literal" with its string value.
    capture_env: (img.config?.warm?.env ?? []).map((e) => ({
      name: e.name,
      kind: e.value.case === "secretRef" ? ("secret_ref" as const) : ("literal" as const),
      value: e.value.value ?? "",
    })),
  };
}

/** List of operator-enabled OCI image URIs. The coordinator stores the
 * RPC-supplied ImageConfig for each URI (ADR 0080), so this list is the
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

/** Mutation: edit an enabled image's config (ADR 0080, full replace).
 * Cheap fields (name/description/env/workdir) apply immediately — the
 * enabled row changes in place; a diff touching resources or warm needs
 * `allowRecapture: true` and spawns a recapture EnableJob instead (and
 * fails FailedPrecondition naming the fields without it). Invalidate
 * both list consumers: the row for the cheap path, the jobs table for
 * the recapture path. */
export function useUpdateImage() {
  const qc = useQueryClient();
  return useMutation(updateImage, {
    onSuccess: () => {
      qc.invalidateQueries({
        queryKey: createConnectQueryKey({
          schema: listEnabledImages,
          input: {},
          cardinality: "finite",
        }),
      });
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
