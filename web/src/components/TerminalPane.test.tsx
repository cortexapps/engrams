// Regression tests for the SHELL-tab "stale glyphs on remount" bug.
//
// Symptom: navigate to a session, type something, leave, come back —
// the prior bash session's text is still painted, and new typed input
// visually overwrites those cells. Two root causes:
//
//   1. ghostty-web's WASM allocator hands a freshly-created Terminal
//      cell-grid memory that overlaps a previous mount's leaked grid.
//      Our hard-clear escape (\x1b[2J\x1b[3J\x1b[H) must run *after*
//      fitAddon.fit(), because fit() resizes the grid and cells added
//      by the resize aren't covered by an earlier clear.
//
//   2. ghostty-web's Terminal.dispose() does NOT call wasmTerm.free();
//      the previous Terminal's grid leaks. We free it ourselves in
//      cleanup before disposing.
//
// Both have come back from "fixed" before because nothing pinned the
// call ordering. These tests assert the order via mock invocation
// counters so a future refactor that reorders the calls fails loudly.

import { afterEach, beforeEach, describe, expect, test, vi } from 'vitest';
import { act, cleanup, render } from '@testing-library/react';
import { TerminalPane } from './TerminalPane';

// vi.hoisted runs before the vi.mock factory, so the classes are
// defined when the mock module is constructed AND accessible inside
// the test bodies via the returned references.
const { MockTerminal, MockFitAddon } = vi.hoisted(() => {
  class MockTerminal {
    static instances: MockTerminal[] = [];
    cols = 80;
    rows = 24;
    open = vi.fn();
    write = vi.fn();
    loadAddon = vi.fn();
    dispose = vi.fn();
    onData = vi.fn();
    onResize = vi.fn();
    wasmTerm = { free: vi.fn() };
    constructor() {
      MockTerminal.instances.push(this);
    }
  }
  class MockFitAddon {
    static instances: MockFitAddon[] = [];
    fit = vi.fn();
    observeResize = vi.fn();
    dispose = vi.fn();
    constructor() {
      MockFitAddon.instances.push(this);
    }
  }
  return { MockTerminal, MockFitAddon };
});

vi.mock('ghostty-web', () => ({
  init: vi.fn(async () => {}),
  Terminal: MockTerminal,
  FitAddon: MockFitAddon,
}));

class MockWebSocket {
  static instances: MockWebSocket[] = [];
  readyState = 0;
  onopen: ((ev: Event) => void) | null = null;
  onmessage: ((ev: MessageEvent) => void) | null = null;
  onerror: ((ev: Event) => void) | null = null;
  onclose: ((ev: CloseEvent) => void) | null = null;
  binaryType = 'blob';
  send = vi.fn();
  close = vi.fn();
  constructor(
    public url: string,
    public protocols?: string | string[],
  ) {
    MockWebSocket.instances.push(this);
  }
}

beforeEach(() => {
  MockTerminal.instances = [];
  MockFitAddon.instances = [];
  MockWebSocket.instances = [];
  vi.stubGlobal('WebSocket', MockWebSocket as unknown as typeof WebSocket);
});

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

async function flush() {
  // The mount effect awaits loadGhostty() (a microtask chain). One
  // act() turn drains the resolved promises and the synchronous body
  // that runs after them.
  await act(async () => {
    await Promise.resolve();
    await Promise.resolve();
  });
}

describe('TerminalPane mount sequence', () => {
  test('clear escape runs AFTER fitAddon.fit() so post-resize cells are zeroed', async () => {
    render(<TerminalPane sessionId="s1" />);
    await flush();

    const term = MockTerminal.instances.at(-1)!;
    const addon = MockFitAddon.instances.at(-1)!;

    expect(term.open).toHaveBeenCalledTimes(1);
    expect(addon.fit).toHaveBeenCalledTimes(1);
    expect(term.write).toHaveBeenCalledWith('\x1b[2J\x1b[3J\x1b[H');

    // Order: open → fit → write(clear). If a future change moves the
    // clear above fit, fit() would resize the grid afterward and the
    // cells it added would come back stale.
    const openOrder = term.open.mock.invocationCallOrder[0];
    const fitOrder = addon.fit.mock.invocationCallOrder[0];
    const clearWriteCall = term.write.mock.calls.findIndex(
      ([arg]) => arg === '\x1b[2J\x1b[3J\x1b[H',
    );
    const clearOrder = term.write.mock.invocationCallOrder[clearWriteCall];

    expect(openOrder).toBeLessThan(fitOrder);
    expect(fitOrder).toBeLessThan(clearOrder);
  });

  test('opens a WebSocket to /sessions/:id/shell with the tty subprotocol', async () => {
    render(<TerminalPane sessionId="abc-123" />);
    await flush();

    expect(MockWebSocket.instances).toHaveLength(1);
    const ws = MockWebSocket.instances[0]!;
    expect(ws.url).toContain('/sessions/abc-123/shell');
    expect(ws.protocols).toBe('tty');
  });
});

describe('TerminalPane unmount sequence', () => {
  test('frees wasmTerm BEFORE disposing the Terminal (ghostty-web dispose() leaks the grid otherwise)', async () => {
    const { unmount } = render(<TerminalPane sessionId="s1" />);
    await flush();

    const term = MockTerminal.instances.at(-1)!;
    const addon = MockFitAddon.instances.at(-1)!;
    const ws = MockWebSocket.instances.at(-1)!;

    unmount();

    // All four cleanups must fire.
    expect(ws.close).toHaveBeenCalledTimes(1);
    expect(addon.dispose).toHaveBeenCalledTimes(1);
    expect(term.wasmTerm.free).toHaveBeenCalledTimes(1);
    expect(term.dispose).toHaveBeenCalledTimes(1);

    // wasmTerm.free() before dispose() — Terminal.dispose() in
    // ghostty-web doesn't free the WASM grid, so we have to do it
    // ourselves *while we still hold a reference to wasmTerm*.
    const freeOrder = term.wasmTerm.free.mock.invocationCallOrder[0];
    const disposeOrder = term.dispose.mock.invocationCallOrder[0];
    expect(freeOrder).toBeLessThan(disposeOrder);
  });
});

describe('TerminalPane remount cycle', () => {
  test('a fresh mount allocates a new Terminal and re-runs the fit-then-clear sequence', async () => {
    const { unmount } = render(<TerminalPane sessionId="s1" />);
    await flush();

    const firstTerm = MockTerminal.instances.at(-1)!;
    expect(firstTerm.write).toHaveBeenCalledWith('\x1b[2J\x1b[3J\x1b[H');

    unmount();

    // Second mount — same sessionId, same as a SPA route nav out and
    // back. Must produce a fresh Terminal instance, not reuse the old.
    render(<TerminalPane sessionId="s1" />);
    await flush();

    const secondTerm = MockTerminal.instances.at(-1)!;
    const secondAddon = MockFitAddon.instances.at(-1)!;
    expect(secondTerm).not.toBe(firstTerm);
    expect(secondTerm.open).toHaveBeenCalledTimes(1);
    expect(secondAddon.fit).toHaveBeenCalledTimes(1);
    expect(secondTerm.write).toHaveBeenCalledWith('\x1b[2J\x1b[3J\x1b[H');

    // And the second mount's clear is still post-fit.
    const fitOrder = secondAddon.fit.mock.invocationCallOrder[0];
    const clearWriteCall = secondTerm.write.mock.calls.findIndex(
      ([arg]) => arg === '\x1b[2J\x1b[3J\x1b[H',
    );
    const clearOrder = secondTerm.write.mock.invocationCallOrder[clearWriteCall];
    expect(fitOrder).toBeLessThan(clearOrder);
  });
});
