import { useEffect, useMemo, useState, type ComponentType } from "react";
import {
  Activity,
  Code2,
  GitPullRequestArrow,
  Globe,
  ListTodo,
  Map,
  Maximize2,
  Minimize2,
  PanelRightClose,
  SquareTerminal,
  X,
} from "lucide-react";

import { TerminalPane } from "./TerminalPane";
import { BrowserPane } from "./BrowserPane";
import { IdePane } from "./IdePane";
import { DiagnosticsPanel } from "./SessionDiagnostics";
import { PlanPane } from "./PlanPane";
import { TasksPane } from "./TasksPane";
import { sessionHasAgentTasks } from "./session-thread/agentTasks";
import { SideEffectsPanel } from "./SideEffectsPanel";
import { Button } from "@/components/ui/button";
import { textVariants } from "@/components/ui/text";
import { cn } from "@/lib/utils";
import type { IndexedEvent, Session } from "../lib/types";

// The Devin-style work pane: the shell + browser, lifted out of the transcript
// into a companion surface so the conversation and the live view sit side by
// side instead of hiding each other. Its own header carries the tab strip and
// the window controls; the body keeps BOTH panes mounted (display:none swap) so
// the ttyd / VNC sockets survive tab switches — the same keep-mounted contract
// the panes rely on, now owned here.
//
// It renders identically inside the desktop resizable panel (`variant="panel"`,
// with expand-to-fill) and the mobile overlay sheet (`variant="overlay"`, where
// collapse means "close the sheet").

export type PaneTabId =
  | "shell"
  | "browser"
  | "ide"
  | "plan"
  | "tasks"
  | "side-effects"
  | "diagnostics";

interface PaneTabDef {
  id: PaneTabId;
  label: string;
  icon: ComponentType<{ className?: string }>;
}

const SHELL_TAB: PaneTabDef = { id: "shell", label: "Shell", icon: SquareTerminal };
const BROWSER_TAB: PaneTabDef = { id: "browser", label: "Browser", icon: Globe };
const IDE_TAB: PaneTabDef = { id: "ide", label: "IDE", icon: Code2 };
const SIDE_EFFECTS_TAB: PaneTabDef = {
  id: "side-effects",
  label: "Side effects",
  icon: GitPullRequestArrow,
};
const DIAGNOSTICS_TAB: PaneTabDef = { id: "diagnostics", label: "Diagnostics", icon: Activity };
const PLAN_TAB: PaneTabDef = { id: "plan", label: "Plan", icon: Map };
const TASKS_TAB: PaneTabDef = { id: "tasks", label: "Tasks", icon: ListTodo };

export interface WorkPaneProps {
  sessionId: string;
  /** Owning task resolved by SessionDetail; null for orphan sessions. */
  taskId: string | null;
  /** The session row, for the Diagnostics view. May be undefined while loading. */
  session: Session | undefined;
  /** The event stream, for the Diagnostics raw log. */
  events: IndexedEvent[];
  /**
   * Whether the pane is actually open (visible). The pane stays MOUNTED while
   * collapsed so the shell/browser sockets survive — but nothing inside is
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
  // ADR 0107: the Plan tab appears once the session has proposed a plan —
  // derived here (not threaded from SessionDetail) so the expanded strip and
  // the collapsed edge rail can never disagree again.
  const hasPlan = useMemo(
    () =>
      events.some(
        (e) => e.event.type === "tool_call_requested" && e.event.name === "exit_plan_mode",
      ),
    [events],
  );
  // The Tasks tab appears once the agent has created a task — the same
  // event-derived gating as the Plan tab.
  const hasTasks = useMemo(() => sessionHasAgentTasks(events), [events]);
  const tabs = [
    SHELL_TAB,
    ...(browserEnabled ? [BROWSER_TAB] : []),
    ...(ideEnabled ? [IDE_TAB] : []),
    ...(hasPlan ? [PLAN_TAB] : []),
    ...(hasTasks ? [TASKS_TAB] : []),
    SIDE_EFFECTS_TAB,
    DIAGNOSTICS_TAB,
  ];

  // Lazy-mount each live pane the first time it's viewed *while the pane is
  // open*, then keep it mounted for the life of the WorkPane (hidden via
  // display:none on tab switch / collapse) so its socket survives. Gating on
  // `open` is what keeps a collapsed pane from silently opening a shell.
  const [shellEverActive, setShellEverActive] = useState(false);
  const [browserEverActive, setBrowserEverActive] = useState(false);
  const [ideEverActive, setIdeEverActive] = useState(false);
  useEffect(() => {
    if (!open) return;
    if (tab === "shell") setShellEverActive(true);
    if (tab === "browser") setBrowserEverActive(true);
    if (tab === "ide") setIdeEverActive(true);
  }, [open, tab]);

  return (
    <section className="flex h-full min-h-0 flex-col bg-background" aria-label="Work pane">
      <header className="flex h-11 shrink-0 items-stretch justify-between gap-2 border-b pr-1.5 pl-3">
        <div role="tablist" aria-label="Work pane views" className="flex items-stretch gap-4">
          {tabs.map((t) => {
            const isActive = t.id === tab;
            return (
              <button
                key={t.id}
                type="button"
                role="tab"
                aria-selected={isActive}
                data-testid={`pane-tab-${t.id}`}
                onClick={() => onTabChange(t.id)}
                className={cn(
                  "-mb-px flex items-center gap-1.5 border-b-2 transition-colors",
                  isActive
                    ? "border-primary text-foreground"
                    : "border-transparent text-muted-foreground hover:text-foreground",
                )}
              >
                <t.icon className="size-3.5" />
                <span className={cn(textVariants({ variant: "label" }), "text-[0.66rem]")}>
                  {t.label}
                </span>
              </button>
            );
          })}
        </div>

        <div className="flex items-center gap-0.5 self-center">
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
          <div className="absolute inset-0" style={{ display: tab === "shell" ? "block" : "none" }}>
            <TerminalPane sessionId={sessionId} />
          </div>
        )}
        {browserEnabled && browserEverActive && (
          <div
            className="absolute inset-0"
            style={{ display: tab === "browser" ? "block" : "none" }}
          >
            <BrowserPane sessionId={sessionId} />
          </div>
        )}
        {ideEnabled && ideEverActive && (
          <div className="absolute inset-0" style={{ display: tab === "ide" ? "block" : "none" }}>
            <IdePane sessionId={sessionId} />
          </div>
        )}
        {/* Diagnostics holds no live socket, so it mounts only while open + on
            its tab — no keep-mounted contract to honour, and no polling while
            the pane is collapsed. */}
        {open && tab === "diagnostics" && (
          <div className="absolute inset-0 overflow-hidden">
            <DiagnosticsPanel session={session} sessionId={sessionId} events={events} />
          </div>
        )}
        {/* ADR 0107: read-only plan reading surface — no socket, mount on
            view only (the Diagnostics contract). */}
        {open && tab === "plan" && (
          <div className="absolute inset-0 overflow-hidden">
            <PlanPane events={events} />
          </div>
        )}
        {/* Agent task checklist — no socket, mount on view only (the
            Diagnostics contract). */}
        {open && tab === "tasks" && (
          <div className="absolute inset-0 overflow-hidden">
            <TasksPane events={events} />
          </div>
        )}
        {open && tab === "side-effects" && (
          <div className="absolute inset-0 overflow-hidden">
            <SideEffectsPanel taskId={taskId} sessionId={sessionId} />
          </div>
        )}
      </div>
    </section>
  );
}
