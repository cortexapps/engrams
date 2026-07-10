/**
 * API-key hooks (ADR 0086).
 *
 * Global API keys live on the orchestrator's native Connect `ApiKeyService`
 * (admin-only). List returns metadata + a masked preview — never the key
 * material; the plaintext appears exactly once in the CreateApiKey response
 * and is unrecoverable afterwards (only its hash is stored).
 */

import { useMutation, useQuery, createConnectQueryKey } from "@connectrpc/connect-query";
import { useQueryClient } from "@tanstack/react-query";
import {
  createApiKey,
  listApiKeys,
  revokeApiKey,
} from "../gen/engram/app/v1/api_key-ApiKeyService_connectquery";

/** All keys (metadata + masked preview) for the management panel. */
export function useApiKeys() {
  return useQuery(listApiKeys, {}, { staleTime: 10_000 });
}

function useInvalidateApiKeys() {
  const qc = useQueryClient();
  return () =>
    qc.invalidateQueries({
      queryKey: createConnectQueryKey({ schema: listApiKeys, input: {}, cardinality: "finite" }),
    });
}

/** Mint a key. The response's `key` is the one-time plaintext — show it NOW. */
export function useCreateApiKey() {
  const invalidate = useInvalidateApiKeys();
  return useMutation(createApiKey, { onSuccess: invalidate });
}

/** Revoke a key by id (idempotent) — it stops authenticating immediately. */
export function useRevokeApiKey() {
  const invalidate = useInvalidateApiKeys();
  return useMutation(revokeApiKey, { onSuccess: invalidate });
}
