import { useEffect, useLayoutEffect, useMemo, useRef, useState, type ComponentType } from "react";
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
  DropdownMenuContent,
  DropdownMenuItem,
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

// The tab always shows its icon — that is what survives the icon-only stage,
// and it is what the eye finds before it reads a word. The active tab is named
// by FILL, not by a rule: on the cover the pill is two clear steps above the
// pane, which reads at a glance where a 2px underline does not.
const TAB_CLASS_NAME = "h-8 shrink-0 gap-1.5 px-2 text-muted-foreground hover:text-foreground";
const TAB_ACTIVE_CLASS_NAME = "bg-accent text-foreground shadow-xs hover:bg-accent";

export interface StripLayout {
  /** Number of tabs rendered in the strip; the rest live in the More menu. */
  count: number;
  /** Tabs drop their labels once the labeled set stops fitting. */
  iconOnly: boolean;
}

function fitsAll(widths: number[], gapPx: number, available: number): boolean {
  const total = widths.reduce((sum, width) => sum + width, 0);
  return total + gapPx * (widths.length - 1) <= available;
}

function fittingPrefix(widths: number[], moreWidth: number, gapPx: number, available: number) {
  let used = moreWidth;
  let count = 0;
  for (const width of widths) {
    if (used + gapPx + width > available) break;
    used += gapPx + width;
    count += 1;
  }
  return count;
}

/** Widest-first strip layout, in three stages: every tab labeled → a labeled
 *  prefix + More, shrinking no further than the first `primaryCount` tabs
 *  (Overview/Browser/IDE) → icon-only tabs, again as many as fit + More.
 *  Icon-only tabs all share one width, so a single `iconWidth` measures them. */
export function stripLayout(
  labeledWidths: number[],
  iconWidth: number,
  moreWidth: number,
  gapPx: number,
  available: number,
  primaryCount: number,
): StripLayout {
  const all = labeledWidths.length;
  if (all === 0) return { count: 0, iconOnly: false };
  // jsdom has no layout engine, so zero-width measurements keep every tab visible.
  if (labeledWidths.every((width) => width === 0)) return { count: all, iconOnly: false };

  // The More trigger renders only when something overflows — don't reserve
  // room for it while everything fits, or one tab would collapse for no reason.
  if (fitsAll(labeledWidths, gapPx, available)) return { count: all, iconOnly: false };

  const labeledCount = fittingPrefix(labeledWidths, moreWidth, gapPx, available);
  if (labeledCount >= primaryCount) return { count: labeledCount, iconOnly: false };

  const iconWidths = Array<number>(all).fill(iconWidth);
  if (fitsAll(iconWidths, gapPx, available)) return { count: all, iconOnly: true };
  return {
    count: Math.max(1, fittingPrefix(iconWidths, moreWidth, gapPx, available)),
    iconOnly: true,
  };
}

/** Every available view in strip priority order: the fitting prefix renders
 *  as labeled tabs, the remainder lives in the More menu. */
export function paneViewDefs(opts: {
  browserEnabled: boolean;
  ideEnabled: boolean;
  hasChanges: boolean;
  isAdmin: boolean;
}): PaneViewDef[] {
  return [
    { id: "overview", label: "Overview", icon: PanelsTopLeft },
    ...(opts.browserEnabled ? [{ id: "browser", label: "Browser", icon: Globe } as const] : []),
    ...(opts.ideEnabled ? [{ id: "ide", label: "IDE", icon: Code2 } as const] : []),
    ...(opts.hasChanges ? [{ id: "changes", label: "Changes", icon: FileDiff } as const] : []),
    { id: "shell", label: "Shell", icon: SquareTerminal },
    ...(opts.isAdmin ? [{ id: "diagnostics", label: "Diagnostics", icon: Activity } as const] : []),
  ];
}

function PaneTab({
  view,
  active,
  iconOnly,
  onSelect,
}: {
  view: PaneViewDef;
  active: boolean;
  iconOnly: boolean;
  onSelect: (id: PaneTabId) => void;
}) {
  return (
    <Button
      variant="ghost"
      size="sm"
      className={cn(TAB_CLASS_NAME, active && TAB_ACTIVE_CLASS_NAME)}
      aria-label={view.label}
      aria-pressed={active}
      title={view.label}
      data-testid={`pane-tab-${view.id}`}
      onClick={() => onSelect(view.id)}
    >
      <view.icon />
      {!iconOnly && <span>{view.label}</span>}
    </Button>
  );
}

function MoreViewsMenu({
  views,
  activeTab,
  onSelect,
}: {
  views: PaneViewDef[];
  activeTab: PaneTabId | null;
  onSelect: (id: PaneTabId) => void;
}) {
  return (
    <DropdownMenu>
      <DropdownMenuTrigger asChild>
        <Button
          variant="ghost"
          size="sm"
          className={cn(TAB_CLASS_NAME, activeTab && TAB_ACTIVE_CLASS_NAME)}
          aria-label="More views"
          aria-pressed={activeTab !== null}
          title="More views"
          data-testid="pane-more-menu"
        >
          <MoreHorizontal />
        </Button>
      </DropdownMenuTrigger>
      <DropdownMenuContent align="start">
        {views.map((view) => (
          <DropdownMenuItem
            key={view.id}
            className={cn(view.id === activeTab && "bg-accent text-foreground")}
            onSelect={() => onSelect(view.id)}
          >
            <view.icon />
            <span>{view.label}</span>
          </DropdownMenuItem>
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
  const views = paneViewDefs({ browserEnabled, ideEnabled, hasChanges, isAdmin });
  const effectiveTab = views.some((view) => view.id === tab) ? tab : "overview";
  // The labeled stage never collapses past these — the always-on core.
  const primaryCount = views.filter((view) =>
    ["overview", "browser", "ide"].includes(view.id),
  ).length;
  const measurementKey = views.map((view) => view.id).join("|");
  const stripRef = useRef<HTMLDivElement>(null);
  const measurementRef = useRef<HTMLDivElement>(null);
  const [layout, setLayout] = useState<StripLayout>({ count: views.length, iconOnly: false });

  useLayoutEffect(() => {
    const strip = stripRef.current;
    const measurement = measurementRef.current;
    if (!strip || !measurement) return;

    const measure = () => {
      // The measurement row holds the labeled set, then one icon-only tab,
      // then the More trigger — sliced back apart by position here.
      const children = Array.from(measurement.children) as HTMLElement[];
      const more = children.pop();
      const icon = children.pop();
      if (!more || !icon) return;
      const width = (el: HTMLElement) => el.getBoundingClientRect().width;
      const labeledWidths = children.map(width);
      const style = getComputedStyle(measurement);
      const gapPx = Number.parseFloat(style.columnGap || style.gap) || 0;
      setLayout(
        stripLayout(
          labeledWidths,
          width(icon),
          width(more),
          gapPx,
          strip.clientWidth,
          primaryCount,
        ),
      );
    };

    measure();
    const observer = new ResizeObserver(measure);
    observer.observe(strip);
    return () => observer.disconnect();
  }, [measurementKey]);

  const prefixCount = Math.min(layout.count, views.length);
  let visibleTabs = views.slice(0, prefixCount);
  const activeIndex = views.findIndex((view) => view.id === effectiveTab);
  if (activeIndex >= prefixCount) {
    // Keep the active view visible by replacing the last fitting tab after Overview.
    visibleTabs =
      visibleTabs.length === 1
        ? [...visibleTabs, views[activeIndex]]
        : [...visibleTabs.slice(0, -1), views[activeIndex]];
  }
  const visibleIds = new Set(visibleTabs.map((view) => view.id));
  const menuViews = views.filter((view) => !visibleIds.has(view.id));

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
    // `work-pane` re-tones this whole subtree to the cover (see index.css): the
    // pane is cut from the chrome rather than from the page, so in light mode
    // the bright thread is flanked by dark chrome and is plainly the subject.
    // The panel variant is a lifted sheet of its own; the overlay variant fills
    // a sheet that already has its own edges.
    <section
      className={cn(
        "work-pane flex h-full min-h-0 flex-col bg-pane",
        variant === "panel" && "work-sheet overflow-hidden rounded-xl border",
      )}
      aria-label="Work pane"
    >
      <header className="flex h-11 shrink-0 items-center justify-between gap-1 border-b px-1.5">
        <div ref={stripRef} className="relative flex min-w-0 flex-1 items-center gap-0.5">
          <div
            ref={measurementRef}
            className="pointer-events-none invisible absolute flex items-center gap-0.5"
            aria-hidden
          >
            {views.map((view) => (
              <Button
                key={`labeled-${view.id}`}
                variant="ghost"
                size="sm"
                className={TAB_CLASS_NAME}
                tabIndex={-1}
              >
                <view.icon />
                <span>{view.label}</span>
              </Button>
            ))}
            <Button variant="ghost" size="sm" className={TAB_CLASS_NAME} tabIndex={-1}>
              <PanelsTopLeft />
            </Button>
            <Button variant="ghost" size="sm" className={TAB_CLASS_NAME} tabIndex={-1}>
              <MoreHorizontal />
            </Button>
          </div>
          {visibleTabs.map((view) => (
            <PaneTab
              key={view.id}
              view={view}
              active={view.id === effectiveTab}
              iconOnly={layout.iconOnly}
              onSelect={onTabChange}
            />
          ))}
          {menuViews.length > 0 && (
            <MoreViewsMenu
              views={menuViews}
              activeTab={menuViews.some((view) => view.id === effectiveTab) ? effectiveTab : null}
              onSelect={onTabChange}
            />
          )}
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
