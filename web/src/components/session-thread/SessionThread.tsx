import { useMemo } from 'react';
import {
  AssistantRuntimeProvider,
  useExternalStoreRuntime,
  type AppendMessage,
  type ThreadMessageLike,
} from '@assistant-ui/react';
import { Thread } from '@/components/assistant-ui/thread';
import { TooltipProvider } from '@/components/ui/tooltip';
import { sendPrompt, interruptSession } from '../../api';
import { buildMessages } from './buildMessages';
import { SessionStatusContext } from './session-status';
import type { IndexedEvent, SessionState } from '../../types';

// The transcript tab, on assistant-ui. The session's SSE event stream is the
// single source of truth: `buildMessages` reduces it to the assistant-ui
// message model and we hand that to an external-store runtime, which only
// RENDERS — it never owns the request lifecycle. Sending a prompt is
// fire-and-forget (`sendPrompt`); the echoed user turn and the assistant's
// reply arrive back over the same SSE feed and re-render through `messages`.
//
// No `onEdit`/`onReload` are wired, so assistant-ui's edit/branch tree is
// inert — a server-authoritative log doesn't branch. `onCancel` maps to the
// operator interrupt (the composer's stop control while a run is in flight).

/** Pull the plain-text body out of a composer AppendMessage. */
function appendText(message: AppendMessage): string {
  return message.content
    .filter((p): p is { type: 'text'; text: string } => p.type === 'text')
    .map((p) => p.text)
    .join('\n')
    .trim();
}

export interface SessionThreadProps {
  sessionId: string;
  events: IndexedEvent[];
  /** Drives whether the composer can send (terminal states block it). */
  status: SessionState | undefined;
}

const SEND_BLOCKED: ReadonlySet<SessionState> = new Set<SessionState>([
  'completed',
  'failed',
  'dead',
  'host_lost',
]);

export function SessionThread({ sessionId, events, status }: SessionThreadProps) {
  const { messages, isRunning } = useMemo(
    () => buildMessages(events, sessionId),
    [events, sessionId],
  );

  const runtime = useExternalStoreRuntime({
    messages,
    isRunning,
    isSendDisabled: status ? SEND_BLOCKED.has(status) : false,
    convertMessage: (m: ThreadMessageLike) => m,
    onNew: async (message) => {
      const text = appendText(message);
      if (text) await sendPrompt(sessionId, text);
    },
    onCancel: async () => {
      // The run_interrupted event arrives over SSE and closes the run. A
      // 409 (no live sandbox) is benign — the run already ended.
      try {
        await interruptSession(sessionId);
      } catch (err) {
        console.warn('interrupt failed', err);
      }
    },
  });

  return (
    <AssistantRuntimeProvider runtime={runtime}>
      <SessionStatusContext.Provider value={status}>
        <TooltipProvider>
          <Thread />
        </TooltipProvider>
      </SessionStatusContext.Provider>
    </AssistantRuntimeProvider>
  );
}
