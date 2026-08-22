/** Settings → Automations: the list (ADR 0119 phase 3.2).
 *
 * One row per automation — name, server-rendered trigger summary, enabled
 * switch, last-run status dot + relative time, a seven-day sparkline, and a
 * built-in pill. Built-ins pin to the top. The editor (3.3), inputs (3.6),
 * and runs (3.7) live behind the row links.
 */
import { Link, useNavigate } from "@tanstack/react-router";
import { toast } from "sonner";
import {
  ArchiveIcon,
  Clock3Icon,
  CopyPlusIcon,
  LockIcon,
  PlusIcon,
  RadioTowerIcon,
  WebhookIcon,
} from "lucide-react";

import type { AutomationSummary } from "@/gen/engram/app/v1/automation_pb";
import {
  useArchiveAutomation,
  useAutomations,
  useBuiltinAutomation,
  useDuplicateAutomation,
  useSetAutomationEnabled,
} from "@/hooks/useAutomations";
import {
  relativeTime,
  runStatusTone,
  toneDotClass,
  automationStatusLabel,
  type RunTone,
} from "@/lib/automations";
import { errorMessage } from "@/lib/errors";
import { PageHeading } from "@/components/page-heading";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
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
import { Switch } from "@/components/ui/switch";
import { Sparkline } from "./Sparkline";
import { WebhookRegistrationsPanel } from "./WebhookRegistrationsPanel";

export const PR_REVIEW_BUILTIN_KEY = "pr_review";

/** Built-ins first (stable by name), then the rest by name. */
export function orderAutomations(items: AutomationSummary[]): AutomationSummary[] {
  return [...items].sort((a, b) => {
    const ak = a.automation?.kind === "builtin" ? 0 : 1;
    const bk = b.automation?.kind === "builtin" ? 0 : 1;
    if (ak !== bk) return ak - bk;
    return (a.automation?.name ?? "").localeCompare(b.automation?.name ?? "");
  });
}

function StatusDot({ status, at }: { status: string | undefined; at: string | undefined }) {
  const tone: RunTone = status ? runStatusTone(status) : "muted";
  const text = status ? `${automationStatusLabel(status)} · ${relativeTime(at)}` : "never run";
  return (
    <span className="inline-flex items-center gap-1.5 text-xs text-muted-foreground">
      <span
        data-testid="status-dot"
        data-tone={tone}
        className={`inline-block size-2 rounded-full ${toneDotClass(tone)}`}
        aria-hidden
      />
      <span>{text}</span>
    </span>
  );
}

function TriggerIcon({ summary }: { summary: string }) {
  if (summary.startsWith("Every") || summary.startsWith("Cron")) {
    return <Clock3Icon className="size-3.5 shrink-0" />;
  }
  if (summary.startsWith("Webhook")) return <WebhookIcon className="size-3.5 shrink-0" />;
  return <RadioTowerIcon className="size-3.5 shrink-0" />;
}

function AutomationRow({ summary }: { summary: AutomationSummary }) {
  const automation = summary.automation;
  const setEnabled = useSetAutomationEnabled();
  const archive = useArchiveAutomation();
  if (!automation) return null;
  const builtin = automation.kind === "builtin";
  const trigger = summary.triggerSummary || "Trigger not configured";

  const onEnabledChange = async (enabled: boolean) => {
    try {
      await setEnabled.mutateAsync({ id: automation.id, enabled });
      toast.success(enabled ? "Automation enabled" : "Automation paused");
    } catch (error) {
      toast.error(errorMessage(error));
    }
  };

  const onArchive = async () => {
    try {
      await archive.mutateAsync({ id: automation.id });
      toast.success("Automation archived");
    } catch (error) {
      toast.error(errorMessage(error));
    }
  };

  return (
    <div
      data-testid="automation-row"
      data-kind={automation.kind}
      className="grid gap-4 rounded-lg border bg-card p-4 shadow-xs md:grid-cols-[minmax(0,1fr)_auto_auto] md:items-center"
    >
      <Link
        to="/settings/automations/$id"
        params={{ id: automation.id }}
        search={{ tab: "build" }}
        className="group min-w-0 outline-none focus-visible:ring-2 focus-visible:ring-ring/50"
      >
        <div className="flex flex-wrap items-center gap-2">
          <span className="text-base font-semibold group-hover:underline">{automation.name}</span>
          {builtin && (
            <Badge variant="secondary" className="gap-1">
              <LockIcon className="size-3" aria-hidden />
              built-in
            </Badge>
          )}
          <StatusDot status={summary.lastRun?.status} at={summary.lastRun?.startedAt} />
        </div>
        {automation.description && (
          <p className="mt-1 truncate text-sm text-muted-foreground">{automation.description}</p>
        )}
        <div className="mt-3 flex flex-wrap items-center gap-x-4 gap-y-1 text-xs text-muted-foreground">
          <span className="inline-flex min-w-0 items-center gap-1.5 font-mono">
            <TriggerIcon summary={trigger} />
            <span className="truncate">{trigger}</span>
          </span>
          {automation.nextFireAt && <span>Next: {relativeTime(automation.nextFireAt)}</span>}
        </div>
      </Link>
      <Link
        to="/settings/automations/$id"
        params={{ id: automation.id }}
        search={{ tab: "runs" }}
        className="justify-self-start rounded-md p-1 outline-none hover:bg-muted focus-visible:ring-2 focus-visible:ring-ring/50 md:justify-self-end"
        aria-label={`Runs for ${automation.name}`}
      >
        <Sparkline days={summary.runs7d} label={`${automation.name}: runs over the last 7 days`} />
      </Link>
      <div className="flex items-center justify-between gap-2 md:justify-end">
        <label className="inline-flex items-center gap-2 text-sm">
          <Switch
            aria-label={`${automation.enabled ? "Disable" : "Enable"} ${automation.name}`}
            checked={automation.enabled}
            disabled={setEnabled.isPending}
            onCheckedChange={onEnabledChange}
          />
          {automation.enabled ? "Enabled" : "Paused"}
        </label>
        {!builtin && (
          <AlertDialog>
            <AlertDialogTrigger asChild>
              <Button variant="ghost" size="sm">
                <ArchiveIcon className="size-4" />
                Archive
              </Button>
            </AlertDialogTrigger>
            <AlertDialogContent>
              <AlertDialogHeader>
                <AlertDialogTitle>Archive “{automation.name}”?</AlertDialogTitle>
                <AlertDialogDescription>
                  It will stop receiving events or scheduled fires. Run history stays available.
                </AlertDialogDescription>
              </AlertDialogHeader>
              <AlertDialogFooter>
                <AlertDialogCancel>Cancel</AlertDialogCancel>
                <AlertDialogAction onClick={onArchive}>Archive automation</AlertDialogAction>
              </AlertDialogFooter>
            </AlertDialogContent>
          </AlertDialog>
        )}
      </div>
    </div>
  );
}

function DuplicatePrReviewButton() {
  const builtin = useBuiltinAutomation(PR_REVIEW_BUILTIN_KEY);
  const duplicate = useDuplicateAutomation();
  const navigate = useNavigate();
  const source = builtin.data?.automation;
  if (!source) return null;
  const onClick = async () => {
    try {
      const result = await duplicate.mutateAsync({ automationId: source.id });
      toast.success("Copied “PR review” — edit the copy freely");
      const id = result.automation?.id;
      if (id) {
        await navigate({
          to: "/settings/automations/$id",
          params: { id },
          search: { tab: "build" },
        });
      }
    } catch (error) {
      toast.error(errorMessage(error));
    }
  };
  return (
    <Button variant="outline" onClick={onClick} disabled={duplicate.isPending}>
      <CopyPlusIcon className="size-3.5" />
      Duplicate “PR review”
    </Button>
  );
}

export function AutomationsList() {
  const automations = useAutomations();
  const rows = orderAutomations(automations.data?.automations ?? []);

  return (
    <div className="space-y-8">
      <PageHeading
        title="Automations"
        count={rows.length}
        actions={
          <Button asChild size="sm">
            <Link to="/settings/automations/new">
              <PlusIcon className="size-3.5" />
              New automation
            </Link>
          </Button>
        }
      />

      <section className="space-y-3" aria-labelledby="automation-list-heading">
        <h2 id="automation-list-heading" className="sr-only">
          Automations
        </h2>
        {automations.isPending && <p className="text-sm text-muted-foreground">Loading…</p>}
        {automations.error && (
          <p className="text-sm text-destructive">{errorMessage(automations.error)}</p>
        )}
        {!automations.isPending && !automations.error && rows.length === 0 && (
          <div className="rounded-lg border border-dashed p-8 text-center">
            <p className="text-sm text-muted-foreground">
              No automations yet. Start from the built-in review pipeline or from scratch.
            </p>
            <div className="mt-4 flex flex-wrap justify-center gap-2">
              <DuplicatePrReviewButton />
              <Button asChild>
                <Link to="/settings/automations/new">New automation</Link>
              </Button>
            </div>
          </div>
        )}
        <div className="space-y-3">
          {rows.map((summary) => (
            <AutomationRow key={summary.automation?.id} summary={summary} />
          ))}
        </div>
      </section>

      <WebhookRegistrationsPanel />
    </div>
  );
}
