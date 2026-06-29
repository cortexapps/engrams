import { useParams } from "@tanstack/react-router";
import { Fragment, useEffect, useState, type ReactNode } from "react";
import { useSession } from "../hooks/useSessions";
import { useSessionEvents } from "../hooks/useSessionEvents";
import { StatusGlyph } from "../components/Glyph";
import { SessionThread } from "../components/session-thread/SessionThread";
import { TabRow } from "../components/TabRow";
import { PageHeading } from "../components/page-heading";
import { TerminalPane } from "../components/TerminalPane";
import { BrowserPane } from "../components/BrowserPane";
import { statusLabel } from "./sessions/session-format";
import { useTasks } from "../hooks/useTasks";
import { ProfileChip } from "../components/profiles/ProfileChip";
import {
  DiagnosticsDrawer,
  DurabilityReadout,
  useDurabilitySummary,
  type DurabilitySummary,
} from "../components/SessionDiagnostics";
import { Separator } from "@/components/ui/separator";
import type { IndexedEvent, Session, ProfileSnapshotView } from "../lib/types";

type ViewTab = "transcript" | "shell" | "browser" | "raw";

// BROWSER is conditional on the session's capability, so the tab list is built
// per-render (below) rather than as a module constant. RAW always trails.
const BASE_TABS = [
  { id: "transcript" as const, label: "TRANSCRIPT" },
  { id: "shell" as const, label: "SHELL" },
];
const RAW_TAB = { id: "raw" as const, label: "RAW" };

export function SessionDetail() {
  const { id } = useParams({ from: "/_app/sessions/$id" });
  const { data: session } = useSession(id);
  const { data: tasksData } = useTasks();
  const profileSnap =
    tasksData?.tasks.flatMap((t) => t.sessions).find((r) => r.sessionId === id)?.profile ?? null;
  // Normalize the embedded snapshot to the UI view once. Point-in-time by
  // design (ADR 0052) — what this session launched from, not the profile's
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
  const [tab, setTab] = useState<ViewTab>("transcript");
  // Once the user opens the SHELL tab, keep TerminalPane mounted for
  // the lifetime of this page. Switching back to TRANSCRIPT/RAW just
  // hides it via CSS — no remount, no fresh canvas, no replayed
  // ghostty-web WASM state. Lazy-mount avoids opening a ttyd
  // connection for users who never visit the SHELL tab.
  const [shellEverActive, setShellEverActive] = useState(false);
  useEffect(() => {
    if (tab === "shell") setShellEverActive(true);
  }, [tab]);

  // The in-guest browser (Xvfb + VNC, ADR 0064) is an optional capability,
  // present iff the session's profile selected the `browser` skill bundle. We
  // read that straight off the profile snapshot the masthead already shows —
  // the same source as the ProfileChip — so there's no separate capability
  // fetch (ADR 0064 as-built: skills ride ProfileSnapshot, not a REST endpoint).
  const browserEnabled = (profile?.skills ?? []).includes("browser");
  const TABS = browserEnabled
    ? [...BASE_TABS, { id: "browser" as const, label: "BROWSER" }, RAW_TAB]
    : [...BASE_TABS, RAW_TAB];

  // Same lazy keep-mounted contract as SHELL: open the noVNC RFB connection
  // only once the user visits BROWSER, then hold it across tab switches.
  const [browserEverActive, setBrowserEverActive] = useState(false);
  useEffect(() => {
    if (tab === "browser") setBrowserEverActive(true);
  }, [tab]);

  // If the capability flips off (e.g. an admin removes the `browser` skill from
  // the profile and useTasks refetches the snapshot) while the user is on the
  // now-absent tab, fall back to TRANSCRIPT so we never render an orphaned tab.
  useEffect(() => {
    if (tab === "browser" && !browserEnabled) setTab("transcript");
  }, [tab, browserEnabled]);

  return (
    // The work surface IS the page — no standing instrument rail. A developer
    // glances at status, the profile this launched from, and a calm "is my work
    // safe" telltale, all in the masthead. The operator/forensic detail (COW
    // gauges, the recovery ladder, admin teleport/pause) lives one click away in
    // the Diagnostics drawer, which also gives phones this data for the first
    // time (the old rail was desktop-only). (ADR 0052 follow-up)
    <div className="flex min-h-0 flex-1 flex-col overflow-hidden">
      <div className="shrink-0 px-6 pt-6">
        {/* No `← back` link — the persistent sessions rail (left) keeps the full
            list in view and highlights this session (ADR 0029). Diagnostics
            opens from the masthead actions, on the title's baseline. */}
        <PageHeading
          eyebrow="task"
          title={id}
          titleVariant="mono"
          showRule={false}
          actions={
            session && (
              <DiagnosticsDrawer session={session} sessionId={id} eventCount={events.length} />
            )
          }
        />

        {session && <SessionVitals session={session} profile={profile} durability={durability} />}

        <div className="mt-5">
          <TabRow tabs={TABS} active={tab} onChange={setTab} />
        </div>
      </div>

      <div className="min-h-0 flex-1 overflow-hidden">
        {tab === "transcript" && (
          <SessionThread
            sessionId={id}
            events={events}
            status={session?.status}
            streamingText={streamingText}
          />
        )}

        {/* Mount TerminalPane once and keep it mounted across tab switches.
            Display:none preserves canvas + WS + ghostty-web WASM grid;
            remounting would restart bash and (because ghostty-web's render loop
            has cross-mount quirks) ghost the previous session's output into the
            fresh canvas. */}
        {shellEverActive && (
          <div className="h-full" style={{ display: tab === "shell" ? "block" : "none" }}>
            <TerminalPane sessionId={id} />
          </div>
        )}

        {/* Same keep-mounted contract as SHELL above: mount BrowserPane once
            (after the first visit) and hide it with display:none on tab
            switches, so the noVNC RFB connection survives without a remount. */}
        {browserEnabled && browserEverActive && (
          <div className="h-full" style={{ display: tab === "browser" ? "block" : "none" }}>
            <BrowserPane sessionId={id} />
          </div>
        )}

        {tab === "raw" && <RawEvents events={events} />}
      </div>
    </div>
  );
}

// The masthead vitals strip — the only session metadata a developer needs at a
// glance: lifecycle status (glyph + word), the profile this launched from
// (ADR 0052, the dense inline chip with its image on hover), and the calm
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

function RawEvents({ events }: { events: IndexedEvent[] }) {
  return (
    <div className="h-full space-y-0.5 overflow-auto px-6 py-4 font-mono text-[0.74rem] text-muted-foreground">
      {events.map((e) => (
        <div
          key={e.idx}
          data-testid="event-row"
          className="grid items-baseline gap-3"
          style={{ gridTemplateColumns: "4ch min-content 1fr" }}
        >
          <span className="tabular-nums text-muted-foreground/70">{e.idx}</span>
          <span className="text-[0.66rem] uppercase tracking-[0.12em]">{e.event.type}</span>
          <span className="truncate text-foreground/80" title={JSON.stringify(e.event)}>
            {summarizeRaw(e.event)}
          </span>
        </div>
      ))}
    </div>
  );
}

function summarizeRaw(ev: unknown): string {
  // Compact one-liner of any event body — used only in the raw tab.
  const obj = ev as Record<string, unknown>;
  const fields = Object.entries(obj)
    .filter(([k]) => k !== "type" && k !== "at")
    .map(([k, v]) => `${k}=${JSON.stringify(v)}`)
    .join(" ");
  return fields;
}
