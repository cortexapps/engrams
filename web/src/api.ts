import type {
  AddRegistryRequest,
  AddRegistryResponse,
  AdminUser,
  CreateSessionResponse,
  EnableJob,
  ImageRef,
  ListEnableJobsResponse,
  ListEnabledImagesResponse,
  ListHostsResponse,
  ListRegistriesResponse,
  ListSessionsResponse,
  Principal,
  Role,
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

// ADR 0031: human auth is purely session-based — the HttpOnly session cookie
// (OIDC mode), the IAP/forward-auth assertion (forward-auth mode), or nothing
// (dev synthetic). We send `credentials: 'include'` so the cookie rides every
// request and add NO Authorization header — the deployment bearer is for
// machine callers only. A 401 means "not authenticated"; we hard-navigate to
// the server-driven login (which 302s to the IdP, or straight back in
// forward-auth/dev). Guard against redirect loops.
let redirecting = false;
function redirectToLogin(): never {
  if (!redirecting) {
    redirecting = true;
    window.location.assign(`${API_BASE}/auth/login`);
  }
  throw new Error('unauthenticated');
}

async function getJSON<T>(path: string): Promise<T> {
  const res = await fetch(`${API_BASE}${path}`, {
    headers: { Accept: 'application/json' },
    credentials: 'include',
  });
  if (res.status === 401) redirectToLogin();
  if (!res.ok) {
    throw new Error(`${path} → ${res.status} ${res.statusText}`);
  }
  return res.json() as Promise<T>;
}

async function postJSON<T>(path: string, body: unknown): Promise<T> {
  const res = await fetch(`${API_BASE}${path}`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json', Accept: 'application/json' },
    credentials: 'include',
    body: JSON.stringify(body),
  });
  if (res.status === 401) redirectToLogin();
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
  const res = await fetch(`${API_BASE}${path}`, {
    method: 'DELETE',
    credentials: 'include',
  });
  if (res.status === 401) redirectToLogin();
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

// ---- ADR 0031: identity ------------------------------------------------

/** Thrown by fetchMe when the user is authenticated but not yet a member
 * of this deployment (coordinator returns 403). The UI shows the
 * "not a member" auth screen with the signed-in email + a sign-out link. */
export class NotMemberError extends Error {
  readonly email: string;
  constructor(email: string) {
    super('not a member');
    this.name = 'NotMemberError';
    this.email = email;
  }
}

export const fetchMe = async (): Promise<Principal> => {
  const res = await fetch(`${API_BASE}/me`, {
    headers: { Accept: 'application/json' },
    credentials: 'include',
  });
  if (res.status === 401) redirectToLogin();
  if (res.status === 403) {
    let email = '';
    try {
      const body = (await res.json()) as { email?: string };
      email = body.email ?? '';
    } catch {
      // ignore — email may not be in the body
    }
    throw new NotMemberError(email);
  }
  if (!res.ok) throw new Error(`/me → ${res.status} ${res.statusText}`);
  return res.json() as Promise<Principal>;
};

export const saveClaudeToken = (token: string) =>
  postJSON<void>('/me/claude-token', { token });

/** Revoke the session cookie, then hard-navigate home (which 401s → login). */
export const logout = async (): Promise<void> => {
  await postJSON<void>('/auth/logout', {});
  window.location.assign('/');
};

/** Admin: list users. The endpoint returns a bare array. */
export const fetchUsers = () => getJSON<AdminUser[]>('/admin/users');

// PATCH isn't covered by postJSON; do it explicitly.
export const updateUser = async (
  id: string,
  patch: { role?: Role; active?: boolean },
): Promise<AdminUser> => {
  const res = await fetch(`${API_BASE}/admin/users/${id}`, {
    method: 'PATCH',
    headers: { 'Content-Type': 'application/json', Accept: 'application/json' },
    credentials: 'include',
    body: JSON.stringify(patch),
  });
  if (res.status === 401) redirectToLogin();
  if (!res.ok) throw new Error((await res.text()) || `${res.status}`);
  return res.json() as Promise<AdminUser>;
};

/** ADR 0031: owner-scoped. Members omit `scope` (their own); admins pass
 * `'all'` for the fleet-wide view (rows carry owner identity). */
export const fetchSessions = (scope?: 'mine' | 'all') =>
  getJSON<ListSessionsResponse>(
    `/sessions${scope ? `?scope=${scope}` : ''}`,
  ).then((r) => r.sessions);

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
  // ADR 0031: the owner is the authenticated principal (server-stamped); the
  // client no longer supplies user_id.
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

// ADR 0030: operator interrupt — stop the in-flight run while keeping
// the session alive. The `run_interrupted` event flows back over the
// SSE stream; this just triggers the stop.
export const interruptSession = (sessionId: string) =>
  postJSON<{ session_id: string; note: string }>(
    `/sessions/${sessionId}/interrupt`,
    {},
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

// ADR 0036: enable/refresh are asynchronous — both return 202 with an
// EnableJob; the coordinator's scanner drives the pipeline and the
// panel polls `/enable-jobs` for real progress.
export const enableImage = (imageUri: string) =>
  postJSON<EnableJob>('/enabled-images', {
    image_uri: imageUri,
  });

export const disableImage = (imageUri: string) =>
  postJSON<void>('/enabled-images/disable', { image_uri: imageUri });

export const refreshEnabledImage = (imageUri: string) =>
  postJSON<EnableJob>('/enabled-images/refresh', {
    image_uri: imageUri,
  });

export const fetchEnableJobs = () =>
  getJSON<ListEnableJobsResponse>('/enable-jobs').then((r) => r.jobs);

export const retryEnableJob = (jobId: string) =>
  postJSON<EnableJob>(`/enable-jobs/${jobId}/retry`, {});
