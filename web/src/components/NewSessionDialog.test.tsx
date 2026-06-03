import { expect, test, vi, beforeEach } from 'vitest';
import { screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { renderWithProviders } from '../test-utils';
import { NewSessionDialog } from './NewSessionDialog';
import * as api from '../api';
import * as imagesHook from '../hooks/useEnabledImages';

beforeEach(() => {
  vi.restoreAllMocks();
  vi.spyOn(imagesHook, 'useEnabledImages').mockReturnValue({
    data: [{
      id: '1', image_uri: 'ghcr.io/x/api:warm', manifest_digest: 'sha256:abc',
      manifest_name: 'api', manifest_description: null, harness_name: 'claude',
      last_refreshed_at: new Date().toISOString(), created_at: new Date().toISOString(),
    }],
    isLoading: false, error: null,
  } as unknown as ReturnType<typeof imagesHook.useEnabledImages>);
});

test('creates a session and reports the new id', async () => {
  const onCreated = vi.fn();
  vi.spyOn(api, 'createSession').mockResolvedValue({
    session_id: 'sess-1', status: 'created', image_version: 'v1',
  });
  renderWithProviders(<NewSessionDialog onCreated={onCreated} />);
  // Router defers the initial render to a microtask — await the trigger.
  await userEvent.click(await screen.findByRole('button', { name: /new session/i }));
  await userEvent.click(await screen.findByRole('button', { name: /^start$/i }));
  await waitFor(() => expect(onCreated).toHaveBeenCalledWith('sess-1'));
});
