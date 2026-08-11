import { Link } from "@tanstack/react-router";
import { ExternalLink, PanelRightClose } from "lucide-react";

import type { Review } from "../../gen/engram/app/v1/review_pb";
import { useSession } from "../../hooks/useSessions";
import { useSessionEvents } from "../../hooks/useSessionEvents";
import { SessionThread } from "../../components/session-thread/SessionThread";
import { StatusGlyph } from "../../components/Glyph";
import { statusLabel } from "../sessions/session-format";
import { Button } from "@/components/ui/button";
import { Tabs, TabsList, TabsTrigger } from "@/components/ui/tabs";

export type WorkerRole = "finder" | "verifier";

export const ROLE_LABEL: Record<WorkerRole, string> = {
  finder: "Finder",
  verifier: "Verifier",
};

/** The session id a role ran under, stamped at kickoff and outliving the session
 *  itself — so a finished pass still has a transcript to open. */
export function roleSession(review: Review, role: WorkerRole): string | undefined {
  return role === "finder" ? review.finderSessionId : review.verifierSessionId;
}

/**
 * The finder or verifier session's thread, in a sheet over the dossier.
 *
 * Reuses the session transcript wholesale: `SessionThread` is a pure props
 * component and already degrades to read-only on a terminal session (the
 * composer disables itself and the running state clears), which is exactly the
 * behaviour a torn-down reviewer session wants. The VM is destroyed at phase
 * end, so the transcript is the only pane that can exist — a shell or browser
 * tab would be dead by construction.
 *
 * One role is mounted at a time: each open transcript holds an SSE connection,
 * and a browser allows only a handful per origin.
 */
export function ReviewTranscriptPane({
  review,
  role,
  onChangeRole,
  onClose,
}: {
  review: Review;
  role: WorkerRole;
  onChangeRole: (role: WorkerRole) => void;
  onClose: () => void;
}) {
  const sessionId = roleSession(review, role);
  const roles: WorkerRole[] = (["finder", "verifier"] as const).filter((r) =>
    Boolean(roleSession(review, r)),
  );

  return (
    <div className="flex h-full min-h-0 flex-col">
      <header className="flex shrink-0 items-center gap-1 border-b px-2 py-1.5">
        {/* Stock shadcn Tabs. Two roles, so the segmented pill is the right
            control — and it is the same tab component every other surface in
            the app now uses. */}
        <Tabs value={role} onValueChange={(v) => onChangeRole(v as WorkerRole)}>
          <TabsList aria-label="Worker session" className="h-8">
            {roles.map((r) => (
              <TabsTrigger key={r} value={r} className="px-3 text-xs">
                {ROLE_LABEL[r]}
              </TabsTrigger>
            ))}
          </TabsList>
        </Tabs>

        <div className="ml-auto flex items-center gap-1">
          {sessionId && (
            <Button variant="ghost" size="sm" asChild>
              <Link to="/sessions/$id" params={{ id: sessionId }}>
                Full session
                <ExternalLink className="size-3" aria-hidden />
              </Link>
            </Button>
          )}
          <Button
            variant="ghost"
            size="icon"
            className="size-7 text-muted-foreground hover:text-foreground"
            onClick={onClose}
            aria-label="Close transcript"
            title="Close transcript"
          >
            <PanelRightClose className="size-4" />
          </Button>
        </div>
      </header>

      {sessionId ? (
        <RoleTranscript key={sessionId} sessionId={sessionId} />
      ) : (
        <p className="p-4 text-sm text-muted-foreground">
          This phase never started, so there is no transcript to show.
        </p>
      )}
    </div>
  );
}

/**
 * Keyed on the session id by the caller, because `SessionThread`'s optimistic
 * pending-prompt array is shared across instances for the life of a page load.
 * Nothing is ever sent from here, but a fresh instance per session keeps that
 * true by construction rather than by assumption.
 */
function RoleTranscript({ sessionId }: { sessionId: string }) {
  const { events, streamingText, hasMore, loadingOlder, loadOlder, oldestIdx } =
    useSessionEvents(sessionId);
  const { data: session } = useSession(sessionId);

  return (
    <div className="flex min-h-0 flex-1 flex-col">
      <div className="flex shrink-0 items-center gap-1.5 border-b px-3 py-1 text-xs text-muted-foreground">
        {session ? (
          <>
            <span className="text-[0.7rem] leading-none">
              <StatusGlyph status={session.status} />
            </span>
            {statusLabel(session.status)}
          </>
        ) : (
          <span className="font-mono">{sessionId.slice(0, 8)}…</span>
        )}
      </div>
      <div className="min-h-0 flex-1 overflow-hidden">
        <SessionThread
          sessionId={sessionId}
          events={events}
          status={session?.status}
          streamingText={streamingText}
          transcriptWindow={{ hasMore, loadingOlder, loadOlder, oldestIdx }}
        />
      </div>
    </div>
  );
}
