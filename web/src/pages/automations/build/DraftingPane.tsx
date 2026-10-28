/** The drafting agent's conversation, beside the canvas.
 *
 * A drafted automation keeps the session that drafts it. The pane shows that
 * session's thread with its composer, so a follow-up ("make it per thread",
 * "add a close on resolved") goes to the agent that holds the context, and
 * the canvas re-renders as it saves. The thread is the session page's, whole:
 * it degrades to read-only when the session ends.
 *
 * `useDrafting` owns the one event stream and derives whether the agent is
 * working, so the shell can say so on the canvas while the pane is closed. */

import { Link } from "@tanstack/react-router";
import { ExternalLink, PanelLeftClose, Sparkles } from "lucide-react";
import { useMemo } from "react";

import { buildMessages } from "@/components/session-thread/buildMessages";
import { SessionThread } from "@/components/session-thread/SessionThread";
import type { TranscriptWindow } from "@/components/session-thread/transcript-window";
import { Button } from "@/components/ui/button";
import { useSessionEvents } from "@/hooks/useSessionEvents";
import { useSession } from "@/hooks/useSessions";
import type { IndexedEvent, SessionState } from "@/lib/types";

export interface DraftingState {
  sessionId: string;
  events: IndexedEvent[];
  status: SessionState | undefined;
  streamingText: string;
  transcriptWindow: TranscriptWindow;
  /** A turn is in flight: the agent is reading, thinking or saving. */
  working: boolean;
}

/** The drafting session's live state, or undefined when the automation has
 * no drafting session. */
export function useDrafting(sessionId: string | undefined): DraftingState | undefined {
  const { events, streamingText, hasMore, loadingOlder, loadOlder, oldestIdx, opening } =
    useSessionEvents(sessionId);
  const session = useSession(sessionId);
  const status = session.data?.status;
  const working = useMemo(
    () =>
      sessionId !== undefined && buildMessages(events, sessionId, status, streamingText).isRunning,
    [events, sessionId, status, streamingText],
  );
  if (sessionId === undefined) return undefined;
  return {
    sessionId,
    events,
    status,
    streamingText,
    transcriptWindow: { hasMore, loadingOlder, loadOlder, oldestIdx, opening },
    working,
  };
}

export function DraftingPane({
  drafting,
  onClose,
}: {
  drafting: DraftingState;
  onClose: () => void;
}) {
  return (
    // The inspector's mirror: a sheet on the card ground with a hairline
    // against the canvas, so the conversation reads as a panel beside the
    // diagram and not as part of it.
    <aside
      className="flex h-full min-h-0 flex-col border-r bg-card"
      aria-label="Drafting agent"
      data-testid="drafting-pane"
    >
      <header className="flex shrink-0 items-center gap-2 border-b px-3 py-1.5 text-xs font-medium">
        <Sparkles className="size-3.5 text-muted-foreground" aria-hidden />
        Drafting agent
        <span className="text-muted-foreground" data-testid="drafting-state">
          {drafting.working ? "· working" : ""}
        </span>
        <div className="ml-auto flex items-center gap-1">
          <Button variant="ghost" size="sm" className="h-7" asChild>
            <Link to="/sessions/$id" params={{ id: drafting.sessionId }}>
              Full session
              <ExternalLink className="size-3" aria-hidden />
            </Link>
          </Button>
          <Button
            variant="ghost"
            size="icon"
            className="size-7 text-muted-foreground hover:text-foreground"
            onClick={onClose}
            aria-label="Hide the drafting agent"
            title="Hide the drafting agent"
          >
            <PanelLeftClose className="size-4" />
          </Button>
        </div>
      </header>
      <div className="min-h-0 flex-1 overflow-hidden">
        <SessionThread
          key={drafting.sessionId}
          sessionId={drafting.sessionId}
          events={drafting.events}
          status={drafting.status}
          streamingText={drafting.streamingText}
          transcriptWindow={drafting.transcriptWindow}
        />
      </div>
    </aside>
  );
}
