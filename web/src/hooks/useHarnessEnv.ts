import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { API_BASE } from "../lib/base";

/**
 * The per-user harness credentials (ADR 0063 B3). Each registered harness's
 * descriptor declares an `auth.user_env` (the human credential env-var name); the
 * settings page is the union of those across the catalog, each with whether the
 * caller has set it. There is no Claude-specific "claude token" concept — this is
 * just "the env vars the registered harnesses ask for."
 */
export interface HarnessEnvVar {
  /** The env-var name the value is sealed under (e.g. CLAUDE_CODE_OAUTH_TOKEN). */
  envVar: string;
  /** The registered harnesses that ask for this env var. */
  harnesses: Array<{ name: string; label: string }>;
  /** Whether the caller has a value sealed for it. */
  present: boolean;
}

const QUERY_KEY = ["me", "harness-env"];

async function fetchHarnessEnv(): Promise<HarnessEnvVar[]> {
  const res = await fetch(`${API_BASE}/me/harness-env`, {
    headers: { Accept: "application/json" },
    credentials: "include",
  });
  if (!res.ok) {
    // 401 → session lapsed; treat as "no vars" rather than throwing.
    if (res.status === 401) return [];
    throw new Error(`/me/harness-env → ${res.status}`);
  }
  const body = (await res.json()) as { vars?: HarnessEnvVar[] };
  return body.vars ?? [];
}

/** The catalog's `user_env` union with per-user presence. */
export function useHarnessEnv(enabled = true) {
  return useQuery({
    queryKey: QUERY_KEY,
    queryFn: fetchHarnessEnv,
    enabled,
    staleTime: 60_000,
    refetchOnWindowFocus: false,
  });
}

/** Seal a value under an env-var name the catalog declares. */
export function useSetHarnessEnv() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: async ({ envVar, value }: { envVar: string; value: string }) => {
      const res = await fetch(`${API_BASE}/me/harness-env/${encodeURIComponent(envVar)}`, {
        method: "PUT",
        headers: { "Content-Type": "application/json" },
        credentials: "include",
        body: JSON.stringify({ value }),
      });
      if (!res.ok) throw new Error(`/me/harness-env PUT → ${res.status}`);
    },
    onSuccess: () => void qc.invalidateQueries({ queryKey: QUERY_KEY }),
  });
}

/** Clear a sealed value (idempotent). */
export function useDeleteHarnessEnv() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: async (envVar: string) => {
      const res = await fetch(`${API_BASE}/me/harness-env/${encodeURIComponent(envVar)}`, {
        method: "DELETE",
        credentials: "include",
      });
      if (!res.ok) throw new Error(`/me/harness-env DELETE → ${res.status}`);
    },
    onSuccess: () => void qc.invalidateQueries({ queryKey: QUERY_KEY }),
  });
}
