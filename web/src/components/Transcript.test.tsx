// Tests for the transcript v2 reducer + rendering (ADR 0030).
//
// The pure-function tests (buildBlocks / isBusy / contextVerb) carry
// the logic coverage — block reduction, run tallies, busy detection,
// the context-aware waiting verb. The render tests are smoke checks
// that each new block kind reaches the DOM with the right in-system
// markup. Busy-state rendering is covered via the pure helpers only:
// the harness-waiting line mounts <EngramMark mode="loop">, whose Web
// Animations `.animate()` isn't implemented in jsdom, so we don't mount
// it here.

import { afterEach, describe, expect, test } from 'vitest';
import { cleanup, screen, waitFor } from '@testing-library/react';
import { renderWithProviders } from '../test-utils';
import {
  Transcript,
  buildBlocks,
  isBusy,
  contextVerb,
} from './Transcript';
import type { IndexedEvent, SessionEvent } from '../types';

afterEach(cleanup);

const AT = '2026-06-02T12:00:00.000Z';
const AT2 = '2026-06-02T12:00:18.000Z';

/** Wrap a flat list of events with their monotonic idx. */
function indexed(events: SessionEvent[]): IndexedEvent[] {
  return events.map((event, idx) => ({ idx, event }));
}

describe('buildBlocks', () => {
  test('a user-role agent_message becomes a user message block', () => {
    const blocks = buildBlocks(
      indexed([
        {
          type: 'agent_message',
          run_id: '',
          message_id: 'u1',
          role: 'user',
          text: 'fix the flaky test',
          at: AT,
        },
      ]),
    );
    expect(blocks).toHaveLength(1);
    expect(blocks[0]).toMatchObject({ kind: 'message', role: 'user' });
  });

  test('exec_started + stdout + exec_completed collapse to one exec block', () => {
    const blocks = buildBlocks(
      indexed([
        { type: 'exec_started', exec_id: 'x1', command: ['cargo', 'check'], at: AT },
        { type: 'stdout', exec_id: 'x1', chunk: 'Compiling…\n' },
        { type: 'stderr', exec_id: 'x1', chunk: 'warning: unused\n' },
        {
          type: 'exec_completed',
          exec_id: 'x1',
          exit_status: 0,
          rusage: { duration_ms: 4200 },
          at: AT2,
        },
      ]),
    );
    const exec = blocks.find((b) => b.kind === 'exec');
    expect(exec).toBeTruthy();
    expect(exec).toMatchObject({
      command: 'cargo check',
      completion: { exit: 0, durationMs: 4200 },
    });
    // stdout + stderr both accumulate onto the open exec.
    expect((exec as { output: string }).output).toContain('Compiling');
    expect((exec as { output: string }).output).toContain('warning: unused');
  });

  test('snapshot_taken / resumed become durability markers', () => {
    const blocks = buildBlocks(
      indexed([
        { type: 'snapshot_taken', snapshot_id: 's', size_bytes: 1_287_000_000, at: AT },
        { type: 'resumed', snapshot_id: 's', at: AT2 },
      ]),
    );
    expect(blocks[0]).toMatchObject({ kind: 'durability', mark: 'snapshot' });
    expect(blocks[1]).toMatchObject({ kind: 'durability', mark: 'resumed' });
  });

  test('a run tallies reads / edits / ran for the receipt', () => {
    const blocks = buildBlocks(
      indexed([
        { type: 'run_started', run_id: 'r1', prompt_summary: null, at: AT },
        {
          type: 'tool_call_started',
          run_id: 'r1',
          tool_call_id: 't1',
          tool_name: 'Read',
          args_summary: '{"file_path":"a.rs"}',
          at: AT,
        },
        {
          type: 'tool_call_started',
          run_id: 'r1',
          tool_call_id: 't2',
          tool_name: 'Edit',
          args_summary: '{"file_path":"a.rs"}',
          at: AT,
        },
        { type: 'exec_started', exec_id: 'x1', command: ['cargo', 'test'], at: AT },
        { type: 'run_completed', run_id: 'r1', ok: true, at: AT2 },
      ]),
    );
    const end = blocks.find((b) => b.kind === 'run-end');
    expect(end).toMatchObject({
      ok: true,
      summary: { reads: 1, edits: 1, ran: 1, other: 0 },
    });
  });

  test('run_interrupted closes the run as interrupted', () => {
    const blocks = buildBlocks(
      indexed([
        { type: 'run_started', run_id: 'r1', prompt_summary: null, at: AT },
        { type: 'exec_started', exec_id: 'x1', command: ['cargo', 'test'], at: AT },
        { type: 'run_interrupted', run_id: 'r1', at: AT2 },
      ]),
    );
    const end = blocks.find((b) => b.kind === 'run-end');
    expect(end).toMatchObject({ ok: false, interrupted: true });
  });
});

describe('isBusy', () => {
  test('an open tool call (no completion) at the end is busy', () => {
    const blocks = buildBlocks(
      indexed([
        { type: 'run_started', run_id: 'r1', prompt_summary: null, at: AT },
        {
          type: 'tool_call_started',
          run_id: 'r1',
          tool_call_id: 't1',
          tool_name: 'Read',
          args_summary: null,
          at: AT,
        },
      ]),
    );
    expect(isBusy(blocks)).toBe(true);
  });

  test('a trailing user message (prompt sent, harness not started) is busy', () => {
    const blocks = buildBlocks(
      indexed([
        {
          type: 'agent_message',
          run_id: '',
          message_id: 'u1',
          role: 'user',
          text: 'go',
          at: AT,
        },
      ]),
    );
    expect(isBusy(blocks)).toBe(true);
  });

  test('a completed run ending in an assistant message is not busy', () => {
    const blocks = buildBlocks(
      indexed([
        { type: 'run_started', run_id: 'r1', prompt_summary: null, at: AT },
        {
          type: 'agent_message',
          run_id: 'r1',
          message_id: 'a1',
          role: 'assistant',
          text: 'done',
          at: AT,
        },
        { type: 'run_completed', run_id: 'r1', ok: true, at: AT2 },
      ]),
    );
    expect(isBusy(blocks)).toBe(false);
  });
});

describe('contextVerb', () => {
  test('an open Read tool reads the file basename from args', () => {
    const blocks = buildBlocks(
      indexed([
        {
          type: 'tool_call_started',
          run_id: 'r1',
          tool_call_id: 't1',
          tool_name: 'Read',
          args_summary: '{"file_path":"crates/x/snapshot_uffd.rs"}',
          at: AT,
        },
      ]),
    );
    expect(contextVerb(blocks, 'thinking')).toBe('reading snapshot_uffd.rs');
  });

  test('an open exec reads the command head', () => {
    const blocks = buildBlocks(
      indexed([
        { type: 'exec_started', exec_id: 'x1', command: ['cargo', 'check', '--workspace'], at: AT },
      ]),
    );
    expect(contextVerb(blocks, 'thinking')).toBe('running cargo check');
  });

  test('falls back to the generic gerund between turns', () => {
    const blocks = buildBlocks(
      indexed([
        {
          type: 'agent_message',
          run_id: 'r1',
          message_id: 'a1',
          role: 'assistant',
          text: 'thinking out loud',
          at: AT,
        },
      ]),
    );
    expect(contextVerb(blocks, 'thinking')).toBe('thinking');
  });
});

// ---- render smoke checks (non-busy states only) ----------------------

/** Render a transcript that ends idle (so the EngramMark-backed
 * harness-waiting line never mounts).
 *
 * Async because TanStack Router defers the initial render to a microtask —
 * we wait for the transcript to actually mount before the caller queries the
 * DOM (with MemoryRouter this render was synchronous). */
async function renderIdle(events: SessionEvent[]) {
  const result = renderWithProviders(
    <Transcript events={indexed([...events, { type: 'harness_idle', at: AT2 }])} sessionId="s1" />,
  );
  await waitFor(() => expect(result.container.childElementCount).toBeGreaterThan(0));
  return result;
}

describe('Transcript rendering', () => {
  test('user turn renders the § you block with the prompt text', async () => {
    const { container } = await renderIdle([
      {
        type: 'agent_message',
        run_id: '',
        message_id: 'u1',
        role: 'user',
        text: 'fix the flaky snapshot test',
        at: AT,
      },
    ]);
    const turn = container.querySelector('.user-turn');
    expect(turn).toBeTruthy();
    expect(container.querySelector('.user-turn-text')?.textContent).toBe(
      'fix the flaky snapshot test',
    );
    expect(turn?.textContent).toContain('§');
  });

  test('a process renders $ command and exit status', async () => {
    const { container } = await renderIdle([
      { type: 'exec_started', exec_id: 'x1', command: ['cargo', 'nextest', 'run'], at: AT },
      {
        type: 'exec_completed',
        exec_id: 'x1',
        exit_status: 0,
        rusage: { duration_ms: 18420 },
        at: AT2,
      },
    ]);
    expect(container.querySelector('.process-cmd')?.textContent).toBe('cargo nextest run');
    expect(container.querySelector('.process-meta')?.textContent).toContain('exit 0');
  });

  test('a snapshot renders a durability marker', async () => {
    const { container } = await renderIdle([
      { type: 'snapshot_taken', snapshot_id: 's', size_bytes: 1_287_000_000, at: AT },
    ]);
    expect(container.querySelector('.durability-label')?.textContent).toContain('snapshotted');
  });

  test('a completed run renders a receipt with the tally', async () => {
    const { container } = await renderIdle([
      { type: 'run_started', run_id: 'r1', prompt_summary: null, at: AT },
      {
        type: 'tool_call_started',
        run_id: 'r1',
        tool_call_id: 't1',
        tool_name: 'Read',
        args_summary: '{"file_path":"a.rs"}',
        at: AT,
      },
      {
        type: 'tool_call_completed',
        run_id: 'r1',
        tool_call_id: 't1',
        tool_name: 'Read',
        ok: true,
        duration_ms: 11,
        result_summary: '212 lines',
        at: AT,
      },
      { type: 'run_completed', run_id: 'r1', ok: true, at: AT2 },
    ]);
    expect(container.querySelector('.run-summary-text')?.textContent).toContain('read 1');
  });

  test('an interrupted run renders an "interrupted" receipt', async () => {
    const { container } = await renderIdle([
      { type: 'run_started', run_id: 'r1', prompt_summary: null, at: AT },
      { type: 'exec_started', exec_id: 'x1', command: ['cargo', 'test'], at: AT },
      { type: 'run_interrupted', run_id: 'r1', at: AT2 },
    ]);
    expect(container.querySelector('.run-summary-text')?.textContent).toContain(
      'interrupted',
    );
  });

  test('assistant message renders Markdown in-system', async () => {
    const { container } = await renderIdle([
      {
        type: 'agent_message',
        run_id: 'r1',
        message_id: 'a1',
        role: 'assistant',
        text: '# Heading\n\nuse `cargo check` to verify',
        at: AT,
      },
    ]);
    expect(container.querySelector('h1.md-h')?.textContent).toBe('Heading');
    expect(container.querySelector('code.md-code')?.textContent).toBe('cargo check');
  });

  test('raw HTML in assistant Markdown is inert (no injection)', async () => {
    const { container } = await renderIdle([
      {
        type: 'agent_message',
        run_id: 'r1',
        message_id: 'a1',
        role: 'assistant',
        text: 'before <img src=x onerror=alert(1)> after',
        at: AT,
      },
    ]);
    // No raw element is created; the tag is treated as literal text.
    expect(container.querySelector('img')).toBeNull();
    expect(screen.getByText(/before/).textContent).toContain('after');
  });
});
