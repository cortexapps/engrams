import type { ListHostsResponse, ListSessionsResponse, Session } from './types';

// Same-origin in dev (Vite proxy → :8090). In a hosted prod build,
// the SPA must run behind something that proxies these paths; we don't
// support cross-origin auth here.
const BASE = '';

async function getJSON<T>(path: string): Promise<T> {
  const res = await fetch(`${BASE}${path}`, {
    headers: { Accept: 'application/json' },
  });
  if (!res.ok) {
    throw new Error(`${path} → ${res.status} ${res.statusText}`);
  }
  return res.json() as Promise<T>;
}

export const fetchSessions = () =>
  getJSON<ListSessionsResponse>('/sessions').then((r) => r.sessions);

export const fetchHosts = () =>
  getJSON<ListHostsResponse>('/api/hosts').then((r) => r.hosts);

export const fetchSession = (id: string) =>
  getJSON<Session>(`/sessions/${id}`);
