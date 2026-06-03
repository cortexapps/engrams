import { expect, test } from 'vitest';
import { screen } from '@testing-library/react';
import { SidebarProvider } from '@/components/ui/sidebar';
import { renderWithProviders } from '../test-utils';
import { ThemeProvider } from './theme-provider';
import { MainSidebar } from './app-sidebar';

test('admin sees all four destinations', async () => {
  renderWithProviders(
    <ThemeProvider>
      <SidebarProvider>
        <MainSidebar />
      </SidebarProvider>
    </ThemeProvider>,
  );
  // Router defers the initial render to a microtask — await the first match.
  // Exact-string names target the destination links (not the logo link, whose
  // accessible name also contains "sessions").
  expect(await screen.findByRole('link', { name: 'Sessions' })).toBeTruthy();
  for (const label of ['Fleet', 'Storage', 'Settings']) {
    expect(screen.getByRole('link', { name: label })).toBeTruthy();
  }
});

test('member does not see Fleet or Storage', async () => {
  renderWithProviders(
    <ThemeProvider>
      <SidebarProvider>
        <MainSidebar />
      </SidebarProvider>
    </ThemeProvider>,
    { principal: {
      email: 'm@e.local', display_name: 'Mem', role: 'member',
      is_admin: false, has_claude_token: true, can_sign_out: false,
    } },
  );
  expect(await screen.findByRole('link', { name: 'Sessions' })).toBeTruthy();
  expect(screen.queryByRole('link', { name: 'Fleet' })).toBeNull();
  expect(screen.queryByRole('link', { name: 'Storage' })).toBeNull();
});
