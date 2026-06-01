import { motion } from 'framer-motion';
import { useMemo } from 'react';
import { ToolCall } from './ToolCall';
import { IdleMarker, RunBoundary } from './RunBoundary';
import { ArtifactCard } from './ArtifactCard';
import { PullRequestCard } from './PullRequestCard';
import type { AgentRole, IndexedEvent } from '../types';

// Render the session's event stream as a continuous transcript. Layout
// principles, repeated:
//
//   - Role labels live in the LEFT MARGIN, not above each message.
//     Adjacent same-role messages collapse — no repeated label.
//   - Tool calls are bracketed asides between paragraphs, with a faint
//     left rule. They are *not* messages; they don't get a role.
//   - Time stamps live in the right margin, faded.
//   - The most-recent paragraph briefly tints amber (".ink-settle"),
//     then fades to ink. It's the only place amber is used here.
//   - A pull request opened via the forge seam renders as a framed
//     notice (PullRequestCard) — the session's reviewable artifact.
//
// Any event the transcript doesn't recognise (stdout, exec_started,
// snapshot, …) is dropped. Those still appear in the raw event log
// sidebar on the detail page.

interface TranscriptProps {
  events: IndexedEvent[];
  /** Owning session id — needed to build artifact serve URLs. */
  sessionId: string;
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
  | { kind: 'run-start'; key: string; runId: string; prompt: string | null }
  | { kind: 'run-end'; key: string; runId: string; ok: boolean }
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
    };

export function Transcript({ events, sessionId }: TranscriptProps) {
  const blocks = useMemo(() => buildBlocks(events), [events]);

  if (blocks.length === 0) {
    return (
      <p
        className="my-8 font-display italic"
        style={{ color: 'var(--color-ink-quiet)' }}
      >
        Waiting for the agent to speak…
      </p>
    );
  }

  // Mark the last *message* block so it can render with the amber-settle
  // animation on first mount.
  let lastMsgKey: string | null = null;
  for (let i = blocks.length - 1; i >= 0; i--) {
    if (blocks[i]!.kind === 'message') {
      lastMsgKey = blocks[i]!.key;
      break;
    }
  }

  // Only render the *trailing* idle marker. An "awaiting prompt" sitting
  // mid-history adds nothing — the user has already responded, so the
  // event log keeps it for forensics but the transcript doesn't show it.
  // The trailing idle is the last idle block with no later message /
  // tool / run-start (run-end blocks render as null and don't count).
  let trailingIdleKey: string | null = null;
  for (let i = blocks.length - 1; i >= 0; i--) {
    const k = blocks[i]!.kind;
    if (k === 'idle') {
      trailingIdleKey = blocks[i]!.key;
      break;
    }
    if (k === 'message' || k === 'tool' || k === 'run-start') {
      break;
    }
  }

  return (
    <div className="space-y-1">
      {blocks.map((b) => {
        switch (b.kind) {
          case 'run-start':
            return <RunBoundary key={b.key} prompt={b.prompt} />;
          case 'run-end':
            // Soft separator — the next RunBoundary will draw the rule.
            return null;
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
          case 'message':
            return (
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
        }
      })}
    </div>
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

      <div
        className={`font-display ${fresh ? 'ink-settle' : ''}`}
        style={{
          fontSize: '1.04rem',
          lineHeight: 1.62,
          color: 'var(--color-ink)',
          whiteSpace: 'pre-wrap',
        }}
      >
        {texts.map((t, i) => (
          <p key={i} className={i === 0 ? '' : 'mt-3'}>
            {t}
          </p>
        ))}
      </div>
    </motion.section>
  );
}

function buildBlocks(events: IndexedEvent[]): Block[] {
  const out: Block[] = [];
  // Track open tool calls so a `tool_call_completed` can attach back to
  // the matching `tool_call_started` in place.
  const openTools = new Map<string, number>(); // tool_call_id → blocks idx

  // Coalesce adjacent agent messages with the same role into a single
  // <Message> with multiple paragraphs. Only adjacent-in-stream;
  // anything else (a tool call, a run boundary) breaks the run.
  let activeMsg: { role: AgentRole; idx: number } | null = null;

  for (const indexed of events) {
    const ev = indexed.event;
    switch (ev.type) {
      case 'run_started':
        out.push({
          kind: 'run-start',
          key: `rs:${indexed.idx}`,
          runId: ev.run_id,
          prompt: ev.prompt_summary,
        });
        activeMsg = null;
        break;

      case 'run_completed':
        out.push({
          kind: 'run-end',
          key: `re:${indexed.idx}`,
          runId: ev.run_id,
          ok: ev.ok,
        });
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
          // Completion without a matching start (replay window edge):
          // synthesize a standalone block so it still renders.
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

      default:
        // status_changed, snapshot_taken, evicted, resumed, exec_*,
        // checkpoint_* — dropped from the transcript view; the raw
        // sidebar can show them.
        break;
    }
  }

  return out;
}

function roleLabel(role: AgentRole): string {
  switch (role) {
    case 'assistant':
      return 'assistant';
    case 'user':
      return 'user';
    case 'system':
      return 'system';
  }
}

function hms(iso: string): string {
  return new Date(iso).toLocaleTimeString('en-GB', {
    hour: '2-digit',
    minute: '2-digit',
    second: '2-digit',
  });
}
