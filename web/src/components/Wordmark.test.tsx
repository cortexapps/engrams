// Regression test for the global Wordmark.
//
// What this catches: a refactor that drops Wordmark from App.tsx,
// or changes its target away from `/`, or accidentally hides it
// (e.g. via a CSS rule that gets too aggressive). The mark is the
// only "back to overview" affordance on inner pages — losing it is
// a silent navigation regression.
//
// The component is small but the contract is load-bearing: every
// page must show a clickable wordmark in the top-left that
// navigates home.

import { afterEach, describe, expect, test } from 'vitest';
import { cleanup, screen } from '@testing-library/react';
import { renderWithProviders } from '../test-utils';
import { Wordmark } from './Wordmark';

afterEach(() => {
  cleanup();
});

describe('Wordmark', () => {
  test('renders an "engrams" link pointing at /', () => {
    renderWithProviders(<Wordmark />);
    const link = screen.getByRole('link', { name: /back to overview/i });
    expect(link.textContent).toBe('engrams');
    // react-router's Link renders an <a href> in the DOM. Asserting
    // on the href is what the user (and their browser's address
    // bar) actually depends on — not on which Link component shipped.
    expect(link.getAttribute('href')).toBe('/');
  });
});
