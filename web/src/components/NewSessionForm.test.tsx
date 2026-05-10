// Regression test for the NewSessionForm POST contract.
//
// Catches drift in the `POST /sessions` body shape. Stage B1 made
// `image` a flat OCI URI string (was a discriminated `{ kind, repo,
// tag }` object); Stage D wired the form to read images from
// `/api/enabled-images` instead of the legacy `/api/images`. A
// refactor that re-introduces the structured shape, or that calls
// the legacy list endpoint, would slip past Rust integration tests
// — the contract sits between the UI and the coordinator.

import { afterEach, describe, expect, test, vi } from 'vitest';
import { cleanup, screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { renderWithProviders } from '../test-utils';
import { NewSessionForm } from './NewSessionForm';

interface FetchCall {
  url: string;
  method: string;
  body?: string;
}

const ENABLED_IMAGE = {
  id: 'img-1',
  image_uri: 'ghcr.io/cortex/api:warm-1',
  manifest_digest: 'sha256:abc',
  manifest_name: 'cortex-api',
  manifest_description: 'demo image for tests',
  last_refreshed_at: new Date().toISOString(),
  created_at: new Date().toISOString(),
};

function installFetchMock() {
  const spy = vi.spyOn(globalThis, 'fetch').mockImplementation(
    async (input: RequestInfo | URL, init?: RequestInit) => {
      const url =
        typeof input === 'string'
          ? input
          : input instanceof URL
            ? input.toString()
            : (input as Request).url;
      const method = init?.method ?? 'GET';

      if (url === '/api/enabled-images' && method === 'GET') {
        return new Response(JSON.stringify({ images: [ENABLED_IMAGE] }), {
          status: 200,
          headers: { 'content-type': 'application/json' },
        });
      }
      if (url === '/api/harnesses' && method === 'GET') {
        return new Response(JSON.stringify([]), {
          status: 200,
          headers: { 'content-type': 'application/json' },
        });
      }
      if (url === '/sessions' && method === 'POST') {
        return new Response(
          JSON.stringify({
            session_id: '00000000-0000-0000-0000-000000000000',
            status: 'pending',
            image_version: 'warm-1',
          }),
          { status: 201, headers: { 'content-type': 'application/json' } },
        );
      }
      throw new Error(`unexpected fetch in test: ${method} ${url}`);
    },
  );
  return {
    spy,
    callsMatching(pred: (c: FetchCall) => boolean): FetchCall[] {
      return spy.mock.calls
        .map(([input, init]) => {
          const u =
            typeof input === 'string'
              ? input
              : input instanceof URL
                ? input.toString()
                : (input as Request).url;
          return {
            url: u,
            method: init?.method ?? 'GET',
            body: typeof init?.body === 'string' ? init?.body : undefined,
          };
        })
        .filter(pred);
    },
  };
}

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
});

describe('NewSessionForm wire contract', () => {
  test('reads images from /api/enabled-images, not /api/images', async () => {
    const mock = installFetchMock();
    renderWithProviders(
      <NewSessionForm onCancel={() => {}} onCreated={() => {}} />,
    );

    await waitFor(() => {
      const gets = mock.callsMatching((c) => c.method === 'GET');
      const urls = gets.map((c) => c.url);
      expect(urls).toContain('/api/enabled-images');
      expect(urls).not.toContain('/api/images');
    });
  });

  test('renders enabled image URIs verbatim in the dropdown', async () => {
    installFetchMock();
    renderWithProviders(
      <NewSessionForm onCancel={() => {}} onCreated={() => {}} />,
    );

    // Wait for the image list to load. The option text includes the
    // URI followed by an em-dash and the manifest name.
    const option = await screen.findByRole('option', {
      name: /ghcr\.io\/cortex\/api:warm-1.*cortex-api/,
    });
    expect(option).not.toBeNull();
  });

  test('POST /sessions sends image as a flat URI string', async () => {
    const mock = installFetchMock();
    renderWithProviders(
      <NewSessionForm onCancel={() => {}} onCreated={() => {}} />,
    );

    // Wait for the form to be ready (image loaded, default-selected).
    await screen.findByRole('option', {
      name: /ghcr\.io\/cortex\/api:warm-1.*cortex-api/,
    });

    const user = userEvent.setup();
    await user.click(screen.getByRole('button', { name: /start/i }));

    await waitFor(() => {
      const posts = mock.callsMatching(
        (c) => c.method === 'POST' && c.url === '/sessions',
      );
      expect(posts.length).toBe(1);
      const body = JSON.parse(posts[0].body!);
      // Locked: flat string, no kind/repo/tag discriminator.
      expect(body.image).toBe('ghcr.io/cortex/api:warm-1');
      expect(typeof body.image).toBe('string');
      // ADR 0005 retired the workspace axis; harness default survives.
      expect(body.workspace).toBeUndefined();
      expect(body.harness).toEqual({ kind: 'none' });
    });
  });
});
