/** The drafting-agent rail (Builder v2): the draft session's live
 * conversation beside the hand-editable Builder. The heavy lifting is the
 * product's own SessionThread (composer, question cards, streaming tail);
 * this component is chrome. Rendered only when the automation has a bound
 * draft session, so its hooks never enter the editor's own order. */

import { Sparkles } from "lucide-react";

import { SessionThread } from "@/components/session-thread/SessionThread";
import { useSessionEvents } from "@/hooks/useSessionEvents";

export function DraftRail({ sessionId }: { sessionId: string }) {
  const { events, streamingText, hasMore, loadingOlder, loadOlder, oldestIdx } =
    useSessionEvents(sessionId);
  return (
    <aside
      className="bg-card flex h-[calc(100vh-8rem)] w-[380px] shrink-0 flex-col overflow-hidden rounded-lg border"
      data-testid="draft-rail"
      aria-label="Drafting agent conversation"
    >
      <div className="text-muted-foreground flex items-center gap-2 border-b px-3 py-2 text-xs font-medium">
        <Sparkles className="text-primary size-3.5" aria-hidden />
        Drafting agent
        <span className="ml-auto font-normal">drafts save straight into this builder</span>
      </div>
      <div className="min-h-0 flex-1 overflow-y-auto px-3">
        <SessionThread
          sessionId={sessionId}
          events={events}
          status={undefined}
          streamingText={streamingText}
          transcriptWindow={{ hasMore, loadingOlder, loadOlder, oldestIdx }}
        />
      </div>
    </aside>
  );
}
