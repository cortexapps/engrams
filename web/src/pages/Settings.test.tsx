// Regression test for the Settings page's breadcrumb-H1 home
// affordance.
//
// What this catches: a refactor that drops the "engrams" link from
// the H1, changes its target away from `/`, or hides it behind a
// styling change. The breadcrumb is the only "back to overview"
// surface on inner pages — losing it is a silent navigation
// regression that users wouldn't immediately notice (the page
// still renders) but would feel as "I can't get back from here".

import { afterEach, describe, expect, test } from 'vitest';
import { cleanup, screen } from '@testing-library/react';
import { renderWithProviders } from '../test-utils';
import { Settings } from './Settings';
import { Routes, Route } from 'react-router-dom';

/** Mount the Settings layout under its own route so useLocation /
 * NavLink resolve correctly. The outlet is intentionally empty —
 * we only care about the chrome the layout renders, not which
 * panel is active. */
function renderSettingsLayout(initialPath: string = '/settings/registries') {
  return renderWithProviders(
    <Routes>
      <Route path="/settings" element={<Settings />}>
        {/* Empty Outlet content — these tests don't exercise the
            tab panels, just the layout's H1 + breadcrumb. */}
        <Route path="registries" element={<></>} />
        <Route path="harnesses" element={<></>} />
        <Route path="profile" element={<></>} />
      </Route>
    </Routes>,
    { initialEntries: [initialPath] },
  );
}

afterEach(() => {
  cleanup();
});

describe('Settings breadcrumb-H1', () => {
  test('renders an "engrams" link pointing at /', () => {
    renderSettingsLayout();
    const link = screen.getByRole('link', { name: /back to sessions/i });
    expect(link.textContent).toBe('engrams');
    expect(link.getAttribute('href')).toBe('/');
  });

  test('renders the active-page name "settings" alongside the link', () => {
    // The H1 must carry both halves of the breadcrumb. We assert
    // on the H1's full text rather than its DOM structure so the
    // test survives type-treatment refactors (e.g. swapping
    // <span> wrappers).
    renderSettingsLayout();
    const heading = screen.getByRole('heading', { level: 1 });
    expect(heading.textContent).toContain('engrams');
    expect(heading.textContent).toContain('settings');
  });
});
