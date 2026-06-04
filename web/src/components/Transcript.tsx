import { motion } from 'framer-motion';
import { useMemo } from 'react';
import { ToolCall } from './ToolCall';
import { IdleMarker, RunBoundary } from './RunBoundary';
import { ArtifactCard } from './ArtifactCard';
import { PullRequestCard } from './PullRequestCard';
import { UserTurn } from './UserTurn';
import { Process } from './Process';
import { DurabilityMarker } from './DurabilityMarker';
import { RunSummary, type RunTally } from './RunSummary';
import { HarnessWaiting } from './HarnessWaiting';
import { Markdown } from './Markdown';
import { hms } from './transcriptFmt';
import type { AgentRole, IndexedEvent } from '../types';

// Render the session's event stream as a continuous transcript (ADR
// 0030). Layout principles:
//
//   - The human's prompt is a RIGHT-ALIGNED contained `§ you` block
//     (UserTurn); the assistant stays open prose, full-width left, with
//     its role-label in the margin. The asymmetry reads turn-taking.
//   - Assistant/system text renders as Markdown once complete; user
//     turns stay plain.
//   - Tool calls are bracketed `[ … ]` asides; shell execs are `$ …`
//     Process lines with expandable output (both previously dropped or
//     bracket-only).
//   - snapshot/resume render as faint centered ⌑ durability markers.
//   - Each run closes with a faint `↳ read N · edited N · ran N` receipt.
//   - While a run is in flight, a trailing HarnessWaiting shows the live
//     action ("running cargo check…") beside a looping engram mark, with
//     an optional ✕ stop control (operator interrupt).

interface TranscriptProps {
  events: IndexedEvent[];
  /** Owning session id — needed to build artifact serve URLs. */
  sessionId: string;
  /** Generic gerund shown between turns (before any tool/exec starts). */
  busyVerb?: string;
  /** When provided, the harness-waiting line shows a ✕ stop control. */
  onStop?: () => void;
}

type Block =
  | { kind: 'message'; key: string; role: AgentRole; texts: string[]; at: string }
  | {
      kind: 'tool';
      key: string;
      toolName: string;
      argsSummary: string | null;
      completion?: { ok: boolean; durationMs: number; resultSummary: string | null };
    }
  | {
      kind: 'exec';
      key: string;
      command: string;
      output: string;
      completion?: { exit: number | null; durationMs?: number | null };
    }
  | {
      kind: 'durability';
      key: string;
      mark: 'snapshot' | 'resumed';
      sizeBytes?: number;
      at: string;
    }
  | { kind: 'run-start'; key: string; runId: string; prompt: string | null; at: string }
  | {
      kind: 'run-end';
      key: string;
      runId: string;
      ok: boolean;
      summary: RunTally;
      endAt: string;
      interrupted?: boolean;
    }
  | { kind: 'idle'; key: string }
  | {
      kind: 'pr';
      key: string;
      url: string;
      repo: string;
      title: string;
      number: number;
      headBranch: string;
      baseBranch: string;
      at: string;
    }
  | {
      kind: 'artifact';
      key: string;
      artifactId: string;
      mediaType: string;
      sizeBytes: number;
      caption: string | null;
      at: string;
    }
  // ADR 0028 A.log: the recovery boundary — everything above it (within
  // the rolled-back span) renders greyed/collapsed; the resumed thread
  // continues below.
  | {
      kind: 'recovery';
      key: string;
      rolledBack: number;
      survivingSideEffects: string[];
      at: string;
    };

/** ADR 0028 A.log: blocks built from tombstoned events render greyed. */
type TaggedBlock = Block & { rewound?: boolean };

export function Transcript({
  events,
  sessionId,
  busyVerb = 'thinking',
  onStop,
}: TranscriptProps) {
  const blocks = useMemo(() => buildBlocks(events), [events]);

  // Mark the last assistant/system *message* so it renders with the
  // amber-settle animation on first mount.
  let lastMsgKey: string | null = null;
  for (let i = blocks.length - 1; i >= 0; i--) {
    if (blocks[i]!.kind === 'message') {
      lastMsgKey = blocks[i]!.key;
      break;
    }
  }

  // Only render the *trailing* idle marker — an "awaiting prompt" sitting
  // mid-history adds nothing (the user already responded).
  let trailingIdleKey: string | null = null;
  for (let i = blocks.length - 1; i >= 0; i--) {
    const k = blocks[i]!.kind;
    if (k === 'idle') {
      trailingIdleKey = blocks[i]!.key;
      break;
    }
    if (k === 'message' || k === 'tool' || k === 'exec' || k === 'run-start') {
      break;
    }
  }

  const busy = !trailingIdleKey && isBusy(blocks);

  if (blocks.length === 0 && !busy) {
    return (
      <p
        className="my-8 font-display italic"
        style={{ color: 'var(--color-ink-quiet)' }}
      >
        No activity yet.
      </p>
    );
  }

  return (
    <div className="space-y-1">
      {blocks.map((b, i) => {
        const el = renderBlock(b, i, blocks, {
          sessionId,
          lastMsgKey,
          trailingIdleKey,
        });
        // ADR 0028 A.log: rolled-back events stay viewable but greyed
        // (opacity + a left rule), making the recovery honest rather
        // than a silent deletion.
        if (b.rewound && el) {
          return (
            <div
              key={b.key}
              className="rewound-block"
              style={{
                opacity: 0.45,
                borderLeft: '2px solid var(--color-ink-quiet)',
                paddingLeft: '0.6rem',
              }}
              title="Rolled back by a checkpoint recovery"
            >
              {el}
            </div>
          );
        }
        return el;
      })}
      {busy && (
        <HarnessWaiting verb={contextVerb(blocks, busyVerb)} onStop={onStop} />
      )}
    </div>
  );
}

function renderBlock(
  b: TaggedBlock,
  i: number,
  blocks: TaggedBlock[],
  ctx: {
    sessionId: string;
    lastMsgKey: string | null;
    trailingIdleKey: string | null;
  },
) {
  const { sessionId, lastMsgKey, trailingIdleKey } = ctx;
  switch (b.kind) {
          case 'run-start': {
            if (b.prompt) return <UserTurn key={b.key} prompt={b.prompt} at={b.at} />;
            // Our claude harness emits run_started with a null
            // prompt_summary — the prompt is carried by the preceding
            // user message (rendered as a UserTurn). Suppress the
            // redundant boundary rule right after a user turn; otherwise
            // draw a thin rule to open the run.
            const prev = blocks[i - 1];
            if (prev && prev.kind === 'message' && prev.role === 'user') {
              return null;
            }
            return <RunBoundary key={b.key} prompt={null} />;
          }
          case 'run-end':
            return (
              <RunSummary
                key={b.key}
                summary={b.summary}
                endAt={b.endAt}
                ok={b.ok}
                interrupted={b.interrupted}
              />
            );
          case 'idle':
            if (b.key !== trailingIdleKey) return null;
            return <IdleMarker key={b.key} />;
          case 'tool':
            return (
              <ToolCall
                key={b.key}
                toolName={b.toolName}
                argsSummary={b.argsSummary}
                completion={b.completion}
              />
            );
          case 'exec':
            return (
              <Process
                key={b.key}
                command={b.command}
                output={b.output}
                completion={b.completion}
              />
            );
          case 'durability':
            return (
              <DurabilityMarker
                key={b.key}
                mark={b.mark}
                sizeBytes={b.sizeBytes}
                at={b.at}
              />
            );
          case 'message':
            return b.role === 'user' ? (
              <UserTurn key={b.key} prompt={b.texts.join('\n')} at={b.at} />
            ) : (
              <Message
                key={b.key}
                role={b.role}
                texts={b.texts}
                at={b.at}
                fresh={b.key === lastMsgKey}
              />
            );
          case 'pr':
            return (
              <PullRequestCard
                key={b.key}
                url={b.url}
                repo={b.repo}
                title={b.title}
                number={b.number}
                headBranch={b.headBranch}
                baseBranch={b.baseBranch}
                at={b.at}
              />
            );
          case 'artifact':
            return (
              <ArtifactCard
                key={b.key}
                sessionId={sessionId}
                artifactId={b.artifactId}
                mediaType={b.mediaType}
                sizeBytes={b.sizeBytes}
                caption={b.caption}
                at={b.at}
              />
            );
          case 'recovery':
            return (
              <RecoveryBoundary
                key={b.key}
                rolledBack={b.rolledBack}
                survivingSideEffects={b.survivingSideEffects}
                at={b.at}
              />
            );
  }
}

// ADR 0028 A.log: the honest recovery boundary. Everything above
// (greyed) was rolled back; the thread resumes below. Surviving
// outside-world side-effects are called out — the platform can't undo
// them.
function RecoveryBoundary({
  rolledBack,
  survivingSideEffects,
  at,
}: {
  rolledBack: number;
  survivingSideEffects: string[];
  at: string;
}) {
  return (
    <section
      className="recovery-boundary my-5 rounded border px-3 py-2 font-display"
      style={{
        borderColor: 'var(--color-amber)',
        background: 'color-mix(in srgb, var(--color-amber) 8%, transparent)',
      }}
    >
      <div className="smallcaps" style={{ color: 'var(--color-amber)' }}>
        ↩ recovered from a checkpoint after a host failure
        <span className="margin-note margin-right" data-tabular>
          {hms(at)}
        </span>
      </div>
      <div className="md" style={{ color: 'var(--color-ink-quiet)' }}>
        ~{rolledBack} {rolledBack === 1 ? 'event' : 'events'} after this point were
        rolled back; the agent resumed from here.
      </div>
      {survivingSideEffects.length > 0 && (
        <ul
          className="md mt-1"
          style={{ color: 'var(--color-ink-quiet)', listStyle: 'disc', paddingLeft: '1.2rem' }}
        >
          {survivingSideEffects.map((s, i) => (
            <li key={i}>{s}</li>
          ))}
        </ul>
      )}
    </section>
  );
}

function Message({
  role,
  texts,
  at,
  fresh,
}: {
  role: AgentRole;
  texts: string[];
  at: string;
  fresh: boolean;
}) {
  return (
    <motion.section
      layout
      initial={{ opacity: 0, y: 4 }}
      animate={{ opacity: 1, y: 0 }}
      transition={{ duration: 0.4, ease: 'easeOut' }}
      className="relative my-5"
    >
      <span className="margin-note margin-left smallcaps">
        {roleLabel(role)}.
      </span>
      <span
        className="margin-note margin-right"
        data-tabular
        style={{ marginTop: '0.45rem' }}
      >
        {hms(at)}
      </span>
      <div className={`prose md font-display ${fresh ? 'ink-settle' : ''}`}>
        <Markdown text={texts.join('\n\n')} />
      </div>
    </motion.section>
  );
}

// ---- block reduction -------------------------------------------------

function classifyTool(name: string): 'reads' | 'edits' | 'other' {
  if (/^(read|grep|glob|ls|list|search|cat|find|notebookread)/i.test(name))
    return 'reads';
  if (/^(edit|write|create|apply_?patch|multiedit|notebookedit|update)/i.test(name))
    return 'edits';
  return 'other';
}

export function buildBlocks(events: IndexedEvent[]): TaggedBlock[] {
  const out: TaggedBlock[] = [];
  const openTools = new Map<string, number>(); // tool_call_id → blocks idx
  const openExecs = new Map<string, number>(); // exec_id → blocks idx
  let activeMsg: { role: AgentRole; idx: number } | null = null;

  // Per-run tally feeding the run-summary receipt.
  let run: RunTally | null = null;
  const bump = (k: 'reads' | 'edits' | 'ran' | 'other') => {
    if (run) run[k] += 1;
  };

  for (const indexed of events) {
    const ev = indexed.event;
    const lenBefore = out.length;
    switch (ev.type) {
      case 'run_started':
        run = { reads: 0, edits: 0, ran: 0, other: 0, at: ev.at };
        out.push({
          kind: 'run-start',
          key: `rs:${indexed.idx}`,
          runId: ev.run_id,
          prompt: ev.prompt_summary,
          at: ev.at,
        });
        activeMsg = null;
        break;

      case 'run_completed':
        out.push({
          kind: 'run-end',
          key: `re:${indexed.idx}`,
          runId: ev.run_id,
          ok: ev.ok,
          summary: run ?? { reads: 0, edits: 0, ran: 0, other: 0 },
          endAt: ev.at,
        });
        run = null;
        activeMsg = null;
        break;

      case 'run_interrupted':
        out.push({
          kind: 'run-end',
          key: `re:${indexed.idx}`,
          runId: ev.run_id,
          ok: false,
          interrupted: true,
          summary: run ?? { reads: 0, edits: 0, ran: 0, other: 0 },
          endAt: ev.at,
        });
        run = null;
        activeMsg = null;
        break;

      case 'agent_message': {
        if (activeMsg && activeMsg.role === ev.role) {
          const target = out[activeMsg.idx]! as Extract<Block, { kind: 'message' }>;
          target.texts.push(ev.text);
        } else {
          out.push({
            kind: 'message',
            key: `m:${indexed.idx}`,
            role: ev.role,
            texts: [ev.text],
            at: ev.at,
          });
          activeMsg = { role: ev.role, idx: out.length - 1 };
        }
        break;
      }

      case 'tool_call_started': {
        bump(classifyTool(ev.tool_name));
        out.push({
          kind: 'tool',
          key: `t:${ev.tool_call_id}`,
          toolName: ev.tool_name,
          argsSummary: ev.args_summary,
        });
        openTools.set(ev.tool_call_id, out.length - 1);
        activeMsg = null;
        break;
      }

      case 'tool_call_completed': {
        const idx = openTools.get(ev.tool_call_id);
        if (idx != null) {
          const target = out[idx]! as Extract<Block, { kind: 'tool' }>;
          target.completion = {
            ok: ev.ok,
            durationMs: ev.duration_ms,
            resultSummary: ev.result_summary,
          };
          openTools.delete(ev.tool_call_id);
        } else {
          out.push({
            kind: 'tool',
            key: `t:${indexed.idx}`,
            toolName: ev.tool_name,
            argsSummary: null,
            completion: {
              ok: ev.ok,
              durationMs: ev.duration_ms,
              resultSummary: ev.result_summary,
            },
          });
        }
        break;
      }

      case 'exec_started':
        bump('ran');
        out.push({
          kind: 'exec',
          key: `x:${ev.exec_id}`,
          command: (ev.command ?? []).join(' '),
          output: '',
        });
        openExecs.set(ev.exec_id, out.length - 1);
        activeMsg = null;
        break;

      case 'stdout':
      case 'stderr': {
        const idx = openExecs.get(ev.exec_id);
        if (idx != null) {
          const target = out[idx]! as Extract<Block, { kind: 'exec' }>;
          target.output += ev.chunk;
        }
        break;
      }

      case 'exec_completed': {
        const idx = openExecs.get(ev.exec_id);
        if (idx != null) {
          const target = out[idx]! as Extract<Block, { kind: 'exec' }>;
          target.completion = {
            exit: ev.exit_status,
            durationMs: ev.rusage?.duration_ms,
          };
          openExecs.delete(ev.exec_id);
        }
        break;
      }

      case 'snapshot_taken':
        out.push({
          kind: 'durability',
          key: `snap:${indexed.idx}`,
          mark: 'snapshot',
          sizeBytes: ev.size_bytes,
          at: ev.at,
        });
        activeMsg = null;
        break;

      case 'resumed':
        out.push({
          kind: 'durability',
          key: `res:${indexed.idx}`,
          mark: 'resumed',
          at: ev.at,
        });
        activeMsg = null;
        break;

      case 'harness_idle':
        out.push({ kind: 'idle', key: `i:${indexed.idx}` });
        activeMsg = null;
        break;

      case 'pull_request_opened':
        out.push({
          kind: 'pr',
          key: `pr:${indexed.idx}`,
          url: ev.url,
          repo: ev.repo,
          title: ev.title,
          number: ev.number,
          headBranch: ev.head_branch,
          baseBranch: ev.base_branch,
          at: ev.at,
        });
        activeMsg = null;
        break;

      case 'file_shared':
        out.push({
          kind: 'artifact',
          key: `art:${indexed.idx}`,
          artifactId: ev.artifact_id,
          mediaType: ev.media_type,
          sizeBytes: ev.size_bytes,
          caption: ev.caption,
          at: ev.at,
        });
        activeMsg = null;
        break;

      case 'recovered_from_checkpoint':
        out.push({
          kind: 'recovery',
          key: `rec:${indexed.idx}`,
          rolledBack: ev.rolled_back,
          survivingSideEffects: ev.surviving_side_effects,
          at: ev.at,
        });
        activeMsg = null;
        break;

      default:
        // status_changed, evicted, checkpoint_* — not surfaced in the
        // transcript; the raw event sidebar shows them.
        break;
    }

    // ADR 0028 A.log: tag any block(s) this event produced as rewound
    // so the render greys/collapses the rolled-back span. (Merged
    // messages within a span are uniformly rewound, so stamping the
    // newest block is sufficient; the boundary event itself is live.)
    if (indexed.rewound && out.length > lenBefore) {
      for (let i = lenBefore; i < out.length; i++) {
        (out[i] as TaggedBlock).rewound = true;
      }
    }
  }

  return out;
}

// Is the harness mid-run? Walk back from the end, skipping the markers
// that don't imply work-in-flight (run-end, durability), until we hit a
// block that decides it.
export function isBusy(blocks: Block[]): boolean {
  for (let i = blocks.length - 1; i >= 0; i--) {
    const b = blocks[i]!;
    if (b.kind === 'run-end' || b.kind === 'durability') continue;
    if (b.kind === 'message') return b.role === 'user';
    if (b.kind === 'tool') return !b.completion;
    if (b.kind === 'exec') return !b.completion;
    if (b.kind === 'run-start') return true;
    return false;
  }
  return false;
}

// Derive the harness-waiting verb from whatever's actually in flight, so
// the indicator reads "running cargo check…" / "reading snapshot_uffd.rs…"
// rather than a generic gerund. Falls back to `generic` between turns.
export function contextVerb(blocks: Block[], generic: string): string {
  for (let i = blocks.length - 1; i >= 0; i--) {
    const b = blocks[i]!;
    if (b.kind === 'run-end' || b.kind === 'durability') continue;
    if (b.kind === 'exec' && !b.completion) {
      return `running ${b.command.split(/\s+/).slice(0, 2).join(' ')}`;
    }
    if (b.kind === 'tool' && !b.completion) {
      const file = argFile(b.argsSummary);
      if (/^(read|cat|notebookread)/i.test(b.toolName))
        return file ? `reading ${file}` : 'reading';
      if (/^(edit|write|create|apply_?patch|multiedit|notebookedit|update)/i.test(b.toolName))
        return file ? `editing ${file}` : 'editing';
      if (/^(grep|glob|ls|list|search|find)/i.test(b.toolName)) return 'searching';
      if (/^(bash|exec|shell|run)/i.test(b.toolName)) {
        const cmd = argCommand(b.argsSummary);
        return cmd ? `running ${cmd}` : 'running';
      }
      return b.toolName.replace(/_/g, ' ').toLowerCase();
    }
    break; // last meaningful block is a message / run-start → generic
  }
  return generic;
}

// The harness sends `args_summary` as the tool input JSON string. Pull a
// file basename (Read/Edit/Write) for the waiting verb, defensively.
function argFile(argsSummary: string | null): string | null {
  if (!argsSummary) return null;
  try {
    const o = JSON.parse(argsSummary) as Record<string, unknown>;
    const p = o.file_path ?? o.path ?? o.notebook_path;
    if (typeof p === 'string') return p.split('/').pop() ?? null;
  } catch {
    /* not JSON — fall through */
  }
  return null;
}

function argCommand(argsSummary: string | null): string | null {
  if (!argsSummary) return null;
  try {
    const o = JSON.parse(argsSummary) as Record<string, unknown>;
    if (typeof o.command === 'string')
      return o.command.split(/\s+/).slice(0, 2).join(' ');
  } catch {
    /* not JSON */
  }
  return null;
}

function roleLabel(role: AgentRole): string {
  return role;
}
