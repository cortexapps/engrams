import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { API_BASE } from "../lib/base";

export interface OAuthCredentialEntry {
  kind: "oauth";
  provider: string;
  harnesses: Array<{ name: string; label: string }>;
  hint?: string;
  connected: boolean;
  version?: number;
  account?: {
    displayName?: string;
    planType?: string;
    workspaceId?: string;
    workspaceName?: string;
  };
  createdAt?: string;
  updatedAt?: string;
}

export interface SecretCredentialEntry {
  kind: "secret_env";
  envVar: string;
  harnesses: Array<{ name: string; label: string }>;
  hint?: string;
  connected: boolean;
}

/** ADR 0115: one entry per connector declaring user-scoped support, joined
 * against the caller's personal credential (PAT or OAuth — one slot). */
export interface ConnectorCredentialEntry {
  kind: "connector";
  provider: string;
  display: { name: string };
  modes: { oauth: boolean; token: boolean };
  tokenHint?: string;
  connected: boolean;
  /** "connected" | "expired" | "broken" | "revoked" | "" (no credential). */
  status: string;
  version?: number;
  account?: {
    displayName?: string;
    workspaceName?: string;
  };
  updatedAt?: string;
}

export type CredentialEntry =
  | OAuthCredentialEntry
  | SecretCredentialEntry
  | ConnectorCredentialEntry;
export interface OAuthFlow {
  id: string;
  provider: string;
  status: string;
  expiresAt: string;
  errorCode?: string;
}

const QUERY_KEY = ["me", "credentials"];

function invalidateCredentialList(queryClient: ReturnType<typeof useQueryClient>) {
  return queryClient.invalidateQueries({ queryKey: QUERY_KEY, exact: true });
}

async function request<T>(path: string, init?: RequestInit): Promise<T> {
  const response = await fetch(`${API_BASE}${path}`, {
    credentials: "include",
    headers: { Accept: "application/json", ...init?.headers },
    ...init,
  });
  if (!response.ok) throw new Error(`${path} → ${response.status}`);
  if (response.status === 204) return undefined as T;
  return response.json() as Promise<T>;
}

export function useCredentials(enabled = true) {
  return useQuery({
    queryKey: QUERY_KEY,
    queryFn: async () =>
      (await request<{ credentials?: CredentialEntry[] }>("/me/credentials")).credentials ?? [],
    enabled,
    staleTime: 30_000,
    refetchOnWindowFocus: false,
  });
}

export function useConnectOAuth() {
  return useMutation({
    mutationFn: (provider: string) =>
      request<{ flow: OAuthFlow; verificationUrl: string; userCode: string }>(
        `/me/credentials/${encodeURIComponent(provider)}/connect`,
        { method: "POST" },
      ),
  });
}

export function useOAuthFlow(flowId: string | null) {
  const queryClient = useQueryClient();
  return useQuery({
    queryKey: ["me", "credentials", "flow", flowId],
    queryFn: async () => {
      const result = await request<{ flow: OAuthFlow }>(
        `/me/credentials/flows/${encodeURIComponent(flowId!)}`,
      );
      if (result.flow.status !== "pending") {
        // The flow query itself extends QUERY_KEY. Prefix invalidation here
        // re-invalidates this query from inside its own queryFn and can leave
        // the UI spinning on the previous pending result. Refresh only the
        // credential list that exposes the newly connected account.
        void invalidateCredentialList(queryClient);
      }
      return result.flow;
    },
    enabled: flowId != null,
    refetchInterval: (query) => (query.state.data?.status === "pending" ? 1_500 : false),
  });
}

export function useCancelOAuth() {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: (flowId: string) =>
      request<{ flow: OAuthFlow }>(`/me/credentials/flows/${encodeURIComponent(flowId)}/cancel`, {
        method: "POST",
      }),
    onSuccess: () => void invalidateCredentialList(queryClient),
  });
}

export function useDisconnectOAuth() {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: (provider: string) =>
      request<void>(`/me/credentials/${encodeURIComponent(provider)}`, { method: "DELETE" }),
    onSuccess: () => void invalidateCredentialList(queryClient),
  });
}

// ── ADR 0115: personal connector credentials ────────────────────────────────

/** Seal a personal access token for a connector (PAT mode). */
export function useSetConnectorCredential() {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: ({ provider, value }: { provider: string; value: string }) =>
      request<void>(`/me/connector-credentials/${encodeURIComponent(provider)}`, {
        method: "PUT",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ value }),
      }),
    onSuccess: () => void invalidateCredentialList(queryClient),
  });
}

/** Disconnect a personal connector credential (PAT or OAuth). */
export function useDeleteConnectorCredential() {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: (provider: string) =>
      request<void>(`/me/connector-credentials/${encodeURIComponent(provider)}`, {
        method: "DELETE",
      }),
    onSuccess: () => void invalidateCredentialList(queryClient),
  });
}

/** The user-subject OAuth authorize entry — a full browser navigation (the
 * IdP redirects back through the shared callback to /settings/credentials). */
export function connectorOAuthAuthorizeUrl(provider: string, force = false): string {
  const qs = force ? "?force=1" : "";
  return `${API_BASE}/me/connector-credentials/${encodeURIComponent(provider)}/oauth/authorize${qs}`;
}
