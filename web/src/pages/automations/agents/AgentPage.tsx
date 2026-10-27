import { useMemo } from "react";
import { Link, useParams } from "@tanstack/react-router";

import { EmptyState } from "@/components/empty-state";
import { SkeletonRows } from "@/components/skeleton-rows";
import { StatusDot } from "@/components/status-dot";
import { Button } from "@/components/ui/button";
import { Text } from "@/components/ui/text";
import { useRun, useRunList } from "@/hooks/useAutomationRuns";
import { useAutomations } from "@/hooks/useAutomations";
import { useInstance } from "@/hooks/useInstances";
import { useNow } from "@/hooks/useNow";
import { relativeAge } from "@/lib/relative-time";

import { ActivityTab } from "../activity/ActivityTab";
import { CloseAgentButton } from "./AgentsTable";
import { HandleChip } from "./HandleChip";
import { agentTitle } from "./handles";

// One scoped agent: what it is called, where it lives, the session it keeps,
// and its timeline (every run bound to it). Close while it is open. What it
// was started with sits behind a disclosure, since the key and the places
// already say what it is.

function useRouteParamId(): string | undefined {
  const params = useParams({ strict: false }) as { id?: string; _splat?: string };
  return params.id ?? params._splat?.split("/").filter(Boolean).at(-1);
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

export function AgentPage() {
  const id = useRouteParamId();
  const detail = useInstance(id);
  const instance = detail.data?.instance;
  const automations = useAutomations();
  const automation = automations.data?.automations.find(
    (summary) => summary.automation?.id === instance?.automationId,
  )?.automation;
  const runs = useRunList(instance?.automationId, { instanceId: instance?.id, limit: 1 });
  const latest = runs.data?.runs[0];
  const latestDetail = useRun(latest?.id);
  const now = useNow(10_000);
  const inputs = useMemo(() => inputsFrom(instance?.inputsJson ?? ""), [instance?.inputsJson]);
  const sessionId = latestDetail.data?.run?.sessionIds[0];
  const error = detail.error ?? automations.error ?? runs.error;

  if (error) {
    return <EmptyState tone="error">Couldn’t load this agent. {String(error)}</EmptyState>;
  }
  if (detail.isPending || automations.isPending || (instance && runs.isPending)) {
    return <SkeletonRows rows={6} columns={["minmax(0,1fr)"]} />;
  }
  if (!instance) {
    return <EmptyState>This agent was not found.</EmptyState>;
  }

  const handles = (detail.data?.handles ?? []).map((h) => h.handle);
  const title = agentTitle(instance, handles);
  const open = instance.status === "open";

  return (
    <div className="space-y-6">
      <nav className="text-xs text-muted-foreground" aria-label="Breadcrumb">
        {automation ? (
          <Link
            to="/automations/$id"
            params={{ id: automation.id }}
            search={{ tab: "agents" }}
            className="underline-offset-4 hover:underline"
          >
            {automation.name}
          </Link>
        ) : (
          "Unknown automation"
        )}{" "}
        › Agents
      </nav>

      <div className="flex flex-wrap items-center justify-between gap-x-6 gap-y-3">
        <div className="flex min-w-0 flex-wrap items-center gap-3">
          <StatusDot tone={open ? "nominal" : "muted"} size={10} label={instance.status} />
          <Text as="h1" variant="display" className="truncate">
            {title}
          </Text>
          <span className="rounded-full bg-secondary px-2 py-0.5 font-mono text-xs tabular-nums text-muted-foreground">
            {open ? `open ${relativeAge(instance.openedAt, now)}` : "closed"}
          </span>
          <span className="rounded-sm border bg-card px-2 py-0.5 font-mono text-xs text-muted-foreground">
            {instance.key}
          </span>
        </div>
        <div className="flex items-center gap-2">
          {sessionId && (
            <Button asChild size="sm">
              <Link to="/sessions/$id" params={{ id: sessionId }}>
                Open session ↗
              </Link>
            </Button>
          )}
          {open && <CloseAgentButton instance={instance} title={title} />}
        </div>
      </div>

      {handles.length > 0 && (
        <div className="flex flex-wrap items-center gap-1.5" aria-label="Where it lives">
          <span className="text-xs text-muted-foreground">Lives in</span>
          {handles.map((handle) => (
            <HandleChip key={handle} handle={handle} />
          ))}
        </div>
      )}

      {inputs.length > 0 && (
        <details className="rounded-lg border bg-card px-4 py-3">
          <summary className="cursor-pointer text-xs font-semibold">Started with</summary>
          <dl className="mt-3 space-y-2">
            {inputs.map(([key, value]) => (
              <div key={key}>
                <dt className="font-mono text-2xs text-muted-foreground">{key}</dt>
                <dd className="mt-0.5 break-words text-xs">
                  {typeof value === "string" ? value : JSON.stringify(value)}
                </dd>
              </div>
            ))}
          </dl>
        </details>
      )}

      <section aria-labelledby="agent-timeline-title">
        <h2 id="agent-timeline-title" className="mb-3 text-sm font-semibold">
          Timeline
        </h2>
        <ActivityTab automationId={instance.automationId} instanceId={instance.id} />
      </section>
    </div>
  );
}
