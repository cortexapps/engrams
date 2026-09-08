import { useMemo } from "react";
import { Link, useParams } from "@tanstack/react-router";
import { Clock3Icon, PlayIcon, PlugZapIcon, WebhookIcon, type LucideIcon } from "lucide-react";
import { toast } from "sonner";

import { EmptyState } from "@/components/empty-state";
import { SkeletonRows } from "@/components/skeleton-rows";
import { StatusDot } from "@/components/status-dot";
import {
  AlertDialog,
  AlertDialogAction,
  AlertDialogCancel,
  AlertDialogContent,
  AlertDialogDescription,
  AlertDialogFooter,
  AlertDialogHeader,
  AlertDialogTitle,
  AlertDialogTrigger,
} from "@/components/ui/alert-dialog";
import { Button } from "@/components/ui/button";
import { Text } from "@/components/ui/text";
import { useRun, useRunList } from "@/hooks/useAutomationRuns";
import { useAutomations } from "@/hooks/useAutomations";
import { useCloseInstance, useInstance } from "@/hooks/useInstances";
import { useNow } from "@/hooks/useNow";
import {
  entrypointIds,
  parseDefinition,
  projectEntrypoint,
  type AutomationDefinition,
  type TriggerSpec,
} from "@/lib/automation-blocks";
import { relativeAge } from "@/lib/relative-time";

import { ActivityTab } from "../activity/ActivityTab";
import { parseHandle } from "./handles";
import { humanizeWorkstreamName } from "./WorkstreamsTable";

const TRIGGER_ICON: Record<string, LucideIcon> = {
  cron: Clock3Icon,
  webhook: WebhookIcon,
  integration: PlugZapIcon,
  manual: PlayIcon,
};

function useRouteParamId(): string | undefined {
  const params = useParams({ strict: false }) as { id?: string; _splat?: string };
  return params.id ?? params._splat?.split("/").filter(Boolean).at(-1);
}

function triggerLabel(kind: string): string {
  if (kind === "cron") return "Schedule";
  if (kind === "integration") return "Integration event";
  return `${kind.charAt(0).toUpperCase()}${kind.slice(1).replaceAll("_", " ")}`;
}

function triggerScope(trigger: TriggerSpec): string {
  const values = [trigger["eventKeys"], trigger["events"]]
    .filter(Array.isArray)
    .flat() as unknown[];
  const event = typeof trigger["event"] === "string" ? [trigger["event"]] : [];
  const scopes = [...values, ...event].filter(
    (value): value is string => typeof value === "string",
  );
  return scopes.length > 0 ? scopes.join(", ") : "any event";
}

function pointY(index: number, count: number): number {
  if (count <= 1) return 111;
  return 28 + (index * 166) / (count - 1);
}

function RoutingMap({
  definition,
  workstreamKey,
  handles,
  runs,
  openedAt,
  now,
}: {
  definition: AutomationDefinition;
  workstreamKey: string;
  handles: readonly { handle: string }[];
  runs: number;
  openedAt: string;
  now: number;
}) {
  const entries = entrypointIds(definition).map((id) => ({
    id,
    trigger: projectEntrypoint(definition, id).trigger,
  }));
  return (
    <section aria-labelledby="routing-map-title">
      <h2 id="routing-map-title" className="mb-3 text-sm font-semibold">
        Routing map
      </h2>
      <div className="overflow-x-auto rounded-lg border bg-card">
        <div className="relative h-[222px] min-w-[860px] overflow-hidden">
          <div
            className="pointer-events-none absolute inset-0 opacity-60"
            style={{
              backgroundImage:
                "radial-gradient(color-mix(in oklch, var(--color-foreground) 8%, transparent) 1px, transparent 1px)",
              backgroundSize: "20px 20px",
              backgroundPosition: "-1px -1px",
            }}
          />
          <div className="absolute inset-x-6 top-3 grid grid-cols-3 text-2xs font-semibold text-muted-foreground">
            <span>Gets in through</span>
            <span className="text-center">Workstream</span>
            <span className="text-right">Owns</span>
          </div>
          <svg
            className="pointer-events-none absolute inset-0 size-full"
            viewBox="0 0 1000 222"
            preserveAspectRatio="none"
            aria-hidden
          >
            {entries.map((entry, index) => (
              <path
                key={`in-${entry.id}`}
                d={`M 270 ${pointY(index, entries.length)} C 350 ${pointY(index, entries.length)}, 360 111, 420 111`}
                fill="none"
                stroke="color-mix(in oklch, var(--color-foreground) 40%, transparent)"
                strokeWidth="1.5"
              />
            ))}
            {handles.map((handle, index) => (
              <path
                key={`out-${handle.handle}`}
                d={`M 580 111 C 650 111, 670 ${pointY(index, handles.length)}, 730 ${pointY(index, handles.length)}`}
                fill="none"
                stroke="color-mix(in oklch, var(--color-foreground) 40%, transparent)"
                strokeWidth="1.5"
              />
            ))}
          </svg>

          {entries.map((entry, index) => {
            const Icon = TRIGGER_ICON[entry.trigger.kind] ?? PlugZapIcon;
            return (
              <div
                key={entry.id}
                className="absolute left-6 flex h-[38px] w-[232px] items-center gap-2.5 rounded-lg border bg-card px-3 shadow-[0_1px_2px_oklch(0_0_0/.06)]"
                style={{ top: pointY(index, entries.length) - 19 }}
              >
                <Icon className="size-3.5 shrink-0 text-muted-foreground" aria-hidden />
                <span className="min-w-0">
                  <span className="block truncate text-xs font-semibold">
                    {triggerLabel(entry.trigger.kind)}
                  </span>
                  <span className="block truncate font-mono text-2xs text-muted-foreground">
                    {triggerScope(entry.trigger)}
                  </span>
                </span>
              </div>
            );
          })}

          <div className="absolute top-[67px] left-1/2 flex h-[88px] w-[220px] -translate-x-1/2 flex-col justify-center rounded-lg bg-sidebar px-4 text-sidebar-foreground shadow-[0_1px_2px_oklch(0_0_0/.06)]">
            <span className="flex items-center gap-2 text-sm font-semibold">
              <span className="size-[7px] shrink-0 rounded-full bg-sidebar-primary" aria-hidden />
              <span className="truncate">{workstreamKey}</span>
            </span>
            <span className="mt-1 text-xs opacity-75">
              <span className="font-mono tabular-nums">{runs}</span> runs
            </span>
            <span className="mt-0.5 font-mono text-2xs opacity-60">
              opened {relativeAge(openedAt, now)}
            </span>
          </div>

          {handles.map((item, index) => {
            const handle = parseHandle(item.handle);
            const content = (
              <>
                <span className="font-mono text-2xs text-muted-foreground">{handle.provider}</span>
                <span className="truncate text-xs">{handle.label}</span>
                {handle.href && <span aria-hidden>↗</span>}
              </>
            );
            const className =
              "absolute right-6 flex h-[38px] w-[212px] items-center gap-2 rounded-lg border bg-card px-3 shadow-[0_1px_2px_oklch(0_0_0/.06)]";
            const style = { top: pointY(index, handles.length) - 19 };
            return handle.href ? (
              <a
                key={item.handle}
                className={className}
                style={style}
                href={handle.href}
                target="_blank"
                rel="noreferrer"
              >
                {content}
              </a>
            ) : (
              <div key={item.handle} className={className} style={style}>
                {content}
              </div>
            );
          })}
        </div>
      </div>
    </section>
  );
}

function inputsFrom(json: string): [string, unknown][] {
  try {
    const value: unknown = JSON.parse(json);
    return typeof value === "object" && value !== null && !Array.isArray(value)
      ? Object.entries(value)
      : [];
  } catch {
    return [];
  }
}

export function WorkstreamPage() {
  const id = useRouteParamId();
  const detail = useInstance(id);
  const instance = detail.data?.instance;
  const automations = useAutomations();
  const automation = automations.data?.automations.find(
    (summary) => summary.automation?.id === instance?.automationId,
  )?.automation;
  const runs = useRunList(instance?.automationId, {
    instanceId: instance?.id,
    limit: 50,
  });
  const latest = runs.data?.runs[0];
  const latestDetail = useRun(latest?.id);
  const close = useCloseInstance();
  const now = useNow(10_000);
  const inputs = useMemo(() => inputsFrom(instance?.inputsJson ?? ""), [instance?.inputsJson]);
  const taskId = latestDetail.data?.run?.sessionIds[0];
  const error = detail.error ?? automations.error ?? runs.error;

  if (error) {
    return <EmptyState tone="error">Couldn’t load this workstream. {String(error)}</EmptyState>;
  }
  if (detail.isPending || automations.isPending || (instance && runs.isPending)) {
    return <SkeletonRows rows={6} columns={["minmax(0,1fr)"]} />;
  }
  if (!instance) {
    return <EmptyState>This workstream was not found.</EmptyState>;
  }

  const definition = parseDefinition(automation?.version?.definitionJson);
  const handles = detail.data?.handles ?? [];
  const open = instance.status === "open";

  return (
    <div className="space-y-6">
      <nav className="text-xs text-muted-foreground" aria-label="Breadcrumb">
        <Link to="/automations/workstreams" className="underline-offset-4 hover:underline">
          Workstreams
        </Link>{" "}
        › {automation?.name ?? "Unknown automation"}
      </nav>

      <div className="flex flex-wrap items-center justify-between gap-x-6 gap-y-3">
        <div className="flex min-w-0 flex-wrap items-center gap-3">
          <StatusDot tone={open ? "nominal" : "muted"} size={10} label={instance.status} />
          <Text as="h1" variant="display" className="truncate">
            {humanizeWorkstreamName(instance.key)}
          </Text>
          <span className="rounded-full bg-secondary px-2 py-0.5 font-mono text-xs tabular-nums text-muted-foreground">
            {open ? `open ${relativeAge(instance.openedAt, now)}` : "closed"}
          </span>
          <span className="rounded-sm border bg-card px-2 py-0.5 font-mono text-xs text-muted-foreground">
            {instance.key}
          </span>
        </div>
        <div className="flex items-center gap-2">
          {taskId && (
            <Button asChild size="sm">
              <Link to="/sessions/$id" params={{ id: taskId }}>
                Open task ↗
              </Link>
            </Button>
          )}
          {open && (
            <AlertDialog>
              <AlertDialogTrigger asChild>
                <Button variant="outline" size="sm" disabled={close.isPending}>
                  Close workstream
                </Button>
              </AlertDialogTrigger>
              <AlertDialogContent>
                <AlertDialogHeader>
                  <AlertDialogTitle>Close {humanizeWorkstreamName(instance.key)}?</AlertDialogTitle>
                  <AlertDialogDescription>
                    Matching events stop firing for this workstream. Its activity stays available.
                  </AlertDialogDescription>
                </AlertDialogHeader>
                <AlertDialogFooter>
                  <AlertDialogCancel>Cancel</AlertDialogCancel>
                  <AlertDialogAction
                    onClick={async () => {
                      try {
                        await close.mutateAsync({ id: instance.id });
                        toast.success(`Closed ${instance.key}`);
                      } catch (closeError) {
                        toast.error(
                          closeError instanceof Error ? closeError.message : String(closeError),
                        );
                      }
                    }}
                  >
                    Close workstream
                  </AlertDialogAction>
                </AlertDialogFooter>
              </AlertDialogContent>
            </AlertDialog>
          )}
        </div>
      </div>

      <RoutingMap
        definition={definition}
        workstreamKey={instance.key}
        handles={handles}
        runs={runs.data?.runs.length ?? 0}
        openedAt={instance.openedAt}
        now={now}
      />

      <div className="grid gap-6 lg:grid-cols-[minmax(0,1fr)_280px]">
        <section aria-labelledby="workstream-timeline-title">
          <h2 id="workstream-timeline-title" className="mb-3 text-sm font-semibold">
            Timeline
          </h2>
          <ActivityTab automationId={instance.automationId} instanceId={instance.id} />
        </section>
        <aside className="space-y-3" aria-label="Memory">
          <h2 className="text-sm font-semibold">Memory</h2>
          <div className="rounded-lg border bg-card px-4 py-3.5">
            <h3 className="mb-3 text-xs font-semibold">Opened with</h3>
            {inputs.length > 0 ? (
              <dl className="space-y-2">
                {inputs.map(([key, value]) => (
                  <div key={key}>
                    <dt className="font-mono text-2xs text-muted-foreground">{key}</dt>
                    <dd className="mt-0.5 break-words text-xs">
                      {typeof value === "string" ? value : JSON.stringify(value)}
                    </dd>
                  </div>
                ))}
              </dl>
            ) : (
              <p className="text-xs text-muted-foreground">No opening inputs.</p>
            )}
          </div>
          <div className="rounded-lg border border-dashed px-4 py-3.5">
            <h3 className="mb-1 text-xs font-semibold">When it closes</h3>
            <p className="text-xs text-muted-foreground">
              Events that match this workstream stop firing; closed workstreams stay in Activity.
            </p>
          </div>
        </aside>
      </div>
    </div>
  );
}
