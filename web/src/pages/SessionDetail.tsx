import { Link, useParams } from "@tanstack/react-router";
import { useEffect, useMemo, useRef, useState } from "react";
import { ChevronLeft, ChevronRight, PanelsTopLeft, Pencil } from "lucide-react";
import type { PanelImperativeHandle } from "react-resizable-panels";
import { useSession } from "../hooks/useSessions";
import { useSessionEvents } from "../hooks/useSessionEvents";
import { useDocumentTitle } from "../hooks/useDocumentTitle";
import { StatusGlyph } from "../components/Glyph";
import { SessionThread } from "../components/session-thread/SessionThread";
import { TitleEditForm } from "./sessions/TitleEditForm";
import { DeleteSessionButton } from "./sessions/DeleteSessionButton";
import { WorkPane, type PaneTabId } from "../components/WorkPane";
import { shortId, statusLabel } from "./sessions/session-format";
import { useTask, useTaskForSession } from "../hooks/useTasks";
import { useIsMobile } from "../hooks/use-mobile";
import { useIsAdmin } from "../auth/AuthProvider";
import {
  ResizablePanelGroup,
  ResizablePanel,
  ResizableHandle,
  percentSize,
} from "@/components/ui/resizable";
import { Sheet, SheetContent, SheetTitle } from "@/components/ui/sheet";
import { Button } from "@/components/ui/button";
import type { ProfileSnapshotView, IndexedEvent } from "../lib/types";
import type { Task } from "../gen/engram/app/v1/task_pb";

// The session workspace (ADR 0065 follow-up). The transcript is the primary
// left column; the live shell + browser live in a resizable companion pane on
// the right (Devin-style), instead of top-level tabs that hid the conversation.
// Diagnostics + the raw event log moved into that pane too. On phones the pane
// opens as a right-side overlay sheet rather than a split.
//
// The pane starts open on desktop and closed in the mobile sheet. Open-state is
// deliberately not persisted; only the last view + width are remembered.

const PANE_PREF_KEY = "engram.workpane";
type WorkPanePref = { tab: PaneTabId; size: number };
const DEFAULT_PANE_PREF: WorkPanePref = { tab: "overview", size: 42 };

export function paneTabFromStored(value: unknown): PaneTabId {
  switch (value) {
    case "overview":
    case "changes":
    case "shell":
    case "browser":
    case "ide":
    case "diagnostics":
      return value;
    default:
      return "overview";
  }
}

function readPanePref(): WorkPanePref {
  try {
    const raw = localStorage.getItem(PANE_PREF_KEY);
    if (!raw) return DEFAULT_PANE_PREF;
    const p = JSON.parse(raw) as { tab?: unknown; size?: unknown };
    return {
      // Old Plan, Tasks, and Side effects preferences now open the home view.
      tab: paneTabFromStored(p.tab),
      // Clamp within [pane minSize, 100 − transcript minSize] so a restored
      // width never collides with either panel's floor (react-resizable-panels
      // would otherwise clamp it and warn).
      size:
        typeof p.size === "number" && p.size >= 24 && p.size <= 70
          ? p.size
          : DEFAULT_PANE_PREF.size,
    };
  } catch {
    return DEFAULT_PANE_PREF;
  }
}

/**
 * True when the agent drove the browser AFTER `sinceMs` (a wall-clock ms
 * reading taken when this session was opened).
 *
 * The SSE feed replays the whole durable log from idx 0, so the raw presence
 * of a `browser_activity` event says nothing about NOW — it only says the
 * agent browsed at some point. Only activity that lands while the human has
 * the session open earns the pane.
 *
 * A malformed timestamp parses to NaN and compares false — no auto-open,
 * which is the safe direction.
 */
export function hasLiveBrowserActivity(events: IndexedEvent[], sinceMs: number): boolean {
  return events.some(
    (ie) => ie.event.type === "browser_activity" && Date.parse(ie.event.at) > sinceMs,
  );
}

function writePanePref(pref: WorkPanePref) {
  try {
    localStorage.setItem(PANE_PREF_KEY, JSON.stringify(pref));
  } catch {
    // ignore — persistence is best-effort
  }
}

function isTypingTarget(el: Element | null): boolean {
  if (!el) return false;
  const tag = el.tagName;
  return (
    tag === "INPUT" ||
    tag === "TEXTAREA" ||
    tag === "SELECT" ||
    (el as HTMLElement).isContentEditable
  );
}

export function taskDisplayTitle(task: Task | null | undefined, sessionId: string): string {
  return task?.localTaskName ?? task?.title ?? sessionId;
}

function primarySessionId(task: Task | undefined): string | undefined {
  return (
    task?.sessions.find((ref) => ref.role === "primary")?.sessionId ?? task?.sessions[0]?.sessionId
  );
}

export function SessionDetail() {
  const { id } = useParams({ from: "/_app/sessions/$id" });
  const { data: session } = useSession(id);
  // Resolve by primary session because ListTasks intentionally excludes child
  // tasks. A list lookup made child pages look like unattributed sessions.
  const { data: task } = useTaskForSession(id);
  // Synthetic `unattributed-*` admin rows have no real task and can't be renamed.
  const taskId = task && !task.id.startsWith("unattributed-") ? task.id : null;
  // GetTask returns descendants of the selected task. A child page makes one
  // additional root lookup so its Overview can show parents and siblings too.
  const rootTaskId = task?.rootTaskId ?? task?.id;
  const { data: loadedRootTask } = useTask(task?.parentTaskId ? rootTaskId : null);
  const taskTree = task?.parentTaskId ? loadedRootTask : task;
  const treeTasks = taskTree ? [taskTree, ...(taskTree.descendants ?? [])] : [];
  const parentTask = task?.parentTaskId
    ? treeTasks.find((candidate) => candidate.id === task.parentTaskId)
    : undefined;
  const parentSessionId = primarySessionId(parentTask);
  const taskTitle = taskDisplayTitle(task, id);
  const titleIsCustom = task?.titleIsCustom ?? false;
  const titleEditable = taskId !== null && !task?.parentTaskId;
  const [editingTitle, setEditingTitle] = useState(false);
  const profileSnap = task?.sessions.find((ref) => ref.sessionId === id)?.profile ?? null;
  // Normalize the embedded snapshot to the UI view once. Point-in-time by
  // design (ADR 0053) — what this session launched from, not the profile's
  // current state.
  const profile: ProfileSnapshotView | null = profileSnap
    ? {
        id: profileSnap.id,
        name: profileSnap.name,
        icon: profileSnap.icon,
        archived: profileSnap.archived,
        imageUri: profileSnap.imageUri,
        skills: profileSnap.skills,
      }
    : null;
  // ADR 0063 B2 echo: the orchestrator persists the EFFECTIVE selection
  // (override ?? profile ?? catalog default) on the task at create time.
  // Unset on tasks that pre-date the echo columns; "" (proto3 unset) = absent.
  const effectiveSelection =
    task?.harness || task?.model || task?.effort
      ? {
          harness: task.harness || undefined,
          model: task.model || undefined,
          effort: task.effort || undefined,
        }
      : null;
  const { events, streamingText, hasMore, loadingOlder, loadOlder, oldestIdx } =
    useSessionEvents(id);

  // The in-guest browser (Xvfb + VNC, ADR 0065) is an optional capability,
  // present iff the session's profile selected the `browser` skill bundle. We
  // read that straight off the profile snapshot the masthead already shows.
  const browserEnabled = (profile?.skills ?? []).includes("browser");
  // The in-guest IDE (code-server, ADR 0085) is the same shape of optional
  // capability, gated on the `ide` skill.
  const ideEnabled = (profile?.skills ?? []).includes("ide");
  const isAdmin = useIsAdmin();

  const isMobile = useIsMobile();
  const prefRef = useRef<WorkPanePref>(readPanePref());
  // Desktop starts open at the preferred split. The mobile sheet stays closed.
  const [paneOpen, setPaneOpen] = useState(!isMobile);
  const [paneTab, setPaneTab] = useState<PaneTabId>(prefRef.current.tab);
  // Desktop only: the pane fills the work area (transcript panel collapsed).
  const [expanded, setExpanded] = useState(false);

  const paneRef = useRef<PanelImperativeHandle>(null);
  const transcriptRef = useRef<PanelImperativeHandle>(null);
  const paneSizeRef = useRef(prefRef.current.size);

  // useIsMobile resolves after the first browser render. Close the sheet when
  // the layout enters mobile; later user opens do not rerun this effect.
  useEffect(() => {
    if (isMobile) {
      setPaneOpen(false);
      setExpanded(false);
    }
  }, [isMobile]);

  // Fall back to the home view if the browser/IDE capability disappears (an
  // admin drops the skill and useTasks refetches) while that view is active,
  // so we never point at an absent tab.
  useEffect(() => {
    if (paneTab === "browser" && !browserEnabled) setPaneTab("overview");
  }, [paneTab, browserEnabled]);
  useEffect(() => {
    if (paneTab === "ide" && !ideEnabled) setPaneTab("overview");
  }, [paneTab, ideEnabled]);

  // Persist the last-viewed tab (width is persisted from onLayout). Open-state
  // is intentionally not stored.
  useEffect(() => {
    writePanePref({ tab: paneTab, size: paneSizeRef.current });
  }, [paneTab]);

  const openPane = (tab?: PaneTabId) => {
    if (tab) setPaneTab(tab);
    // Set the logical state as well as resizing the desktop panel. On the
    // first render useIsMobile has not settled yet; keeping this true means a
    // browser-activity signal cannot be consumed by the temporary desktop
    // branch and then disappear when the mobile sheet mounts.
    setPaneOpen(true);
    if (!isMobile) {
      // resize() un-collapses to the preferred width after a user collapse.
      // The imperative API reads a bare number as PIXELS in v4, and the stored
      // width is a percentage — so it has to go through percentSize().
      paneRef.current?.resize(percentSize(paneSizeRef.current));
    }
  };

  const collapsePane = () => {
    setExpanded(false);
    setPaneOpen(false);
    if (!isMobile) {
      paneRef.current?.collapse();
    }
  };

  const toggleExpand = () => {
    if (expanded) transcriptRef.current?.expand();
    else transcriptRef.current?.collapse();
  };

  // ADR 0097: the shared harness enriches a browser-driving Shell/Bash call
  // into browser_activity with the same tool id. Surface the shared Chrome on
  // the first such event that lands WHILE this page is open, then respect a
  // human collapse for the rest of this page lifetime.
  //
  // Replayed history must never open the pane: the SSE feed replays the whole
  // durable log from idx 0, so opening a session the agent browsed hours ago
  // would otherwise pop the browser on every visit. The event's coordinator
  // timestamp against the moment this session was opened is the boundary.
  // Clock skew degrades in the safe direction — a browser clock that runs
  // ahead only delays the auto-open to the next activity event (and `]` always
  // works). The rail keeps this component mounted across session switches, so
  // the boundary is keyed by session id, not by mount.
  const autoOpenedBrowserRef = useRef(false);
  const openedAtRef = useRef<{ id: string; ms: number }>({ id, ms: Date.now() });
  if (openedAtRef.current.id !== id) {
    openedAtRef.current = { id, ms: Date.now() };
    autoOpenedBrowserRef.current = false;
  }
  useEffect(() => {
    if (!browserEnabled || autoOpenedBrowserRef.current) return;
    if (hasLiveBrowserActivity(events, openedAtRef.current.ms)) {
      autoOpenedBrowserRef.current = true;
      openPane("browser");
    }
    // openPane reads refs + isMobile; events/browserEnabled are the reactive inputs.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [events, browserEnabled]);

  // `]` toggles the pane — but never while typing (composer, shell textarea).
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== "]" || e.metaKey || e.ctrlKey || e.altKey) return;
      if (isTypingTarget(document.activeElement)) return;
      e.preventDefault();
      if (paneOpen) collapsePane();
      else openPane();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
    // openPane/collapsePane read refs + isMobile; paneOpen is the only reactive
    // input to the branch.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [paneOpen, isMobile]);

  const transcript = (
    <SessionThread
      sessionId={id}
      events={events}
      status={session?.status}
      streamingText={streamingText}
      transcriptWindow={{ hasMore, loadingOlder, loadOlder, oldestIdx }}
    />
  );

  // ADR 0107: the session is waiting on the user (a plan review or an
  // unanswered question) — the tab title picks up the ● prefix.
  const needsAttention = useMemo(() => {
    const submitted = new Set<string>();
    for (const { event } of events) {
      if (event.type === "tool_result_submitted") submitted.add(event.tool_call_id);
    }
    return events.some(
      ({ event }) =>
        event.type === "tool_call_requested" &&
        (event.name === "exit_plan_mode" || event.name === "ask_user_question") &&
        !submitted.has(event.tool_call_id),
    );
  }, [events]);
  useDocumentTitle(needsAttention ? `\u25cf ${taskTitle ?? shortId(id)} — engrams` : null);

  // The masthead is the thread sheet's own header, not a band spanning both
  // surfaces — the thread and the work pane are two separate sheets now, and a
  // header bridging them would glue them back together. View switching stays in
  // the work pane header, so this row only carries session identity.
  const masthead = (
    <header className="flex h-12 shrink-0 items-center gap-2 border-b px-4">
      <div className="flex min-w-0 flex-1 items-center gap-2">
        {session && (
          <span
            className="shrink-0 text-xs"
            aria-label={statusLabel(session.status)}
            title={statusLabel(session.status)}
            data-testid="session-status-glyph"
          >
            <StatusGlyph status={session.status} attention={needsAttention} />
          </span>
        )}
        {editingTitle && titleEditable ? (
          <TitleEditForm
            taskId={taskId}
            initial={taskTitle}
            isCustom={titleIsCustom}
            onDone={() => setEditingTitle(false)}
            inputClassName="h-8 text-base"
            className="max-w-xl flex-1"
          />
        ) : (
          <div className="group/title flex min-w-0 items-center gap-1">
            {parentSessionId && parentTask && (
              <>
                <Link
                  to="/sessions/$id"
                  params={{ id: parentSessionId }}
                  aria-label={`Back to ${taskDisplayTitle(parentTask, parentSessionId)}`}
                  title={`Back to ${taskDisplayTitle(parentTask, parentSessionId)}`}
                  className="flex min-w-0 max-w-40 shrink items-center gap-0.5 rounded-md px-1.5 py-1 text-sm text-muted-foreground transition-colors hover:bg-muted hover:text-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
                >
                  <ChevronLeft className="size-4 shrink-0" />
                  <span className="truncate">{taskDisplayTitle(parentTask, parentSessionId)}</span>
                </Link>
                <ChevronRight className="size-3.5 shrink-0 text-muted-foreground/60" aria-hidden />
              </>
            )}
            {/* Truncation is lossy, so the full title stays reachable as a
                native tooltip. */}
            <span
              className="min-w-0 truncate text-base font-medium"
              title={taskTitle}
              data-testid="session-title"
            >
              {taskTitle}
            </span>
            {titleEditable && (
              <Button
                type="button"
                size="icon-xs"
                variant="ghost"
                onClick={() => setEditingTitle(true)}
                aria-label="Rename session"
                title="Rename"
                className="shrink-0 opacity-0 transition-opacity group-hover/title:opacity-100 focus-visible:opacity-100"
              >
                <Pencil />
              </Button>
            )}
          </div>
        )}
      </div>
      {/* The switcher lives in the pane header, so a closed pane has no
          affordance of its own — this button is the way back in. Mobile
          always shows it (the sheet starts closed). */}
      {(isMobile || !paneOpen) && (
        <Button type="button" variant="outline" size="sm" onClick={() => openPane()}>
          <PanelsTopLeft />
          Panel
        </Button>
      )}
      {/* Only attributed sessions have a task to delete; synthetic admin rows
          (taskId null) hide the control. */}
      {taskId && <DeleteSessionButton taskId={taskId} title={taskTitle} />}
    </header>
  );

  // The thread sheet: the conversation is the subject of this page, so it is
  // the lightest, most raised surface on screen. It carries its own masthead.
  const leftColumn = (
    <div className="work-sheet flex h-full min-h-0 min-w-0 flex-1 flex-col overflow-hidden rounded-xl border bg-background">
      {masthead}
      <div className="min-h-0 flex-1 overflow-hidden">{transcript}</div>
    </div>
  );

  return (
    // The 8px gutter is the cover showing between the two sheets. Without it
    // the shadows have nothing to fall onto and the lift disappears.
    <div className="flex min-h-0 flex-1 flex-col overflow-hidden p-2">
      <div className="flex min-h-0 flex-1 overflow-hidden">
        {isMobile ? (
          <>
            {leftColumn}
            <Sheet open={paneOpen} onOpenChange={(o) => !o && collapsePane()}>
              <SheetContent
                side="right"
                showCloseButton={false}
                className="w-full gap-0 p-0 sm:max-w-xl"
              >
                <SheetTitle className="sr-only">Session work pane</SheetTitle>
                <WorkPane
                  sessionId={id}
                  taskId={taskId}
                  session={session}
                  events={events}
                  profile={profile}
                  selection={effectiveSelection}
                  taskTree={taskTree}
                  isAdmin={isAdmin}
                  open={paneOpen}
                  tab={paneTab}
                  onTabChange={setPaneTab}
                  browserEnabled={browserEnabled}
                  ideEnabled={ideEnabled}
                  onCollapse={collapsePane}
                  variant="overlay"
                />
              </SheetContent>
            </Sheet>
          </>
        ) : (
          <ResizablePanelGroup
            orientation="horizontal"
            className="min-h-0 flex-1"
            // v4 hands back a map of panel id to percentage rather than an
            // ordered array. `onLayoutChanged` (past tense) fires once the
            // pointer is released instead of on every drag frame, which is
            // what upstream recommends for writing to a storage API — so the
            // preference is no longer rewritten dozens of times per drag.
            onLayoutChanged={(layout) => {
              const transcript = layout.transcript;
              const workpane = layout.workpane;
              // Only remember the split when both panels are genuinely open —
              // skip collapsed (0) and fullscreen (transcript 0) so neither
              // clobbers the preferred width.
              if (transcript > 1 && workpane > 1) {
                paneSizeRef.current = workpane;
                writePanePref({ tab: paneTab, size: workpane });
              }
            }}
          >
            <ResizablePanel
              id="transcript"
              panelRef={transcriptRef}
              collapsible
              collapsedSize={0}
              minSize={30}
              defaultSize={100 - paneSizeRef.current}
              // v4 removed onCollapse/onExpand; the collapse edge is derived
              // from the resize callback instead. `prev === undefined` is the
              // initial mount, which is not a transition.
              onResize={(size, _id, prev) => {
                if (prev === undefined) return;
                if (size.asPercentage === 0 && prev.asPercentage !== 0) setExpanded(true);
                else if (size.asPercentage !== 0 && prev.asPercentage === 0) setExpanded(false);
              }}
              className="min-w-0"
            >
              {leftColumn}
            </ResizablePanel>

            {/* The gutter between the two sheets. The pill is the component's
                own look now — it was spelled out here, which left the rail's
                handle on the other side of the page looking like a hairline. */}
            <ResizableHandle className={paneOpen ? undefined : "hidden"} />

            <ResizablePanel
              id="workpane"
              panelRef={paneRef}
              collapsible
              collapsedSize={0}
              minSize={24}
              defaultSize={paneSizeRef.current}
              onResize={(size, _id, prev) => {
                if (prev === undefined) return;
                if (size.asPercentage === 0 && prev.asPercentage !== 0) {
                  setExpanded(false);
                  setPaneOpen(false);
                } else if (size.asPercentage !== 0 && prev.asPercentage === 0) {
                  setPaneOpen(true);
                }
              }}
              className="min-w-0 overflow-hidden"
            >
              <WorkPane
                sessionId={id}
                taskId={taskId}
                session={session}
                events={events}
                profile={profile}
                selection={effectiveSelection}
                taskTree={taskTree}
                isAdmin={isAdmin}
                open={paneOpen}
                tab={paneTab}
                onTabChange={setPaneTab}
                browserEnabled={browserEnabled}
                ideEnabled={ideEnabled}
                onCollapse={collapsePane}
                variant="panel"
                expanded={expanded}
                onToggleExpand={toggleExpand}
              />
            </ResizablePanel>
          </ResizablePanelGroup>
        )}
      </div>
    </div>
  );
}
