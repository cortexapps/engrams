import type {
  AddRegistryRequest,
  AddRegistryResponse,
  CreateSessionResponse,
  EnabledImageSummary,
  ImageRef,
  ListEnabledImagesResponse,
  ListHostsResponse,
  ListRegistriesResponse,
  ListSessionsResponse,
  Session,
  SessionCowStateResponse,
  SessionMode,
  StorageSummaryResponse,
} from './types';

// Every coordinator HTTP route lives under `/api/v1`. The SPA owns the
// root path namespace (`/`, `/sessions/:id`, `/settings/...`), so a
// browser deep-link to `/sessions/:id` resolves to index.html (the
// reverse proxy proxies only `/api/` to the coord) instead of colliding
// with the `GET /sessions/:id` API route. Same-origin in dev (Vite
// proxy → :8090) and prod (nginx). Exported so the SSE/WS/artifact
// helpers build on the same base.
export const API_BASE = '/api/v1';

async function getJSON<T>(path: string): Promise<T> {
  const res = await fetch(`${API_BASE}${path}`, {
    headers: { Accept: 'application/json' },
  });
  if (!res.ok) {
    throw new Error(`${path} → ${res.status} ${res.statusText}`);
  }
  return res.json() as Promise<T>;
}

async function postJSON<T>(path: string, body: unknown): Promise<T> {
  const res = await fetch(`${API_BASE}${path}`, {
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
  if (res.status === 204) return undefined as T;
  return res.json() as Promise<T>;
}

async function deleteEmpty(path: string): Promise<void> {
  const res = await fetch(`${API_BASE}${path}`, { method: 'DELETE' });
  if (!res.ok) {
    let detail = '';
    try {
      detail = await res.text();
    } catch {
      // ignore
    }
    throw new Error(detail || `${path} → ${res.status} ${res.statusText}`);
  }
}

export const fetchSessions = () =>
  getJSON<ListSessionsResponse>('/sessions').then((r) => r.sessions);

export const fetchHosts = () =>
  getJSON<ListHostsResponse>('/hosts').then((r) => r.hosts);

/** Cordon a host: flips it to `draining` so the scheduler stops
 * assigning new sessions to it (in-flight sessions stay put). The
 * coordinator returns `204 No Content`. */
export const drainHost = (hostId: string) =>
  postJSON<void>(`/hosts/${hostId}/drain`, {});

export const fetchSession = (id: string) =>
  getJSON<Session>(`/sessions/${id}`);

// ---- ADR 0016 Phase A: COW state diagnostic --------------------------

/** Per-session COW snapshot. `state` is `null` when the session has
 * no live sandbox (Idle / HostLost / Pending / terminal). The
 * host-wide / fleet-wide COW view is served by `fetchStorageSummary`
 * below (ADR 0029) rather than a per-host fan-out from the browser. */
export const fetchSessionCowState = (sessionId: string) =>
  getJSON<SessionCowStateResponse>(`/sessions/${sessionId}/cow-state`);

// ---- ADR 0029: Storage surface ---------------------------------------

/** Fleet-wide COW/chunk rollups + the per-sandbox durability ledger. */
export const fetchStorageSummary = () =>
  getJSON<StorageSummaryResponse>('/storage/summary');

// ADR 0021 P1.5a retired `fetchHarnesses` + the `/api/harnesses`
// endpoint. The harness (if any) is an image property baked at
// bake time and surfaced as `EnabledImageSummary.harness_name`.

export interface CreateSessionInput {
  image: ImageRef;
  /** Defaults to `"agent"` server-side; pass `"dev_vm"` to leave a
   * harnessed image's agent resident-but-undriven and use the
   * session as a shell-only dev VM. */
  mode?: SessionMode;
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

// ---- Settings · Registries ---------------------------------------------

export const fetchRegistries = () =>
  getJSON<ListRegistriesResponse>('/registries').then((r) => r.registries);

export const addRegistry = (req: AddRegistryRequest) =>
  postJSON<AddRegistryResponse>('/registries', req);

export const deleteRegistry = (host: string) =>
  deleteEmpty(`/registries/${encodeURIComponent(host)}`);

// ADR 0021 P1.5a retired `fetchHarnessPacks` / `addHarnessPack` /
// `deleteHarnessPack` with the rest of the `/api/harnesses` surface.

// ---- Settings · Enabled images ----------------------------------------
//
// Image URIs contain `/` and `:` which makes them awkward as path
// segments; the coordinator takes them in the body for write paths.

export const fetchEnabledImages = () =>
  getJSON<ListEnabledImagesResponse>('/enabled-images').then(
    (r) => r.images,
  );

export const enableImage = (imageUri: string) =>
  postJSON<EnabledImageSummary>('/enabled-images', {
    image_uri: imageUri,
  });

export const disableImage = (imageUri: string) =>
  postJSON<void>('/enabled-images/disable', { image_uri: imageUri });

export const refreshEnabledImage = (imageUri: string) =>
  postJSON<EnabledImageSummary>('/enabled-images/refresh', {
    image_uri: imageUri,
  });
