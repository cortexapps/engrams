import { expect, test } from 'vitest';
import { screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { SidebarProvider } from '@/components/ui/sidebar';
import { renderWithProviders } from '../test-utils';
import { UserMenu } from './user-menu';

test('shows the signed-in email and a settings link when opened', async () => {
  renderWithProviders(
    <SidebarProvider>
      <UserMenu />
    </SidebarProvider>,
  );
  await userEvent.click(await screen.findByRole('button', { name: /local admin/i }));
  // The email renders in both the trigger button and the open dropdown label.
  expect((await screen.findAllByText('dev@engram.local')).length).toBeGreaterThan(0);
  expect(screen.getByRole('menuitem', { name: /settings/i })).toBeTruthy();
});
