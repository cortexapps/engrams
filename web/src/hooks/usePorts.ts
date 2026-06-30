/**
 * Live-host port exposures (ADR 0064) — React Query hooks over the orchestrator
 * REST CRUD at `/api/v1/sessions/:id/ports`. Plain fetch (not a Connect service)
 * to match the rest of the session-scoped REST surface (artifacts, shell, …).
 */

import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { API_BASE } from "../lib/base";

export interface PortExposure {
  slug: string;
  sessionId: string;
  port: number;
  label: string;
  visibility: "private" | "shared";
  shareToken: string | null;
  /** The (eventual) preview URL — reachable once the edge proxy (P2b) is live. */
  url: string;
  createdAt: string;
  expiresAt: string | null;
}

export interface ExposeInput {
  port: number;
  label?: string;
  visibility?: "private" | "shared";
}

async function listPorts(sessionId: string): Promise<PortExposure[]> {
  const res = await fetch(`${API_BASE}/sessions/${sessionId}/ports`, {
    credentials: "include",
  });
  if (!res.ok) throw new Error(`ports list → ${res.status}`);
  const body = (await res.json()) as { exposures: PortExposure[] };
  return body.exposures;
}

async function exposePort(sessionId: string, input: ExposeInput): Promise<PortExposure> {
  const res = await fetch(`${API_BASE}/sessions/${sessionId}/ports`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    credentials: "include",
    body: JSON.stringify(input),
  });
  if (!res.ok) throw new Error(`ports create → ${res.status}`);
  return (await res.json()) as PortExposure;
}

async function revokePort(sessionId: string, slug: string): Promise<void> {
  const res = await fetch(`${API_BASE}/sessions/${sessionId}/ports/${slug}`, {
    method: "DELETE",
    credentials: "include",
  });
  // 404 is fine — already gone (idempotent revoke).
  if (!res.ok && res.status !== 404) throw new Error(`ports delete → ${res.status}`);
}

const portsKey = (sessionId: string) => ["ports", sessionId] as const;

export function usePorts(sessionId: string) {
  return useQuery({ queryKey: portsKey(sessionId), queryFn: () => listPorts(sessionId) });
}

export function useExposePort(sessionId: string) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (input: ExposeInput) => exposePort(sessionId, input),
    onSuccess: () => void qc.invalidateQueries({ queryKey: portsKey(sessionId) }),
  });
}

export function useRevokePort(sessionId: string) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (slug: string) => revokePort(sessionId, slug),
    onSuccess: () => void qc.invalidateQueries({ queryKey: portsKey(sessionId) }),
  });
}

/** The link to share for an exposure — adds the capability token for a shared
 * exposure, otherwise the plain preview URL. */
export function shareUrl(e: PortExposure): string {
  return e.visibility === "shared" && e.shareToken
    ? `${e.url}?token=${encodeURIComponent(e.shareToken)}`
    : e.url;
}

// ---------------------------------------------------------------------------
// Liveness (ADR 0064)
// ---------------------------------------------------------------------------

/** "up" = the guest port answers; "down" = no response; "unknown" = the server
 * declined to probe (session not running) — never a guess. */
export type PortHealth = "up" | "down" | "unknown";

async function fetchPortHealth(sessionId: string, slug: string): Promise<PortHealth> {
  const res = await fetch(`${API_BASE}/sessions/${sessionId}/ports/${slug}/health`, {
    credentials: "include",
  });
  if (!res.ok) return "unknown";
  const body = (await res.json()) as { status?: PortHealth };
  return body.status ?? "unknown";
}

/**
 * Poll one exposure's liveness — only while `enabled`. The caller passes the
 * session-is-active gate, so we never probe (and the server never opens the
 * relay, hence never auto-resumes) a suspended session.
 */
export function usePortHealth(sessionId: string, slug: string, enabled: boolean) {
  return useQuery({
    queryKey: ["port-health", sessionId, slug],
    queryFn: () => fetchPortHealth(sessionId, slug),
    enabled,
    refetchInterval: enabled ? 10_000 : false,
  });
}
