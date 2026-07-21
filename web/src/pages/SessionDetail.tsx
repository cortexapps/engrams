import { useParams } from "@tanstack/react-router";
import { Fragment, useEffect, useRef, useState, type ComponentType, type ReactNode } from "react";
import { Activity, Code2, GitPullRequestArrow, Globe, Pencil, SquareTerminal } from "lucide-react";
import type { ImperativePanelHandle } from "react-resizable-panels";
import { useSession } from "../hooks/useSessions";
import { useSessionEvents } from "../hooks/useSessionEvents";
import { StatusGlyph } from "../components/Glyph";
import { SessionThread } from "../components/session-thread/SessionThread";
import { PageHeading } from "../components/page-heading";
import { TitleEditForm } from "./sessions/TitleEditForm";
import { DeleteSessionButton } from "./sessions/DeleteSessionButton";
import { WorkPane, type PaneTabId } from "../components/WorkPane";
import { statusLabel } from "./sessions/session-format";
import { useTasks } from "../hooks/useTasks";
import { useIsMobile } from "../hooks/use-mobile";
import { ProfileChip } from "../components/profiles/ProfileChip";
import {
  DurabilityReadout,
  useDurabilitySummary,
  type DurabilitySummary,
} from "../components/SessionDiagnostics";
import { ResizablePanelGroup, ResizablePanel, ResizableHandle } from "@/components/ui/resizable";
import { Sheet, SheetContent, SheetTitle } from "@/components/ui/sheet";
import { Button } from "@/components/ui/button";
import { Separator } from "@/components/ui/separator";
import type { Session, ProfileSnapshotView } from "../lib/types";

// The session workspace (ADR 0065 follow-up). The transcript is the primary
// left column; the live shell + browser live in a resizable companion pane on
// the right (Devin-style), instead of top-level tabs that hid the conversation.
// Diagnostics + the raw event log moved into that pane too. On phones the pane
// opens as a right-side overlay sheet rather than a split.
//
// The pane ALWAYS starts collapsed and its contents mount lazily — a shell/VNC
// socket only opens once the developer actually opens that view. Open-state is
// deliberately not persisted; only the last view + width are remembered so a
// reopen lands where they left it.

const PANE_PREF_KEY = "engram.workpane";
type WorkPanePref = { tab: PaneTabId; size: number };
const DEFAULT_PANE_PREF: WorkPanePref = { tab: "shell", size: 42 };

function readPanePref(): WorkPanePref {
  try {
    const raw = localStorage.getItem(PANE_PREF_KEY);
    if (!raw) return DEFAULT_PANE_PREF;
    const p = JSON.parse(raw) as Partial<WorkPanePref>;
    return {
      tab:
        p.tab === "browser" ||
        p.tab === "ide" ||
        p.tab === "side-effects" ||
        p.tab === "diagnostics"
          ? p.tab
          : "shell",
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

export function SessionDetail() {
  const { id } = useParams({ from: "/_app/sessions/$id" });
  const { data: session } = useSession(id);
  const { data: tasksData } = useTasks();
  // The task owning this session — for the masthead title + rename control.
  const task = tasksData?.tasks.find((t) => t.sessions.some((r) => r.sessionId === id));
  // Synthetic `unattributed-*` admin rows have no real task and can't be renamed.
  const taskId = task && !task.id.startsWith("unattributed-") ? task.id : null;
  const taskTitle = task?.title ?? null;
  const titleIsCustom = task?.titleIsCustom ?? false;
  const [editingTitle, setEditingTitle] = useState(false);
  const profileSnap =
    tasksData?.tasks.flatMap((t) => t.sessions).find((r) => r.sessionId === id)?.profile ?? null;
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
  const { events, streamingText } = useSessionEvents(id);
  // One poll per session, shared by React Query with the Diagnostics drawer's
  // gauges; null until the session resolves (and whenever there's nothing
  // calming to say).
  const durability = useDurabilitySummary(id, session?.status);

  // The in-guest browser (Xvfb + VNC, ADR 0065) is an optional capability,
  // present iff the session's profile selected the `browser` skill bundle. We
  // read that straight off the profile snapshot the masthead already shows.
  const browserEnabled = (profile?.skills ?? []).includes("browser");
  // The in-guest IDE (code-server, ADR 0085) is the same shape of optional
  // capability, gated on the `ide` skill.
  const ideEnabled = (profile?.skills ?? []).includes("ide");

  const isMobile = useIsMobile();
  const prefRef = useRef<WorkPanePref>(readPanePref());
  // Always start collapsed — open-state isn't persisted, so nothing inside the
  // pane mounts (and no socket opens) until the developer opens it.
  const [paneOpen, setPaneOpen] = useState(false);
  const [paneTab, setPaneTab] = useState<PaneTabId>(prefRef.current.tab);
  // Desktop only: the pane fills the work area (transcript panel collapsed).
  const [expanded, setExpanded] = useState(false);

  const paneRef = useRef<ImperativePanelHandle>(null);
  const transcriptRef = useRef<ImperativePanelHandle>(null);
  const paneSizeRef = useRef(prefRef.current.size);

  // Fall back to the shell view if the browser/IDE capability disappears (an
  // admin drops the skill and useTasks refetches) while that view is active,
  // so we never point at an absent tab.
  useEffect(() => {
    if (paneTab === "browser" && !browserEnabled) setPaneTab("shell");
  }, [paneTab, browserEnabled]);
  useEffect(() => {
    if (paneTab === "ide" && !ideEnabled) setPaneTab("shell");
  }, [paneTab, ideEnabled]);

  // Persist the last-viewed tab (width is persisted from onLayout). Open-state
  // is intentionally not stored — the pane always starts collapsed.
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
      // resize() un-collapses to an explicit width — reliable even on a fresh
      // load that started collapsed (expand() would only restore a remembered
      // size, which doesn't exist yet).
      paneRef.current?.resize(paneSizeRef.current);
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
  // the first such event, then respect a human collapse for the rest of this
  // page lifetime.
  const autoOpenedBrowserRef = useRef(false);
  useEffect(() => {
    if (!browserEnabled || autoOpenedBrowserRef.current) return;
    const agentBootedBrowser = events.some((ie) => ie.event.type === "browser_activity");
    if (agentBootedBrowser) {
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
    />
  );

  const paneTabDefs: {
    id: PaneTabId;
    label: string;
    icon: ComponentType<{ className?: string }>;
  }[] = [
    { id: "shell", label: "Shell", icon: SquareTerminal },
    ...(browserEnabled ? [{ id: "browser", label: "Browser", icon: Globe } as const] : []),
    ...(ideEnabled ? [{ id: "ide", label: "IDE", icon: Code2 } as const] : []),
    { id: "side-effects", label: "Side effects", icon: GitPullRequestArrow },
    { id: "diagnostics", label: "Diagnostics", icon: Activity },
  ];

  // The transcript column: its own masthead (task id + vitals) over the thread.
  // The work pane sits beside it at full height, so the masthead lives here
  // rather than spanning the width above both.
  const leftColumn = (
    <div className="flex h-full min-h-0 min-w-0 flex-1 flex-col overflow-hidden">
      <div className="shrink-0 px-6 pt-6 pb-4">
        <PageHeading
          eyebrow="task"
          title={
            editingTitle && taskId ? (
              <TitleEditForm
                taskId={taskId}
                initial={taskTitle ?? ""}
                isCustom={titleIsCustom}
                onDone={() => setEditingTitle(false)}
                inputClassName="h-9 text-lg"
                className="max-w-xl"
              />
            ) : (
              <span className="group/title inline-flex max-w-full items-center gap-2">
                <span className="truncate">{taskTitle ?? id}</span>
                {taskId && (
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
              </span>
            )
          }
          // A named session reads as prose; the raw-id fallback stays in the
          // lab-readout mono voice.
          titleVariant={taskTitle ? "display" : "mono"}
          showRule={false}
          actions={
            <div className="flex items-center gap-2">
              {/* Desktop reopens the pane via the edge rail; phones have no
                  rail, so they get an explicit button. */}
              <Button variant="outline" size="sm" className="md:hidden" onClick={() => openPane()}>
                <SquareTerminal />
                Panel
              </Button>
              {/* Only attributed sessions have a task to delete; synthetic
                  admin rows (taskId null) hide the control. */}
              {taskId && <DeleteSessionButton taskId={taskId} title={taskTitle} />}
            </div>
          }
        />
        {session && <SessionVitals session={session} profile={profile} durability={durability} />}
      </div>
      <div className="min-h-0 flex-1 overflow-hidden">{transcript}</div>
    </div>
  );

  return (
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
        <>
          <ResizablePanelGroup
            direction="horizontal"
            className="min-h-0 flex-1"
            onLayout={(sizes) => {
              // Only remember the split when both panels are genuinely open —
              // skip collapsed (0) and fullscreen (transcript 0) so neither
              // clobbers the preferred width.
              if (sizes[0] > 1 && sizes[1] > 1) {
                paneSizeRef.current = sizes[1];
                writePanePref({ tab: paneTab, size: sizes[1] });
              }
            }}
          >
            <ResizablePanel
              id="transcript"
              order={1}
              ref={transcriptRef}
              collapsible
              collapsedSize={0}
              minSize={30}
              defaultSize={100}
              onCollapse={() => setExpanded(true)}
              onExpand={() => setExpanded(false)}
              className="min-w-0"
            >
              {leftColumn}
            </ResizablePanel>

            <ResizableHandle className={paneOpen ? "" : "hidden"} />

            <ResizablePanel
              id="workpane"
              order={2}
              ref={paneRef}
              collapsible
              collapsedSize={0}
              minSize={24}
              defaultSize={0}
              onCollapse={() => {
                setExpanded(false);
                setPaneOpen(false);
              }}
              onExpand={() => setPaneOpen(true)}
              className="min-w-0 overflow-hidden"
            >
              <WorkPane
                sessionId={id}
                taskId={taskId}
                session={session}
                events={events}
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

          {/* Collapsed edge rail — the Devin-style reopen affordance, full
              height. Each glyph opens the pane straight to that view. */}
          {!paneOpen && (
            <div
              className="flex w-9 shrink-0 flex-col items-center gap-1 border-l bg-background py-2"
              aria-label="Open work pane"
            >
              {paneTabDefs.map((t) => (
                <Button
                  key={t.id}
                  variant="ghost"
                  size="icon"
                  className="size-7 text-muted-foreground hover:text-foreground"
                  title={`Open ${t.label}`}
                  aria-label={`Open ${t.label}`}
                  onClick={() => openPane(t.id)}
                >
                  <t.icon />
                </Button>
              ))}
            </div>
          )}
        </>
      )}
    </div>
  );
}

// The masthead vitals strip — the only session metadata a developer needs at a
// glance: lifecycle status (glyph + word), the profile this launched from
// (ADR 0053, the dense inline chip with its image on hover), and the calm
// durability telltale. Items render only when present and are divided by a
// hairline Separator, so the strip never trails a dangling divider.
function SessionVitals({
  session,
  profile,
  durability,
}: {
  session: Session;
  profile: ProfileSnapshotView | null;
  durability: DurabilitySummary | null;
}) {
  const items: { key: string; node: ReactNode }[] = [
    {
      key: "status",
      node: (
        <span className="inline-flex items-center gap-1.5">
          <StatusGlyph status={session.status} />
          <span data-testid="session-status" className="font-medium text-foreground">
            {statusLabel(session.status)}
          </span>
        </span>
      ),
    },
    ...(profile
      ? [
          {
            key: "profile",
            node: (
              <ProfileChip
                profile={profile}
                fallbackImage={session.image}
                disclosure="tooltip"
                className="max-w-full text-foreground"
              />
            ),
          },
        ]
      : []),
    ...(durability
      ? [{ key: "durability", node: <DurabilityReadout summary={durability} /> }]
      : []),
  ];

  return (
    <div className="mt-2.5 flex flex-wrap items-center gap-x-3 gap-y-1.5 text-sm text-muted-foreground">
      {items.map((item, i) => (
        <Fragment key={item.key}>
          {i > 0 && (
            <Separator orientation="vertical" className="data-[orientation=vertical]:h-4" />
          )}
          {item.node}
        </Fragment>
      ))}
    </div>
  );
}
