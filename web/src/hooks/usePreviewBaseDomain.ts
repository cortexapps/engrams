/**
 * The deployment's preview base domain (ADR 0118), read from the public
 * `/api/v1/auth-config` posture endpoint.
 *
 * Used to show what a `${…_INGRESS_URL}` reference will resolve to while
 * editing a profile. Deployment-static, so it is cached hard; `undefined` when
 * the deployment publishes no preview domain, and on any fetch error — every
 * caller must treat it as optional and simply render less.
 */

import { useQuery } from "@tanstack/react-query";
import { API_BASE } from "../lib/base";

interface AuthConfig {
  previewBaseDomain?: string;
}

export function usePreviewBaseDomain(): string | undefined {
  const { data } = useQuery({
    queryKey: ["auth-config"],
    queryFn: async (): Promise<AuthConfig> => {
      const res = await fetch(`${API_BASE}/auth-config`, { credentials: "same-origin" });
      if (!res.ok) throw new Error(`auth-config → ${res.status}`);
      return (await res.json()) as AuthConfig;
    },
    // Fixed per deployment. Never refetch on focus, and don't retry — a missing
    // domain degrades the hint, it does not break the editor.
    staleTime: Infinity,
    gcTime: Infinity,
    retry: false,
    refetchOnWindowFocus: false,
  });
  return data?.previewBaseDomain;
}
