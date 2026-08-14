import { Link } from "@tanstack/react-router";
import { ExternalLink } from "lucide-react";

import { SessionThread } from "@/components/session-thread/SessionThread";
import { StatusGlyph } from "@/components/Glyph";
import { Button } from "@/components/ui/button";
import { useSession } from "@/hooks/useSessions";
import { useSessionEvents } from "@/hooks/useSessionEvents";
import { statusLabel } from "@/pages/sessions/session-format";

/**
 * The owner's conversation with the drafting agent, beside the canvas.
 *
 * The brief's screen anatomy names three regions — chat rail, canvas, section
 * rail — and this is the first: the same session that `createSpec` booted with
 * the problem statement, embedded whole through `SessionThread` (the
 * ReviewTranscriptPane precedent). The spec read route already returns
 * `sessionId` only to the owner, so mounting on a non-null id is the R60 rule
 * — collaborators see the doc, not this chat.
 *
 * Keyed on the session id by the caller: `SessionThread`'s optimistic
 * pending-prompt array is shared across instances for the life of a page load.
 */
export function SpecChatRail({ sessionId }: { sessionId: string }) {
  const { events, streamingText, hasMore, loadingOlder, loadOlder, oldestIdx } =
    useSessionEvents(sessionId);
  const { data: session } = useSession(sessionId);

  return (
    <aside className="spec-chat-shell" aria-label="Drafting session">
      <header className="spec-chat-header">
        <span className="spec-chat-status">
          {session ? (
            <>
              <StatusGlyph status={session.status} />
              {statusLabel(session.status)}
            </>
          ) : (
            "Drafting session"
          )}
        </span>
        <Button variant="ghost" size="sm" asChild>
          <Link to="/sessions/$id" params={{ id: sessionId }}>
            Full session
            <ExternalLink className="size-3" aria-hidden />
          </Link>
        </Button>
      </header>
      <div className="spec-chat-thread">
        <SessionThread
          sessionId={sessionId}
          events={events}
          status={session?.status}
          streamingText={streamingText}
          transcriptWindow={{ hasMore, loadingOlder, loadOlder, oldestIdx }}
        />
      </div>
    </aside>
  );
}
