import { useEffect, useMemo, useState, type ComponentType } from "react";
import {
  Activity,
  Code2,
  FileDiff,
  Globe,
  Maximize2,
  Minimize2,
  MoreHorizontal,
  PanelRightClose,
  PanelsTopLeft,
  SquareTerminal,
  X,
} from "lucide-react";

import { TerminalPane } from "./TerminalPane";
import { BrowserPane } from "./BrowserPane";
import { ChangesPane } from "./ChangesPane";
import { IdePane } from "./IdePane";
import { OverviewPane, type OverviewSelection } from "./OverviewPane";
import { DiagnosticsPanel } from "./SessionDiagnostics";
import { WorkDock } from "./WorkDock";
import { extractFileChanges } from "./session-thread/fileChanges";
import { Button } from "@/components/ui/button";
import {
  DropdownMenu,
  DropdownMenuCheckboxItem,
  DropdownMenuContent,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { cn } from "@/lib/utils";
import type { IndexedEvent, ProfileSnapshotView, Session } from "../lib/types";

// The Devin-style work pane: the shell + browser, lifted out of the transcript
// into a companion surface so the conversation and the live view sit side by
// side instead of hiding each other. The pane header keeps primary views
// visible and moves secondary views into an overflow menu. The body keeps
// every live pane mounted (display:none swap) so the ttyd / VNC / iframe
// sockets survive view switches — the same keep-mounted contract the panes
// rely on, now owned here.
//
// It renders identically inside the desktop resizable panel (`variant="panel"`,
// with expand-to-fill) and the mobile overlay sheet (`variant="overlay"`, where
// collapse means "close the sheet").

export type PaneTabId = "overview" | "changes" | "shell" | "browser" | "ide" | "diagnostics";

export interface PaneViewDef {
  id: PaneTabId;
  label: string;
  icon: ComponentType<{ className?: string }>;
}

export interface PaneViewGroups {
  primary: PaneViewDef[];
  overflow: PaneViewDef[];
}

export function paneViewDefs(opts: {
  browserEnabled: boolean;
  ideEnabled: boolean;
  hasChanges: boolean;
  isAdmin: boolean;
}): PaneViewGroups {
  return {
    primary: [
      { id: "overview", label: "Overview", icon: PanelsTopLeft },
      ...(opts.browserEnabled ? [{ id: "browser", label: "Browser", icon: Globe } as const] : []),
      ...(opts.ideEnabled ? [{ id: "ide", label: "IDE", icon: Code2 } as const] : []),
    ],
    overflow: [
      ...(opts.hasChanges ? [{ id: "changes", label: "Changes", icon: FileDiff } as const] : []),
      { id: "shell", label: "Shell", icon: SquareTerminal },
      ...(opts.isAdmin
        ? [{ id: "diagnostics", label: "Diagnostics", icon: Activity } as const]
        : []),
    ],
  };
}

function PaneTab({
  view,
  active,
  onSelect,
  responsive,
}: {
  view: PaneViewDef;
  active: boolean;
  onSelect: (id: PaneTabId) => void;
  responsive?: boolean;
}) {
  return (
    <Button
      variant="ghost"
      size="sm"
      className={cn(
        "h-8 shrink-0 gap-1.5 px-2 text-muted-foreground",
        active && "bg-accent text-foreground",
        responsive && "@max-[20rem]/workpane:hidden",
      )}
      aria-label={view.label}
      aria-pressed={active}
      title={view.label}
      data-testid={`pane-tab-${view.id}`}
      onClick={() => onSelect(view.id)}
    >
      <view.icon />
      <span className="@max-[34rem]/workpane:hidden">{view.label}</span>
    </Button>
  );
}

function MoreViewsMenu({
  views,
  activeTab,
  onSelect,
  compact = false,
}: {
  views: PaneViewDef[];
  activeTab: PaneTabId | null;
  onSelect: (id: PaneTabId) => void;
  compact?: boolean;
}) {
  return (
    <DropdownMenu>
      <DropdownMenuTrigger asChild>
        <Button
          variant="ghost"
          size="sm"
          className={cn(
            "h-8 shrink-0 gap-1.5 px-2 text-muted-foreground",
            activeTab && "bg-accent text-foreground",
          )}
          aria-label="More views"
          aria-pressed={activeTab !== null}
          title="More views"
          data-testid={compact ? "pane-more-menu-compact" : "pane-more-menu"}
        >
          <MoreHorizontal />
          <span className="@max-[34rem]/workpane:hidden">More</span>
        </Button>
      </DropdownMenuTrigger>
      <DropdownMenuContent align="start">
        {views.map((view) => (
          <DropdownMenuCheckboxItem
            key={view.id}
            checked={view.id === activeTab}
            onCheckedChange={() => onSelect(view.id)}
          >
            <view.icon />
            <span>{view.label}</span>
          </DropdownMenuCheckboxItem>
        ))}
      </DropdownMenuContent>
    </DropdownMenu>
  );
}

export interface WorkPaneProps {
  sessionId: string;
  /** Owning task resolved by SessionDetail; null for orphan sessions. */
  taskId: string | null;
  /** The session row, for the Overview and Diagnostics views. */
  session: Session | undefined;
  /** The event stream for summaries, changes, the work dock, and diagnostics. */
  events: IndexedEvent[];
  /** The profile snapshot from the owning session row. */
  profile: ProfileSnapshotView | null;
  /** Effective harness, model, and effort for the owning task. */
  selection: OverviewSelection | null;
  /** Whether the viewer can open the operator diagnostics view. */
  isAdmin: boolean;
  /**
   * Whether the pane is actually open (visible). The pane stays MOUNTED while
   * collapsed so the shell/browser/IDE sockets survive — but nothing inside is
   * mounted until it's first opened, so we never open a socket the user hasn't
   * asked for.
   */
  open: boolean;
  tab: PaneTabId;
  onTabChange: (tab: PaneTabId) => void;
  browserEnabled: boolean;
  ideEnabled: boolean;
  /** Hide the pane (panel: collapse the panel; overlay: close the sheet). */
  onCollapse: () => void;
  variant?: "panel" | "overlay";
  /** Desktop only: pane fills the work area (transcript collapsed). */
  expanded?: boolean;
  onToggleExpand?: () => void;
}

export function WorkPane({
  sessionId,
  taskId,
  session,
  events,
  profile,
  selection,
  isAdmin,
  open,
  tab,
  onTabChange,
  browserEnabled,
  ideEnabled,
  onCollapse,
  variant = "panel",
  expanded = false,
  onToggleExpand,
}: WorkPaneProps) {
  const hasChanges = useMemo(() => extractFileChanges(events).length > 0, [events]);
  const viewGroups = paneViewDefs({ browserEnabled, ideEnabled, hasChanges, isAdmin });
  const views = [...viewGroups.primary, ...viewGroups.overflow];
  const effectiveTab = views.some((view) => view.id === tab) ? tab : "overview";
  const promotedView = viewGroups.overflow.find((view) => view.id === effectiveTab);
  const compactMenuViews = views.filter((view) => view.id !== effectiveTab);

  // Lazy-mount each live pane the first time it's viewed *while the pane is
  // open*, then keep it mounted for the life of the WorkPane (hidden via
  // display:none on tab switch / collapse) so its socket survives. Gating on
  // `open` is what keeps a collapsed pane from silently opening a shell.
  const [shellEverActive, setShellEverActive] = useState(false);
  const [browserEverActive, setBrowserEverActive] = useState(false);
  const [ideEverActive, setIdeEverActive] = useState(false);
  useEffect(() => {
    if (!open) return;
    if (effectiveTab === "shell") setShellEverActive(true);
    if (effectiveTab === "browser") setBrowserEverActive(true);
    if (effectiveTab === "ide") setIdeEverActive(true);
  }, [effectiveTab, open]);

  return (
    <section
      className="@container/workpane flex h-full min-h-0 flex-col bg-background"
      aria-label="Work pane"
    >
      <header className="flex h-11 shrink-0 items-center justify-between gap-1 border-b px-1.5">
        <div className="flex min-w-0 items-center gap-0.5">
          {viewGroups.primary.map((view) => (
            <PaneTab
              key={view.id}
              view={view}
              active={view.id === effectiveTab}
              onSelect={onTabChange}
              responsive={view.id !== "overview"}
            />
          ))}
          {promotedView && <PaneTab view={promotedView} active onSelect={onTabChange} responsive />}
          <div className="@max-[20rem]/workpane:hidden">
            <MoreViewsMenu
              views={viewGroups.overflow}
              activeTab={promotedView?.id ?? null}
              onSelect={onTabChange}
            />
          </div>
          <div className="hidden @max-[20rem]/workpane:block">
            <MoreViewsMenu
              views={compactMenuViews}
              activeTab={effectiveTab === "overview" ? null : effectiveTab}
              onSelect={onTabChange}
              compact
            />
          </div>
        </div>

        <div className="flex shrink-0 items-center gap-0.5 self-center">
          {variant === "panel" && onToggleExpand && (
            <Button
              variant="ghost"
              size="icon"
              className="size-7 text-muted-foreground hover:text-foreground"
              onClick={onToggleExpand}
              aria-label={expanded ? "Restore pane" : "Expand pane"}
              title={expanded ? "Restore" : "Expand"}
            >
              {expanded ? <Minimize2 /> : <Maximize2 />}
            </Button>
          )}
          <Button
            variant="ghost"
            size="icon"
            className="size-7 text-muted-foreground hover:text-foreground"
            onClick={onCollapse}
            aria-label={variant === "overlay" ? "Close pane" : "Collapse pane"}
            title={variant === "overlay" ? "Close" : "Collapse"}
          >
            {variant === "overlay" ? <X /> : <PanelRightClose />}
          </Button>
        </div>
      </header>

      <div className="relative min-h-0 flex-1">
        {shellEverActive && (
          <div
            className="absolute inset-0"
            style={{ display: effectiveTab === "shell" ? "block" : "none" }}
          >
            <TerminalPane sessionId={sessionId} />
          </div>
        )}
        {browserEnabled && browserEverActive && (
          <div
            className="absolute inset-0"
            style={{ display: effectiveTab === "browser" ? "block" : "none" }}
          >
            <BrowserPane sessionId={sessionId} />
          </div>
        )}
        {ideEnabled && ideEverActive && (
          <div
            className="absolute inset-0"
            style={{ display: effectiveTab === "ide" ? "block" : "none" }}
          >
            <IdePane sessionId={sessionId} />
          </div>
        )}
        {/* Information views hold no live socket, so they mount only while
            open and selected. They do no work while the pane is collapsed. */}
        {open && effectiveTab === "overview" && (
          <div className="absolute inset-0 overflow-hidden">
            <OverviewPane
              sessionId={sessionId}
              taskId={taskId}
              session={session}
              events={events}
              profile={profile}
              selection={selection}
              onShowChanges={() => onTabChange("changes")}
            />
          </div>
        )}
        {open && effectiveTab === "changes" && (
          <div className="absolute inset-0 overflow-hidden">
            <ChangesPane events={events} />
          </div>
        )}
        {open && effectiveTab === "diagnostics" && (
          <div className="absolute inset-0 overflow-hidden">
            <DiagnosticsPanel session={session} sessionId={sessionId} events={events} />
          </div>
        )}
      </div>

      {open && <WorkDock events={events} />}
    </section>
  );
}
