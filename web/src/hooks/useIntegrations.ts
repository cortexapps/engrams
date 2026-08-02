/**
 * Integration hooks (ADR 0057 C3/C4).
 *
 * `useConnectors` + the connector mutations back the Plane-B catalog editor;
 * `useMintKinds` feeds the data-driven Plane-A mint form. Connectors live in the
 * orchestrator DB (built-ins are read-only seeds); mint kinds are the
 * coordinator's static registry, proxied. Both admin-only.
 */

import { useMutation, useQuery, createConnectQueryKey } from "@connectrpc/connect-query";
import { useQueryClient } from "@tanstack/react-query";
import {
  listConnectors,
  upsertConnector,
  deleteConnector,
  getIntegrationCatalog,
  setMintCredential,
  uploadConnectorLogo,
  testConnector,
  listConnections,
  createConnection,
  deleteConnection,
  testConnection,
  setConnectionEnabled,
  getGoogleCloudSetup,
} from "../gen/engram/app/v1/integration-IntegrationService_connectquery";
import { listMintKinds } from "../gen/engram/app/v1/mint-MintService_connectquery";
import { fallbackIdentity, type ProviderIdentity } from "../lib/connectorModel";
import { builtinLogo } from "../lib/connectorLogos";

/** Built-in seeds (read-only) + admin-authored connectors. Each carries `status`. */
export function useConnectors() {
  return useQuery(listConnectors, {}, { staleTime: 10_000 });
}

/** The member-readable provider catalog (display + powers + hosts; no secrets). */
export function useIntegrationCatalog() {
  return useQuery(getIntegrationCatalog, {}, { staleTime: 30_000 });
}

/** The coordinator's mint-kind registry (Plane-A form metadata). */
export function useMintKinds() {
  return useQuery(listMintKinds, {}, { staleTime: 60_000 });
}

function useInvalidateConnectors() {
  const qc = useQueryClient();
  return () =>
    qc.invalidateQueries({
      queryKey: createConnectQueryKey({ schema: listConnectors, input: {}, cardinality: "finite" }),
    });
}

function useInvalidateCatalog() {
  const qc = useQueryClient();
  return () =>
    qc.invalidateQueries({
      queryKey: createConnectQueryKey({
        schema: getIntegrationCatalog,
        input: {},
        cardinality: "finite",
      }),
    });
}

/** Create or replace a custom connector (validated coordinator-side at load + server-side here). */
export function useUpsertConnector() {
  const invalidate = useInvalidateConnectors();
  return useMutation(upsertConnector, { onSuccess: invalidate });
}

/** Delete a custom connector (idempotent; built-ins are rejected server-side). */
export function useDeleteConnector() {
  const invalidate = useInvalidateConnectors();
  const invalidateCatalog = useInvalidateCatalog();
  return useMutation(deleteConnector, {
    onSuccess: () => {
      invalidate();
      invalidateCatalog();
    },
  });
}

/** Store a mint kind's credentials (seals `<kind>.<field>` org secrets). Admin-only.
 * Refreshes connectors so the derived connected/available status updates. */
export function useSetMintCredential() {
  const invalidate = useInvalidateConnectors();
  return useMutation(setMintCredential, { onSuccess: invalidate });
}

/** Upload/replace (or, with empty bytes, clear) a connector's logo. Admin-only.
 * Refreshes the catalog so the `icon.logo` overlay updates. */
export function useUploadConnectorLogo() {
  const invalidateCatalog = useInvalidateCatalog();
  return useMutation(uploadConnectorLogo, { onSuccess: invalidateCatalog });
}

/** Test a connector's credential (admin). Draft values test an about-to-be-saved
 * credential before sealing; empty tests the stored one. Returns {ok, message}. */
export function useTestConnector() {
  return useMutation(testConnector);
}

export function useIntegrationConnections() {
  return useQuery(listConnections, {}, { staleTime: 10_000 });
}

function useInvalidateConnections() {
  const qc = useQueryClient();
  return () =>
    qc.invalidateQueries({
      queryKey: createConnectQueryKey({
        schema: listConnections,
        input: {},
        cardinality: "finite",
      }),
    });
}

export function useCreateConnection() {
  const invalidate = useInvalidateConnections();
  return useMutation(createConnection, { onSuccess: invalidate });
}

export function useDeleteConnection() {
  const invalidate = useInvalidateConnections();
  return useMutation(deleteConnection, { onSuccess: invalidate });
}

export function useTestConnection() {
  const invalidate = useInvalidateConnections();
  return useMutation(testConnection, { onSuccess: invalidate });
}

export function useSetConnectionEnabled() {
  const invalidate = useInvalidateConnections();
  return useMutation(setConnectionEnabled, { onSuccess: invalidate });
}

export function useGoogleCloudSetup(id: string) {
  return useQuery(
    getGoogleCloudSetup,
    { id },
    { enabled: id.length > 0, staleTime: Number.POSITIVE_INFINITY },
  );
}

/**
 * Resolve a provider's full display identity from the member catalog, falling
 * back to the deterministic monogram identity for an unknown provider (e.g. a
 * session event naming a since-removed connector). Logo precedence:
 * bundled built-in → uploaded (catalog serve URL) → monogram.
 */
export function useProviderIdentity(provider: string): ProviderIdentity {
  const { data } = useIntegrationCatalog();
  const entry = data?.providers.find((p) => p.provider === provider);
  const fallback = fallbackIdentity(provider);
  if (!entry) return fallback;
  const d = entry.display;
  const logo = builtinLogo(provider) ?? (d?.icon?.logo || undefined);
  return {
    provider,
    name: d?.name || fallback.name,
    category: d?.category || fallback.category,
    blurb: d?.blurb ?? "",
    icon: {
      mono: d?.icon?.mono || fallback.icon.mono,
      color: d?.icon?.color || fallback.icon.color,
      ...(logo ? { logo } : {}),
    },
  };
}
