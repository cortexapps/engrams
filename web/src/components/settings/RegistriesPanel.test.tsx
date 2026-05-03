// Regression tests for the AddRegistryForm payload contract.
//
// What this catches: silent drift in the JSON shape we POST to
// /api/registries. The Rust HTTP smoke test
// (crates/engram-coordinator/tests/registry_smoke.rs) validates that
// the *server* accepts a given request, but it doesn't catch a UI
// refactor that ships e.g. `password_hash` instead of `password`,
// or `auth_kind` at the top level instead of nested under `auth`,
// or that submits the impersonate_sa field with an empty string
// instead of omitting it. The contract sits between the two
// services; both ends need a regression pin.
//
// This file is *not* a substitute for end-to-end tests; it
// deliberately mocks fetch so we can assert on the call arguments
// without booting the coordinator.

import { afterEach, describe, expect, test, vi } from 'vitest';
import { cleanup, screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { renderWithProviders } from '../../test-utils';
import { RegistriesPanel } from './RegistriesPanel';

// Helper: install a route-aware fetch stub.
//   GET /api/registries   → empty list (so the panel renders without rows)
//   POST /api/registries  → success, captured for assertions
//   anything else         → 500 (test fails loudly)
//
// Returns the spy on `fetch` so tests can read the captured POST
// body and headers.
function installFetchMock(): ReturnType<typeof vi.spyOn> {
  return vi.spyOn(globalThis, 'fetch').mockImplementation(
    async (input: RequestInfo | URL, init?: RequestInit) => {
      const url =
        typeof input === 'string'
          ? input
          : input instanceof URL
            ? input.toString()
            : (input as Request).url;
      const method = init?.method ?? 'GET';
    if (url === '/api/registries' && method === 'GET') {
      return new Response(JSON.stringify({ registries: [] }), {
        status: 200,
        headers: { 'content-type': 'application/json' },
      });
    }
    if (url === '/api/registries' && method === 'POST') {
      return new Response(
        JSON.stringify({
          id: '00000000-0000-0000-0000-000000000000',
          host: 'gcr.io',
          auth_kind: 'static',
          auth_principal: '_json_key',
        }),
        { status: 201, headers: { 'content-type': 'application/json' } },
      );
    }
      throw new Error(`unexpected fetch in test: ${method} ${url}`);
    },
  );
}

/** Open the inline AddRegistryForm. The panel renders the trigger
 * once the empty-list query has resolved, so we wait for it. */
async function openAddForm() {
  const user = userEvent.setup();
  const trigger = await screen.findByRole('button', {
    name: /register a new registry/i,
  });
  await user.click(trigger);
  return user;
}

/** Pull the body off the most recent POST captured by `fetch`. */
function lastPostBody(spy: ReturnType<typeof vi.spyOn>): unknown {
  const posts = spy.mock.calls.filter(
    ([, init]: [unknown, RequestInit | undefined]) => init?.method === 'POST',
  );
  expect(posts.length, 'expected at least one POST').toBeGreaterThan(0);
  const body = (posts.at(-1)![1] as RequestInit).body;
  expect(typeof body).toBe('string');
  return JSON.parse(body as string);
}

describe('AddRegistryForm payload contract', () => {
  afterEach(() => {
    cleanup();
    vi.restoreAllMocks();
  });

  test('static auth: posts {host, auth: {kind: "static", username, password}}', async () => {
    const fetchSpy = installFetchMock();
    renderWithProviders(<RegistriesPanel />);

    const user = await openAddForm();

    // Static is the default auth kind, so we just fill in the three
    // visible fields and submit.
    await user.type(screen.getByPlaceholderText('ghcr.io'), 'gcr.io');
    await user.type(
      screen.getByPlaceholderText('username or _json_key'),
      '_json_key',
    );
    await user.type(screen.getByPlaceholderText('•••••'), 'hunter2');

    await user.click(screen.getByRole('button', { name: /^register$/i }));

    await waitFor(() => {
      expect(lastPostBody(fetchSpy)).toEqual({
        host: 'gcr.io',
        auth: {
          kind: 'static',
          username: '_json_key',
          password: 'hunter2',
        },
      });
    });
  });

  test('gcp workload identity (ambient): posts {auth: {kind: "gcp_workload_identity"}} with no impersonate field', async () => {
    const fetchSpy = installFetchMock();
    renderWithProviders(<RegistriesPanel />);

    const user = await openAddForm();
    await user.type(
      screen.getByPlaceholderText('ghcr.io'),
      'us-east1-docker.pkg.dev',
    );

    // Click the GCP WI radio card. We match on the visible label
    // text so the test survives DOM-structure refactors.
    await user.click(
      screen.getByRole('radio', { name: /gcp workload identity/i }),
    );
    // Wait for the GCP-WI form fragment to mount (AnimatePresence
    // mode="wait" waits for the static fragment's exit animation
    // first). findBy* polls until present.
    await screen.findByPlaceholderText(/engram@my-project/);
    // Leave impersonate empty — ambient identity path.

    await user.click(screen.getByRole('button', { name: /^register$/i }));

    await waitFor(() => {
      // The shape under `auth` must be exactly `{kind: ...}` — NO
      // impersonate_sa key (not even with an empty string), because
      // the server's serde decoder treats missing as "ambient" and
      // an empty string as a malformed input.
      expect(lastPostBody(fetchSpy)).toEqual({
        host: 'us-east1-docker.pkg.dev',
        auth: { kind: 'gcp_workload_identity' },
      });
    });
  });

  test('gcp workload identity with impersonation: posts impersonate_sa verbatim', async () => {
    const fetchSpy = installFetchMock();
    renderWithProviders(<RegistriesPanel />);

    const user = await openAddForm();
    await user.type(
      screen.getByPlaceholderText('ghcr.io'),
      'us-east1-docker.pkg.dev',
    );
    await user.click(
      screen.getByRole('radio', { name: /gcp workload identity/i }),
    );
    // The impersonate field belongs to the GCP-WI form fragment
    // that AnimatePresence mounts after the static fragment exits.
    // Use findBy* so the test waits for that transition instead of
    // racing it.
    const impersonate = await screen.findByPlaceholderText(/engram@my-project/);
    await user.type(impersonate, 'engram@cortex.iam.gserviceaccount.com');

    await user.click(screen.getByRole('button', { name: /^register$/i }));

    await waitFor(() => {
      expect(lastPostBody(fetchSpy)).toEqual({
        host: 'us-east1-docker.pkg.dev',
        auth: {
          kind: 'gcp_workload_identity',
          impersonate_sa: 'engram@cortex.iam.gserviceaccount.com',
        },
      });
    });
  });

  test('static missing username: rejects locally, never POSTs', async () => {
    // The form must validate before fetch — surfacing inline errors
    // is friendlier than waiting for the server's 400 + a generic
    // "Bad request" toast.
    const fetchSpy = installFetchMock();
    renderWithProviders(<RegistriesPanel />);

    const user = await openAddForm();
    await user.type(screen.getByPlaceholderText('ghcr.io'), 'gcr.io');
    await user.type(screen.getByPlaceholderText('•••••'), 'hunter2');
    // username deliberately blank.

    await user.click(screen.getByRole('button', { name: /^register$/i }));

    // Inline error rendered.
    await screen.findByText(/username is required/i);
    // No POST fired.
    const posts = fetchSpy.mock.calls.filter(
      ([, init]: [unknown, RequestInit | undefined]) => init?.method === 'POST',
    );
    expect(posts).toHaveLength(0);
  });

  test('static missing password: rejects locally, never POSTs', async () => {
    const fetchSpy = installFetchMock();
    renderWithProviders(<RegistriesPanel />);

    const user = await openAddForm();
    await user.type(screen.getByPlaceholderText('ghcr.io'), 'gcr.io');
    await user.type(
      screen.getByPlaceholderText('username or _json_key'),
      '_json_key',
    );
    // password blank.

    await user.click(screen.getByRole('button', { name: /^register$/i }));

    await screen.findByText(/password is required/i);
    const posts = fetchSpy.mock.calls.filter(
      ([, init]: [unknown, RequestInit | undefined]) => init?.method === 'POST',
    );
    expect(posts).toHaveLength(0);
  });

  test('aws instance role card is rendered but not selectable', async () => {
    // The card sits in the auth-model grid as a "coming soon" stub.
    // We render the row so users see the road map; we *must not*
    // let them select it (clicking would set authKind to a value
    // the server's CHECK constraint rejects).
    installFetchMock();
    renderWithProviders(<RegistriesPanel />);
    const user = await openAddForm();

    const card = screen.getByRole('radio', { name: /aws instance role/i });
    expect((card as HTMLButtonElement).disabled).toBe(true);

    // Clicking does nothing — authKind stays on `static`, password
    // field remains visible.
    await user.click(card);
    expect(
      screen.queryByPlaceholderText('username or _json_key'),
    ).not.toBeNull();
  });
});
