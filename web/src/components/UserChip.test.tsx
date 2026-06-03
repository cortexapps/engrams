// Regression tests for UserChip's dismiss affordances.
//
// What this catches: silent removal of the popover-dismiss
// behaviors users rely on. The chip is a click-to-open menu — if
// the Escape key or the outside-click handler stops working, the
// only way to dismiss the popover is to click the chip again.
// That's the kind of regression that ships, gets reported in
// support, and only then noticed.
//
// We deliberately don't assert on visual styling, drop-shadow,
// animation timings, or color values — those are caught by humans
// and screenshot tests, not unit tests.

import { afterEach, describe, expect, test } from 'vitest';
import { cleanup, screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { renderWithProviders } from '../test-utils';
import { UserChip } from './UserChip';

afterEach(() => {
  cleanup();
});

// TanStack Router defers the initial render to a microtask, so the chip
// trigger isn't in the DOM synchronously after render — findBy* polls until
// it mounts. Every dismiss test starts by opening the menu, so they share this.
function openMenuTrigger() {
  return screen.findByRole('button', { name: /open user menu/i });
}

describe('UserChip dismiss behaviors', () => {
  test('clicking the chip toggles the popover open', async () => {
    const user = userEvent.setup();
    renderWithProviders(<UserChip />);

    // Closed state: the menu items aren't in the DOM at all
    // (AnimatePresence unmounts the popover entirely on close).
    expect(screen.queryByRole('menu')).toBeNull();

    await user.click(await openMenuTrigger());
    expect(screen.queryByRole('menu')).not.toBeNull();
    // Settings link is reachable.
    expect(screen.queryByRole('link', { name: /settings/i })).not.toBeNull();
  });

  test('Escape key dismisses the popover', async () => {
    const user = userEvent.setup();
    renderWithProviders(<UserChip />);

    await user.click(await openMenuTrigger());
    expect(screen.queryByRole('menu')).not.toBeNull();

    await user.keyboard('{Escape}');

    // AnimatePresence runs the exit animation; waitFor polls until
    // the popover unmounts so the test isn't racing the animation.
    await waitFor(() => {
      expect(screen.queryByRole('menu')).toBeNull();
    });
  });

  test('clicking outside the chip dismisses the popover', async () => {
    const user = userEvent.setup();
    renderWithProviders(
      <div>
        <UserChip />
        {/* A target outside the chip's ref tree — clicks here must
            close the popover. */}
        <div data-testid="outside" style={{ height: 200 }}>
          page content
        </div>
      </div>,
    );

    await user.click(await openMenuTrigger());
    expect(screen.queryByRole('menu')).not.toBeNull();

    await user.click(screen.getByTestId('outside'));

    await waitFor(() => {
      expect(screen.queryByRole('menu')).toBeNull();
    });
  });

  test('clicking the Settings link closes the popover', async () => {
    // The link's onClick fires before navigation, so the popover
    // closes during the route transition. Without this, returning
    // to a page that mounts UserChip leaves the menu inexplicably
    // open.
    const user = userEvent.setup();
    renderWithProviders(<UserChip />);

    await user.click(await openMenuTrigger());
    const settingsLink = screen.getByRole('link', { name: /settings/i });
    await user.click(settingsLink);

    // The link's onClick fires `setOpen(false)`. AnimatePresence's
    // exit animation runs through React's commit cycle, so we wait
    // for the unmount instead of asserting synchronously.
    await waitFor(() => {
      expect(screen.queryByRole('menu')).toBeNull();
    });
  });
});
