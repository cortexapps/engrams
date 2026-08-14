/**
 * StartScreen — the /sessions landing. A composer-first "start a task" surface
 * (the one canonical create path, replacing the old New-task dialog and the
 * /launch picker): type what the agent should do, confirm or switch the profile
 * — preselected to the one you last launched — glance at what that profile lets
 * the session reach, and launch. Recent tasks sit beneath for quick re-entry.
 *
 * A profile is the unit of "what this session can do": CreateTask takes only a
 * profile + prompt, so the profile control here switches + inspects; editing a
 * profile's powers lives in Settings (admins). Calls the same CreateTask path
 * the rest of the app does.
 */

import { useLayoutEffect, useMemo, useRef, useState } from "react";
import { useWindowHeight } from "@react-hook/window-size";
import { Link, useNavigate } from "@tanstack/react-router";
import { useQueryClient } from "@tanstack/react-query";
import { useMutation, createConnectQueryKey } from "@connectrpc/connect-query";

import { createTask, listTasks } from "../../gen/engram/app/v1/task-TaskService_connectquery";
import { sendPrompt } from "../../gen/engram/app/v1/session-SessionService_connectquery";
import { useDeleteTask, useTasksAsSessionList } from "../../hooks/useTasks";
import { useNow } from "../../hooks/useNow";
import {
  rememberLastProfileId,
  TaskComposer,
  type TaskComposerState,
} from "../../components/composer/TaskComposer";
import { StatusGlyph } from "../../components/Glyph";
import { compareSessions, relativeTime, shortId } from "./session-format";
import type { SessionListItem } from "../../lib/types";
import { Button } from "@/components/ui/button";
import { Text } from "@/components/ui/text";
import { cn } from "@/lib/utils";
import {
  serializeComposer,
  useSessionUploads,
} from "../../components/session-files/useSessionUploads";

export function StartScreen() {
  const navigate = useNavigate();
  const qc = useQueryClient();
  // `order: "recency"` — the DEFAULT list order bands live work onto page one,
  // which is right for a task list and wrong here: with 25 live or idle tasks a
  // task you finished five minutes ago never enters the window, so the one list
  // meant for re-entry cannot show the thing you just left. (It also seeds the
  // default profile below, which would pick from the same skewed window.)
  const { data: taskList } = useTasksAsSessionList({
    scope: "mine",
    pageSize: 25,
    order: "recency",
  });
  const createTaskMutation = useMutation(createTask, {
    onSuccess: () =>
      qc.invalidateQueries({
        queryKey: createConnectQueryKey({ schema: listTasks, cardinality: undefined }),
      }),
  });
  const sendPromptMutation = useMutation(sendPrompt);
  const deleteTaskMutation = useDeleteTask();

  const recent = useMemo<SessionListItem[]>(
    () => [...(taskList ?? [])].sort(compareSessions),
    [taskList],
  );

  const [composerState, setComposerState] = useState<TaskComposerState | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [deferredSessionId, setDeferredSessionId] = useState<string | undefined>();
  const [deferredTaskId, setDeferredTaskId] = useState<string | undefined>();
  const uploads = useSessionUploads(deferredSessionId);

  const launch = async ({ prompt, profileId, harnessOverride }: TaskComposerState) => {
    if (
      !profileId ||
      (!prompt.trim() && uploads.tokens.length === 0) ||
      createTaskMutation.isPending ||
      sendPromptMutation.isPending ||
      uploads.busy
    )
      return;
    setError(null);
    try {
      let sessionId = deferredSessionId;
      let mustSendDeferredPrompt = deferredSessionId != null;
      if (!sessionId) {
        const deferInitialPrompt = uploads.tokens.length > 0;
        const res = await createTaskMutation.mutateAsync({
          type: "chat",
          profileId,
          prompt: prompt.trim(),
          deferInitialPrompt,
          // Only explicit overrides ride along. Everything else resolves from
          // the profile and harness catalog on the server.
          ...(harnessOverride.harness ? { harness: harnessOverride.harness } : {}),
          ...(harnessOverride.model ? { model: harnessOverride.model } : {}),
          ...(harnessOverride.modelRouter !== null
            ? { modelRouter: harnessOverride.modelRouter }
            : {}),
          ...(harnessOverride.effort ? { effort: harnessOverride.effort } : {}),
          ...(harnessOverride.mode ? { harnessMode: harnessOverride.mode } : {}),
        });
        mustSendDeferredPrompt = deferInitialPrompt;
        rememberLastProfileId(profileId);
        if (uploads.tokens.length > 0 && res.task?.id) setDeferredTaskId(res.task.id);
        sessionId = res.task?.sessions[0]?.sessionId;
        if (sessionId && uploads.tokens.length > 0) setDeferredSessionId(sessionId);
      }
      if (sessionId && mustSendDeferredPrompt) {
        const completed = uploads.tokens.length > 0 ? await uploads.uploadAll(sessionId) : [];
        await sendPromptMutation.mutateAsync({
          sessionId,
          text: serializeComposer(prompt, completed),
          promptId: crypto.randomUUID(),
          ...(harnessOverride.mode ? { harnessMode: harnessOverride.mode } : {}),
        });
      }
      if (sessionId) {
        setDeferredSessionId(undefined);
        setDeferredTaskId(undefined);
        navigate({ to: "/sessions/$id", params: { id: sessionId } });
      } else setError("Task created but no session id returned.");
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  };

  // Vertically center the whole section on the page — computed in JS against the
  // viewport, not flex `justify-center`, so a growing composer extends DOWNWARD
  // from a fixed top instead of dragging the section up the page. Recomputes on
  // mount, on viewport-height change (useWindowHeight), and when the data-driven
  // resting height changes (profiles / recent) — but NEVER on a keystroke.
  // offsetHeight excludes the margin we set, so the measurement isn't circular;
  // clamp at 0 so a section taller than the viewport just top-anchors and scrolls.
  // wait: 1 — all but eliminate the hook's 100ms debounce so the section
  // re-centers in lockstep with a drag-resize instead of lagging behind it.
  const windowHeight = useWindowHeight({ wait: 1 });
  const scrollRef = useRef<HTMLDivElement>(null);
  const sectionRef = useRef<HTMLDivElement>(null);
  const [topPad, setTopPad] = useState(0);
  useLayoutEffect(() => {
    const scroll = scrollRef.current;
    const section = sectionRef.current;
    if (!scroll || !section) return;
    setTopPad(Math.max(0, (scroll.clientHeight - section.offsetHeight) / 2));
  }, [windowHeight, composerState?.profileId, recent.length]);

  return (
    <div ref={scrollRef} className="flex-1 overflow-auto">
      <div className="mx-auto flex min-h-full w-full max-w-4xl flex-col px-4 pb-12">
        {/* topPad vertically centers the section at the initial paint / on resize
            (see the useWindowHeight effect). It's a fixed margin, not flex
            centering, so a growing composer extends DOWNWARD from here instead of
            re-centering and dragging the heading up the page. */}
        <div ref={sectionRef} style={{ marginTop: topPad }} className="flex flex-col gap-5">
          <Text as="h1" variant="display" className="text-balance">
            Start a task
          </Text>

          <TaskComposer
            submitLabel="Launch"
            pendingLabel="Launching…"
            pending={createTaskMutation.isPending}
            disabled={sendPromptMutation.isPending}
            submitTestId="launch-task"
            recentProfileId={recent.find((row) => row.profile)?.profile?.id}
            uploads={uploads}
            onStateChange={setComposerState}
            onSubmit={launch}
          />

          {error && (
            <div
              role="alert"
              className="flex flex-wrap items-center gap-2 text-sm text-destructive"
            >
              <span>{error}</span>
              {deferredTaskId && (
                <Button
                  type="button"
                  variant="outline"
                  size="sm"
                  disabled={deleteTaskMutation.isPending}
                  onClick={() => {
                    void deleteTaskMutation.mutateAsync({ taskId: deferredTaskId }).then(() => {
                      setDeferredTaskId(undefined);
                      setDeferredSessionId(undefined);
                      setError(null);
                    });
                  }}
                >
                  Delete idle task
                </Button>
              )}
            </div>
          )}

          {/* Recent rides with the composer in the centered cluster — re-entry
              right under the box, not stranded at the foot of the page. */}
          <RecentTasks rows={recent} />
        </div>
      </div>
    </div>
  );
}

// Re-entry, not a data grid: the few most-recent tasks in the same vocabulary as
// the rail (glyph + name + profile + age), each a link into that session. The
// full table is one click away.
//
// The name is the TITLE, exactly as the rail and the task list show it: this is
// the list meant to get you back into yesterday's work, so it must not be the
// one place a task has no name. The id stays on the row's tooltip, and an
// untitled task still falls back to it.
function RecentTasks({ rows }: { rows: SessionListItem[] }) {
  const now = useNow();
  if (rows.length === 0) {
    return (
      <section className="mt-1">
        <Text variant="label" tone="muted">
          Recent
        </Text>
        <p className="mt-2.5 text-sm text-muted-foreground">Tasks you start show up here.</p>
      </section>
    );
  }
  return (
    <section className="mt-1 flex flex-col gap-2">
      <div className="flex items-center justify-between">
        <Text variant="label" tone="muted">
          Recent
        </Text>
        <Link
          to="/sessions/list"
          className="text-xs text-muted-foreground transition-colors hover:text-foreground"
        >
          See all →
        </Link>
      </div>
      <ul className="flex flex-col">
        {rows.slice(0, 5).map((s) => (
          <li key={s.id} data-testid="recent-row">
            <Link
              to="/sessions/$id"
              params={{ id: s.id }}
              title={s.id}
              className="flex items-center gap-3 rounded-md px-2 py-2 outline-none transition-colors hover:bg-accent/60 focus-visible:bg-accent/60 focus-visible:ring-[3px] focus-visible:ring-ring/40"
            >
              <span className="inline-flex w-3 shrink-0 justify-center text-[0.7rem] leading-none">
                <StatusGlyph status={s.status} />
              </span>
              {/* The name, and nothing else. The profile was here too, which on
                  a five-row re-entry list is a column of the same word — you
                  came back for the task, not for what it runs on. */}
              <span
                className={cn(
                  "min-w-0 flex-1 truncate text-sm",
                  s.title ? "font-medium" : "font-mono",
                )}
              >
                {s.title ?? shortId(s.id)}
              </span>
              <span className="ml-auto shrink-0 font-mono text-xs tabular-nums text-muted-foreground">
                {relativeTime(s.last_active_at, now)}
              </span>
            </Link>
          </li>
        ))}
      </ul>
    </section>
  );
}
