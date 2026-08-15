/**
 * Session apps (ADR 0118) — React Query hooks over the orchestrator REST CRUD
 * at `/api/v1/sessions/:id/apps`. Plain fetch (not a Connect service) to match
 * the rest of the session-scoped REST surface (artifacts, shell, …).
 */

import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { API_BASE } from "../lib/base";

export interface SessionApp {
  /** The routing key — `<name>-<session slug>`, one DNS label. */
  hostLabel: string;
  sessionId: string;
  name: string;
  port: number;
  /** "org" = any logged-in teammate; "private" = owner + admin only. */
  visibility: "org" | "private";
  url: string;
  createdAt: string;
}

export interface ReserveAppInput {
  port: number;
  /** Omitted → the server derives `port-<port>`. */
  name?: string;
  visibility?: "org" | "private";
}

async function listApps(sessionId: string): Promise<SessionApp[]> {
  const res = await fetch(`${API_BASE}/sessions/${sessionId}/apps`, {
    credentials: "include",
  });
  if (!res.ok) throw new Error(`apps list → ${res.status}`);
  const body = (await res.json()) as { apps: SessionApp[] };
  return body.apps;
}

async function reserveApp(sessionId: string, input: ReserveAppInput): Promise<SessionApp> {
  const res = await fetch(`${API_BASE}/sessions/${sessionId}/apps`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    credentials: "include",
    body: JSON.stringify(input),
  });
  if (!res.ok) throw new Error(`apps create → ${res.status}`);
  return (await res.json()) as SessionApp;
}

async function revokeApp(sessionId: string, hostLabel: string): Promise<void> {
  const res = await fetch(`${API_BASE}/sessions/${sessionId}/apps/${hostLabel}`, {
    method: "DELETE",
    credentials: "include",
  });
  // 404 is fine — already gone (idempotent revoke).
  if (!res.ok && res.status !== 404) throw new Error(`apps delete → ${res.status}`);
}

const appsKey = (sessionId: string) => ["apps", sessionId] as const;

export function useApps(sessionId: string) {
  return useQuery({ queryKey: appsKey(sessionId), queryFn: () => listApps(sessionId) });
}

export function useReserveApp(sessionId: string) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (input: ReserveAppInput) => reserveApp(sessionId, input),
    onSuccess: () => void qc.invalidateQueries({ queryKey: appsKey(sessionId) }),
  });
}

export function useRevokeApp(sessionId: string) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (hostLabel: string) => revokeApp(sessionId, hostLabel),
    onSuccess: () => void qc.invalidateQueries({ queryKey: appsKey(sessionId) }),
  });
}

// ---------------------------------------------------------------------------
// Liveness
// ---------------------------------------------------------------------------

/** "up" = the guest port answers; "down" = no response; "unknown" = the server
 * declined to probe (session not running) — never a guess. */
export type AppHealth = "up" | "down" | "unknown";

async function fetchAppHealth(sessionId: string, hostLabel: string): Promise<AppHealth> {
  const res = await fetch(`${API_BASE}/sessions/${sessionId}/apps/${hostLabel}/health`, {
    credentials: "include",
  });
  if (!res.ok) return "unknown";
  const body = (await res.json()) as { status?: AppHealth };
  return body.status ?? "unknown";
}

/**
 * Poll one app's liveness — only while `enabled`. The caller passes the
 * session-is-active gate, so we never probe (and the server never opens the
 * relay, hence never auto-resumes) a suspended session.
 */
export function useAppHealth(sessionId: string, hostLabel: string, enabled: boolean) {
  return useQuery({
    queryKey: ["app-health", sessionId, hostLabel],
    queryFn: () => fetchAppHealth(sessionId, hostLabel),
    enabled,
    refetchInterval: enabled ? 10_000 : false,
  });
}
