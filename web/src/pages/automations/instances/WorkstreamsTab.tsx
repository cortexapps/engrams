/** The Workstreams tab (ADR 0120 instances — "instance" never appears in
 * the UI). One row per workstream, labeled by its rendered key; expanding a
 * row shows the kickoff input snapshot, the handles it owns (its routing
 * history), and its runs. Kickoff opens-or-joins by key through RunNow;
 * the recent-drops list answers "why didn't my automation fire".
 */

import { useMemo, useState } from "react";
import { ChevronRightIcon, PlusIcon } from "lucide-react";
import { toast } from "sonner";

import { EmptyState } from "@/components/empty-state";
import type { AutomationInstance } from "@/gen/engram/app/v1/automation_pb";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Field, FieldLabel } from "@/components/ui/field";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Skeleton } from "@/components/ui/skeleton";
import { Switch } from "@/components/ui/switch";
import { useRunNow } from "@/hooks/useAutomations";
import {
  useCloseInstance,
  useInstance,
  useInstanceList,
  useInvalidateInstances,
  useRecentDrops,
} from "@/hooks/useInstances";
import {
  buildInputsPayload,
  resolveInputValues,
  validateInputs,
  type InputFieldError,
  type InputFieldSpec,
  type InputValues,
} from "@/lib/automation-inputs";
import { relativeTime } from "@/lib/relative-time";
import { cn } from "@/lib/utils";

import { InputsForm } from "../inputs/InputsForm";
import { RunList, RunStatusDot } from "../runs/RunsTab";

export interface WorkstreamsTabProps {
  automationId: string;
  /** The current version's input schema (the kickoff form). */
  inputsSchema: InputFieldSpec[];
  /** The automation row's values (raw parse) — the DEFAULTS for new
   * workstreams; resolveInputValues normalizes any shape. */
  defaultInputs: unknown;
  /** Injected by tests; defaults to the wall clock. */
  now?: () => number;
}

const DROP_REASON_LABEL: Record<string, string> = {
  closed_instance: "workstream closed",
  no_handle_match: "no workstream owns this",
  no_open_instance: "no open workstream",
};

export function WorkstreamsTab({
  automationId,
  inputsSchema,
  defaultInputs,
  now = Date.now,
}: WorkstreamsTabProps) {
  const [includeClosed, setIncludeClosed] = useState(false);
  const [kickoffOpen, setKickoffOpen] = useState(false);
  const [expanded, setExpanded] = useState<string | null>(null);
  const list = useInstanceList(automationId, { includeClosed: true });
  const drops = useRecentDrops(automationId);
  const tick = now();

  const rows = (list.data?.instances ?? []).filter(
    (instance) => includeClosed || instance.status === "open",
  );

  return (
    <section className="flex flex-col gap-3" aria-label="Workstreams">
      <div className="flex items-center justify-between">
        <p className="text-muted-foreground text-sm">
          {list.data
            ? `${list.data.instances.filter((i) => i.status === "open").length} open workstreams`
            : "Loading workstreams…"}
        </p>
        <div className="flex items-center gap-4">
          <Label className="flex items-center gap-2 text-sm">
            <Switch
              checked={includeClosed}
              onCheckedChange={(checked) => setIncludeClosed(checked === true)}
              aria-label="show closed workstreams"
            />
            Show closed
          </Label>
          <Button type="button" size="sm" onClick={() => setKickoffOpen(true)}>
            <PlusIcon className="size-4" aria-hidden />
            Kick off
          </Button>
        </div>
      </div>

      {list.isLoading && <Skeleton className="h-24 w-full" data-testid="workstreams-loading" />}
      {list.data && rows.length === 0 && (
        <EmptyState>
          No {includeClosed ? "" : "open "}workstreams. Kick one off, or let a matching event open
          one.
        </EmptyState>
      )}
      <ul className="flex flex-col gap-1">
        {rows.map((instance) => (
          <WorkstreamRow
            key={instance.id}
            instance={instance}
            now={tick}
            expanded={expanded === instance.id}
            onToggle={() => setExpanded(expanded === instance.id ? null : instance.id)}
          />
        ))}
      </ul>

      {(drops.data?.drops.length ?? 0) > 0 && (
        <details className="text-muted-foreground rounded-md border border-dashed px-3 py-2 text-xs">
          <summary className="cursor-pointer select-none">
            {drops.data!.drops.length} recent events did not fire
          </summary>
          <ul className="mt-2 flex flex-col gap-1" data-testid="recent-drops">
            {drops.data!.drops.map((drop, i) => (
              <li key={i} className="flex items-center gap-2">
                <span className="bg-secondary rounded px-1.5 py-0.5">
                  {DROP_REASON_LABEL[drop.reason] ?? drop.reason}
                </span>
                <span className="min-w-0 flex-1 truncate">
                  {drop.eventKey || drop.entrypointId} · {drop.detail}
                </span>
                <span className="shrink-0 tabular-nums">{relativeTime(drop.droppedAt, tick)}</span>
              </li>
            ))}
          </ul>
        </details>
      )}

      <KickoffDialog
        automationId={automationId}
        inputsSchema={inputsSchema}
        defaultInputs={defaultInputs}
        openKeys={
          new Set(
            (list.data?.instances ?? [])
              .filter((instance) => instance.status === "open")
              .map((instance) => instance.key),
          )
        }
        open={kickoffOpen}
        onOpenChange={setKickoffOpen}
      />
    </section>
  );
}

function WorkstreamRow({
  instance,
  now,
  expanded,
  onToggle,
}: {
  instance: AutomationInstance;
  now: number;
  expanded: boolean;
  onToggle: () => void;
}) {
  const open = instance.status === "open";
  return (
    <li className="rounded-md border" data-testid="workstream-row">
      <button
        type="button"
        onClick={onToggle}
        aria-expanded={expanded}
        className="hover:bg-secondary/60 flex w-full items-center gap-3 rounded-md px-3 py-2 text-left text-sm"
      >
        <ChevronRightIcon
          className={cn(
            "text-muted-foreground size-3 shrink-0 transition-transform",
            expanded && "rotate-90",
          )}
        />
        <RunStatusDot status={open ? "running" : "completed"} />
        <span className="min-w-0 flex-1 truncate font-medium">{instance.key}</span>
        {!open && (
          <span className="bg-secondary text-muted-foreground rounded px-1.5 text-xs">
            closed{instance.closeReason ? ` · ${instance.closeReason}` : ""}
          </span>
        )}
        <span className="text-muted-foreground w-20 shrink-0 text-right text-xs tabular-nums">
          {relativeTime(instance.openedAt, now)}
        </span>
      </button>
      {expanded && <WorkstreamDetail instance={instance} now={now} />}
    </li>
  );
}

function WorkstreamDetail({ instance, now }: { instance: AutomationInstance; now: number }) {
  const detail = useInstance(instance.id);
  const closeMutation = useCloseInstance();
  const inputs = useMemo(() => {
    try {
      return Object.entries(JSON.parse(instance.inputsJson) as Record<string, unknown>);
    } catch {
      return [];
    }
  }, [instance.inputsJson]);

  return (
    <div className="border-t px-4 py-3 text-sm" data-testid="workstream-detail">
      {inputs.length > 0 && (
        <dl className="mb-3 grid grid-cols-[auto_1fr] gap-x-4 gap-y-1 text-xs">
          {inputs.map(([key, value]) => (
            <div key={key} className="contents">
              <dt className="text-muted-foreground">{key}</dt>
              <dd className="min-w-0 truncate tabular-nums">
                {typeof value === "string" ? value : JSON.stringify(value)}
              </dd>
            </div>
          ))}
        </dl>
      )}
      {(detail.data?.handles.length ?? 0) > 0 && (
        <div className="mb-3">
          <p className="text-muted-foreground mb-1 text-xs">Routes here</p>
          <ul className="flex flex-wrap gap-1" data-testid="workstream-handles">
            {detail.data!.handles.map((h) => (
              <li key={h.handle} className="bg-secondary rounded px-1.5 py-0.5 font-mono text-xs">
                {h.handle}
              </li>
            ))}
          </ul>
        </div>
      )}
      <RunList automationId={instance.automationId} instanceId={instance.id} now={() => now} />
      {instance.status === "open" && (
        <div className="mt-3">
          <Button
            type="button"
            size="sm"
            variant="outline"
            disabled={closeMutation.isPending}
            onClick={async () => {
              try {
                await closeMutation.mutateAsync({ id: instance.id });
                toast.success(`Closed ${instance.key}`);
              } catch (error) {
                toast.error(error instanceof Error ? error.message : String(error));
              }
            }}
          >
            Close workstream
          </Button>
        </div>
      )}
    </div>
  );
}

function KickoffDialog({
  automationId,
  inputsSchema,
  defaultInputs,
  openKeys,
  open,
  onOpenChange,
}: {
  automationId: string;
  inputsSchema: InputFieldSpec[];
  defaultInputs: unknown;
  /** Keys of currently-open workstreams: typing one switches the dialog to
   * JOIN mode — no inputs are sent (the server refuses a join snapshot;
   * the open workstream's kickoff inputs stand). */
  openKeys: Set<string>;
  open: boolean;
  onOpenChange: (open: boolean) => void;
}) {
  const runNow = useRunNow();
  const invalidateInstances = useInvalidateInstances();
  const [key, setKey] = useState("");
  const [values, setValues] = useState<InputValues | null>(null);
  const [errors, setErrors] = useState<InputFieldError[]>([]);
  const resolved = useMemo(
    () => resolveInputValues(inputsSchema, defaultInputs),
    [inputsSchema, defaultInputs],
  );
  const current = values ?? resolved;
  const joining = openKeys.has(key.trim());

  const kickoff = async () => {
    if (key.trim() === "") {
      toast.error("Name the workstream (its key)");
      return;
    }
    if (!joining) {
      const clientErrors = validateInputs(inputsSchema, current);
      if (clientErrors.length > 0) {
        setErrors(clientErrors);
        toast.error("Fix the highlighted inputs");
        return;
      }
    }
    try {
      await runNow.mutateAsync({
        automationId,
        instanceKey: key.trim(),
        // Joining an open workstream sends NO snapshot: its kickoff inputs
        // stand, and the server refuses a join that carries one.
        ...(joining ? {} : { instanceInputsJson: buildInputsPayload(inputsSchema, current) }),
      });
      await invalidateInstances();
      toast.success(joining ? `Joined ${key.trim()}` : `Kicked off ${key.trim()}`);
      onOpenChange(false);
      setKey("");
      setValues(null);
      setErrors([]);
    } catch (error) {
      toast.error(error instanceof Error ? error.message : String(error));
    }
  };

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="max-w-lg">
        <DialogHeader>
          <DialogTitle>Kick off a workstream</DialogTitle>
          <DialogDescription>
            Names an ongoing case this automation owns. Kicking off an existing open key joins it
            instead.
          </DialogDescription>
        </DialogHeader>
        <form
          className="flex flex-col gap-4"
          data-testid="kickoff-form"
          onSubmit={(e) => {
            e.preventDefault();
            void kickoff();
          }}
        >
          <Field>
            <FieldLabel>Key</FieldLabel>
            <Input
              value={key}
              onChange={(e) => setKey(e.target.value)}
              placeholder="project-ENG-42"
              aria-label="workstream key"
              autoFocus
            />
          </Field>
          {joining ? (
            <p className="text-muted-foreground text-sm" data-testid="kickoff-join-hint">
              {key.trim()} is already open — this run joins it, and its kickoff inputs stay as they
              are.
            </p>
          ) : (
            <InputsForm
              schema={inputsSchema}
              values={current}
              errors={errors}
              onChange={(k, next) => {
                setValues({ ...current, [k]: next });
                setErrors((prev) => prev.filter((e) => e.key !== k));
              }}
              disabled={runNow.isPending}
            />
          )}
          <DialogFooter>
            <Button type="submit" disabled={runNow.isPending}>
              {runNow.isPending
                ? joining
                  ? "Joining…"
                  : "Kicking off…"
                : joining
                  ? "Join"
                  : "Kick off"}
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  );
}
