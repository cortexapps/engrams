import type {
  CreateSessionResponse,
  HarnessDescriptor,
  HarnessSpec,
  ImageDescriptor,
  ImageRef,
  ListHostsResponse,
  ListSessionsResponse,
  Session,
  WorkspaceSpec,
} from './types';

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

async function postJSON<T>(path: string, body: unknown): Promise<T> {
  const res = await fetch(`${BASE}${path}`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json', Accept: 'application/json' },
    body: JSON.stringify(body),
  });
  if (!res.ok) {
    // Surface the body as the error message — the coordinator's
    // ApiError serialises a useful message and the form needs it
    // to render "secret `X` not available", "image not found", etc.
    let detail = '';
    try {
      detail = await res.text();
    } catch {
      // ignore
    }
    throw new Error(detail || `${path} → ${res.status} ${res.statusText}`);
  }
  // Some endpoints (DELETE-style) return 204. We don't hit those here,
  // but be safe anyway.
  if (res.status === 204) return undefined as T;
  return res.json() as Promise<T>;
}

export const fetchSessions = () =>
  getJSON<ListSessionsResponse>('/sessions').then((r) => r.sessions);

export const fetchHosts = () =>
  getJSON<ListHostsResponse>('/api/hosts').then((r) => r.hosts);

export const fetchSession = (id: string) =>
  getJSON<Session>(`/sessions/${id}`);

export const fetchImages = () => getJSON<ImageDescriptor[]>('/api/images');

export const fetchHarnesses = () =>
  getJSON<HarnessDescriptor[]>('/api/harnesses');

export interface CreateSessionInput {
  image: ImageRef;
  workspace: WorkspaceSpec;
  /** Defaults to `{ kind: "none" }` server-side. */
  harness?: HarnessSpec;
  user_id?: string;
  prompt?: string;
  /** Map of env-var name → value. Honored only for SecretMode::Literal images. */
  secrets?: Record<string, string>;
}

export const createSession = (input: CreateSessionInput) =>
  postJSON<CreateSessionResponse>('/sessions', input);

export const sendPrompt = (sessionId: string, text: string) =>
  postJSON<{ session_id: string; note: string }>(
    `/sessions/${sessionId}/prompt`,
    { text },
  );
