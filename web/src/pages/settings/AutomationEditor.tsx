import { useEffect, useMemo, useRef, useState } from "react";
import { Link, useNavigate, useParams } from "@tanstack/react-router";
import { toast } from "sonner";
import {
  AlertCircleIcon,
  ArrowLeftIcon,
  CheckCircle2Icon,
  ChevronDownIcon,
  Clock3Icon,
  Code2Icon,
  ExternalLinkIcon,
  FileJson2Icon,
  Loader2Icon,
  SaveIcon,
  SparklesIcon,
  WebhookIcon,
} from "lucide-react";

import type { TestRenderResponse } from "@/gen/engram/app/v1/automation_pb";
import {
  useAutomation,
  useAutomationRuns,
  useCreateAutomation,
  useTestAutomationRender,
  useUpdateAutomation,
  useWebhookEvents,
  useWebhookRegistrations,
  useWebhookSamples,
} from "@/hooks/useAutomations";
import { useHarnessCatalog } from "@/hooks/useHarnessCatalog";
import { useProfiles } from "@/hooks/useProfiles";
import {
  automationErrorField,
  automationStatusLabel,
  rawVariables,
  type AutomationField,
} from "@/lib/automations";
import { errorMessage } from "@/lib/errors";
import { shortId } from "@/pages/sessions/session-format";
import {
  EMPTY_OVERRIDE,
  SessionHarnessControls,
  type HarnessOverride,
} from "@/pages/sessions/SessionHarnessControls";
import { PageHeading } from "@/components/page-heading";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card";
import { Collapsible, CollapsibleContent, CollapsibleTrigger } from "@/components/ui/collapsible";
import { Field, FieldDescription, FieldError, FieldLabel } from "@/components/ui/field";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { Switch } from "@/components/ui/switch";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";
import { Textarea } from "@/components/ui/textarea";

type TriggerKind = "cron" | "webhook";
type TemplateTarget = "prompt" | "title";

type EditorErrors = Partial<Record<AutomationField, string>>;

interface EditorDraft {
  name: string;
  description: string;
  enabled: boolean;
  triggerKind: TriggerKind;
  schedule: string;
  timezone: string;
  registrationId: string;
  events: string[];
  profileId: string;
  /** ADR 0063 B2: `null` on a field = inherit the profile's default. */
  override: HarnessOverride;
  promptTemplate: string;
  titleTemplate: string;
  includeEventContext: boolean;
  /** ADR 0107: start the session in plan mode (plan-then-implement). */
  planFirst: boolean;
}

const EMPTY_DRAFT: EditorDraft = {
  name: "",
  description: "",
  enabled: true,
  triggerKind: "cron",
  schedule: "0 9 * * 1-5",
  timezone: "UTC",
  registrationId: "",
  events: [],
  profileId: "",
  override: EMPTY_OVERRIDE,
  promptTemplate: "",
  titleTemplate: "",
  includeEventContext: true,
  planFirst: false,
};

const TIMEZONE_SUGGESTIONS = [
  "UTC",
  "America/Los_Angeles",
  "America/New_York",
  "Europe/London",
  "Europe/Berlin",
  "Asia/Kolkata",
  "Asia/Tokyo",
  "Australia/Sydney",
];

/** Send only the fields the automation actually overrides — an absent field is
 *  the profile's default, which the server resolves at launch (ADR 0063 B2). */
function overrideFields(override: HarnessOverride): {
  harness?: string;
  model?: string;
  modelRouter?: string;
  effort?: string;
} {
  return {
    ...(override.harness ? { harness: override.harness } : {}),
    ...(override.model ? { model: override.model } : {}),
    ...(override.modelRouter !== null ? { modelRouter: override.modelRouter } : {}),
    ...(override.effort ? { effort: override.effort } : {}),
  };
}

function fieldError(errors: EditorErrors, field: AutomationField) {
  return errors[field] ? <FieldError>{errors[field]}</FieldError> : null;
}

function runStatusVariant(status: string): "default" | "secondary" | "destructive" | "outline" {
  if (status === "launched") return "default";
  if (status === "skipped") return "secondary";
  if (status === "render_failed" || status === "launch_failed") return "destructive";
  return "outline";
}

function RunHistory({ automationId }: { automationId: string }) {
  const runs = useAutomationRuns(automationId, 50);

  return (
    <Card>
      <CardHeader>
        <CardTitle>Run history</CardTitle>
        <CardDescription>
          Launch records keep the rendered prompt and failure details used for debugging.
        </CardDescription>
      </CardHeader>
      <CardContent>
        {runs.isPending && <p className="text-sm text-muted-foreground">Loading runs…</p>}
        {runs.error && <p className="text-sm text-destructive">{errorMessage(runs.error)}</p>}
        {!runs.isPending && (runs.data?.runs.length ?? 0) === 0 && (
          <div className="rounded-md border border-dashed p-6 text-center text-sm text-muted-foreground">
            This automation has not run yet.
          </div>
        )}
        {(runs.data?.runs.length ?? 0) > 0 && (
          <Table>
            <TableHeader>
              <TableRow>
                <TableHead>Status</TableHead>
                <TableHead>Created</TableHead>
                <TableHead>Prompt</TableHead>
                <TableHead>Task</TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {runs.data?.runs.map((run) => (
                <TableRow key={run.id}>
                  <TableCell className="align-top">
                    <Badge variant={runStatusVariant(run.status)}>
                      {automationStatusLabel(run.status)}
                    </Badge>
                    {run.error && (
                      <p className="mt-2 max-w-xs whitespace-normal text-xs text-destructive">
                        {run.error}
                      </p>
                    )}
                  </TableCell>
                  <TableCell className="align-top text-xs text-muted-foreground">
                    <time dateTime={run.createdAt}>{new Date(run.createdAt).toLocaleString()}</time>
                  </TableCell>
                  <TableCell className="max-w-md align-top whitespace-normal">
                    {run.renderedPrompt ? (
                      <Collapsible>
                        <CollapsibleTrigger asChild>
                          <Button variant="ghost" size="sm" className="h-auto px-0">
                            <ChevronDownIcon className="size-3.5" />
                            Show rendered prompt
                          </Button>
                        </CollapsibleTrigger>
                        <CollapsibleContent>
                          <pre className="mt-2 max-h-56 overflow-auto whitespace-pre-wrap rounded-md bg-muted p-3 font-mono text-xs">
                            {run.renderedPrompt}
                          </pre>
                        </CollapsibleContent>
                      </Collapsible>
                    ) : (
                      <span className="text-xs text-muted-foreground">—</span>
                    )}
                  </TableCell>
                  <TableCell className="align-top">
                    {run.taskId ? (
                      run.sessionId ? (
                        <Button
                          asChild
                          variant="link"
                          size="sm"
                          className="h-auto px-0 font-mono text-xs"
                        >
                          <Link to="/sessions/$id" params={{ id: run.sessionId }}>
                            {shortId(run.taskId)}
                            <ExternalLinkIcon className="size-3" />
                          </Link>
                        </Button>
                      ) : (
                        <Button
                          asChild
                          variant="link"
                          size="sm"
                          className="h-auto px-0 font-mono text-xs"
                        >
                          <Link to="/sessions/list">
                            {shortId(run.taskId)}
                            <ExternalLinkIcon className="size-3" />
                          </Link>
                        </Button>
                      )
                    ) : (
                      <span className="text-xs text-muted-foreground">—</span>
                    )}
                  </TableCell>
                </TableRow>
              ))}
            </TableBody>
          </Table>
        )}
      </CardContent>
    </Card>
  );
}

function PreviewPanel({
  preview,
  pending,
  requestError,
}: {
  preview: TestRenderResponse | null;
  pending: boolean;
  requestError: string;
}) {
  return (
    <Card className="gap-4 lg:sticky lg:top-4">
      <CardHeader className="border-b">
        <div className="flex items-center gap-2">
          <SparklesIcon className="size-4 text-primary" />
          <CardTitle>Live preview</CardTitle>
          {pending && <Loader2Icon className="size-3.5 animate-spin text-muted-foreground" />}
        </div>
        <CardDescription>
          Rendered by the same strict template engine used before a session launches.
        </CardDescription>
      </CardHeader>
      <CardContent className="space-y-4">
        {requestError && (
          <div className="flex gap-2 rounded-md border border-destructive/30 bg-destructive/5 p-3 text-sm text-destructive">
            <AlertCircleIcon className="mt-0.5 size-4 shrink-0" />
            {requestError}
          </div>
        )}
        {(preview?.errors.length ?? 0) > 0 && (
          <div className="space-y-2">
            {preview?.errors.map((error, index) => (
              <div
                key={`${error.field}-${index}`}
                className="rounded-md border border-destructive/30 bg-destructive/5 p-3"
              >
                <p className="text-xs font-medium text-destructive">{error.field}</p>
                <p className="mt-1 text-sm text-destructive">{error.message}</p>
              </div>
            ))}
          </div>
        )}
        {!requestError && preview && preview.errors.length === 0 && (
          <>
            <div className="flex items-center gap-2 text-xs text-muted-foreground">
              <CheckCircle2Icon className="size-4 text-primary" />
              Template rendered successfully
            </div>
            {preview.renderedTitle && (
              <div>
                <p className="mb-1.5 text-xs font-medium text-muted-foreground">Title</p>
                <div className="rounded-md border bg-muted/40 p-3 text-sm font-medium">
                  {preview.renderedTitle}
                </div>
              </div>
            )}
            <div>
              <p className="mb-1.5 text-xs font-medium text-muted-foreground">Prompt</p>
              <pre className="max-h-[32rem] min-h-36 overflow-auto whitespace-pre-wrap rounded-md border bg-muted/40 p-3 font-mono text-xs leading-relaxed">
                {preview.renderedPrompt ?? ""}
              </pre>
            </div>
          </>
        )}
        {!preview && !requestError && !pending && (
          <div className="rounded-md border border-dashed p-6 text-center text-sm text-muted-foreground">
            Enter a prompt to render a preview.
          </div>
        )}
      </CardContent>
    </Card>
  );
}

export function AutomationEditor({ mode }: { mode: "create" | "edit" }) {
  const params = useParams({ strict: false });
  const id = typeof params.id === "string" ? params.id : undefined;
  const navigate = useNavigate();
  const existing = useAutomation(mode === "edit" ? id : undefined);
  const profiles = useProfiles(false);
  const harnesses = useHarnessCatalog();
  const registrations = useWebhookRegistrations();
  const createAutomation = useCreateAutomation();
  const updateAutomation = useUpdateAutomation();
  const testRender = useTestAutomationRender();
  const [draft, setDraft] = useState<EditorDraft>(EMPTY_DRAFT);
  const [hydratedId, setHydratedId] = useState<string | null>(null);
  const [errors, setErrors] = useState<EditorErrors>({});
  const [preview, setPreview] = useState<TestRenderResponse | null>(null);
  const [previewRequestError, setPreviewRequestError] = useState("");
  const [selectedSampleId, setSelectedSampleId] = useState("");
  const [activeTemplate, setActiveTemplate] = useState<TemplateTarget>("prompt");
  const promptRef = useRef<HTMLTextAreaElement>(null);
  const titleRef = useRef<HTMLInputElement>(null);
  const previewSequence = useRef(0);
  const events = useWebhookEvents(
    draft.triggerKind === "webhook" ? draft.registrationId : undefined,
  );
  const samples = useWebhookSamples(
    draft.triggerKind === "webhook" ? draft.registrationId : undefined,
    25,
  );

  useEffect(() => {
    const automation = existing.data?.automation;
    if (mode !== "edit" || !automation || hydratedId === automation.id) return;
    const trigger = automation.trigger?.trigger;
    const action = automation.action?.action;
    setDraft({
      name: automation.name,
      description: automation.description,
      enabled: automation.enabled,
      triggerKind: trigger?.case === "webhook" ? "webhook" : "cron",
      schedule: trigger?.case === "cron" ? trigger.value.schedule : EMPTY_DRAFT.schedule,
      timezone: trigger?.case === "cron" ? trigger.value.timezone : EMPTY_DRAFT.timezone,
      registrationId: trigger?.case === "webhook" ? trigger.value.registrationId : "",
      events: trigger?.case === "webhook" ? [...trigger.value.events] : [],
      profileId: action?.case === "createTask" ? action.value.profileId : "",
      override:
        action?.case === "createTask"
          ? {
              harness: action.value.harness ?? null,
              modelRouter: action.value.modelRouter ?? null,
              model: action.value.model ?? null,
              effort: action.value.effort ?? null,
              // Mode is the `planFirst` switch below, not one of these pickers.
              mode: null,
            }
          : EMPTY_OVERRIDE,
      promptTemplate: action?.case === "createTask" ? action.value.promptTemplate : "",
      titleTemplate: action?.case === "createTask" ? (action.value.titleTemplate ?? "") : "",
      includeEventContext: action?.case === "createTask" ? action.value.includeEventContext : true,
      planFirst: action?.case === "createTask" ? action.value.harnessMode === "plan" : false,
    });
    setHydratedId(automation.id);
  }, [existing.data?.automation, hydratedId, mode]);

  useEffect(() => {
    if (mode === "create" && !draft.profileId && profiles.data?.profiles[0]) {
      setDraft((current) => ({ ...current, profileId: profiles.data!.profiles[0]!.id }));
    }
  }, [draft.profileId, mode, profiles.data]);

  useEffect(() => {
    const available = samples.data?.samples ?? [];
    if (selectedSampleId && available.some((sample) => sample.id === selectedSampleId)) return;
    setSelectedSampleId(available[0]?.id ?? "");
  }, [samples.data?.samples, selectedSampleId]);

  useEffect(() => {
    const sequence = ++previewSequence.current;
    if (!draft.promptTemplate.trim()) {
      setPreview(null);
      setPreviewRequestError("");
      return;
    }
    const timer = window.setTimeout(async () => {
      try {
        const response = await testRender.mutateAsync({
          ...(mode === "edit" && id ? { automationId: id } : {}),
          automationName: draft.name || "Draft automation",
          draftAction: {
            action: {
              case: "createTask",
              value: {
                // The preview renders templates only; the harness/model/effort
                // override does not affect the rendered text.
                profileId: draft.profileId,
                promptTemplate: draft.promptTemplate,
                ...(draft.titleTemplate.trim() ? { titleTemplate: draft.titleTemplate } : {}),
                includeEventContext: draft.includeEventContext,
                ...(draft.planFirst ? { harnessMode: "plan" } : {}),
              },
            },
          },
          ...(draft.triggerKind === "cron"
            ? { scheduledFor: new Date().toISOString() }
            : {
                registrationId: draft.registrationId,
                eventKey: draft.events[0] ?? "",
                ...(selectedSampleId
                  ? { sample: { case: "sampleId" as const, value: selectedSampleId } }
                  : {}),
              }),
        });
        if (previewSequence.current === sequence) {
          setPreview(response);
          setPreviewRequestError("");
        }
      } catch (previewError) {
        if (previewSequence.current === sequence) {
          setPreview(null);
          setPreviewRequestError(errorMessage(previewError));
        }
      }
    }, 450);
    return () => window.clearTimeout(timer);
  }, [
    draft.events,
    draft.includeEventContext,
    draft.name,
    draft.profileId,
    draft.promptTemplate,
    draft.registrationId,
    draft.titleTemplate,
    draft.triggerKind,
    id,
    mode,
    selectedSampleId,
  ]);

  const selectedProfile = profiles.data?.profiles.find((profile) => profile.id === draft.profileId);
  const selectedSample = samples.data?.samples.find((sample) => sample.id === selectedSampleId);
  const raw = useMemo(
    () => rawVariables(selectedSample?.payloadJson),
    [selectedSample?.payloadJson],
  );

  const update = <K extends keyof EditorDraft>(key: K, value: EditorDraft[K]) => {
    setDraft((current) => ({ ...current, [key]: value }));
    setErrors((current) => ({ ...current, [key]: undefined, form: undefined }));
  };

  const toggleEvent = (eventKey: string, checked: boolean) => {
    update(
      "events",
      checked
        ? [...new Set([...draft.events, eventKey])]
        : draft.events.filter((key) => key !== eventKey),
    );
  };

  const insertVariable = (path: string) => {
    const token = `\${{ ${path} }}`;
    if (activeTemplate === "title") {
      const input = titleRef.current;
      const start = input?.selectionStart ?? draft.titleTemplate.length;
      const end = input?.selectionEnd ?? start;
      update(
        "titleTemplate",
        `${draft.titleTemplate.slice(0, start)}${token}${draft.titleTemplate.slice(end)}`,
      );
      requestAnimationFrame(() => {
        titleRef.current?.focus();
        titleRef.current?.setSelectionRange(start + token.length, start + token.length);
      });
      return;
    }
    const textarea = promptRef.current;
    const start = textarea?.selectionStart ?? draft.promptTemplate.length;
    const end = textarea?.selectionEnd ?? start;
    update(
      "promptTemplate",
      `${draft.promptTemplate.slice(0, start)}${token}${draft.promptTemplate.slice(end)}`,
    );
    requestAnimationFrame(() => {
      promptRef.current?.focus();
      promptRef.current?.setSelectionRange(start + token.length, start + token.length);
    });
  };

  const validate = (): EditorErrors => {
    const next: EditorErrors = {};
    if (!draft.name.trim()) next.name = "Name is required.";
    if (!draft.profileId) next.profile = "Choose a profile.";
    if (!draft.promptTemplate.trim()) next.promptTemplate = "Prompt template is required.";
    if (draft.triggerKind === "cron") {
      if (!draft.schedule.trim()) next.schedule = "Cron schedule is required.";
      if (!draft.timezone.trim()) next.timezone = "IANA timezone is required.";
    } else {
      if (!draft.registrationId) next.registration = "Choose a webhook registration.";
      if (draft.events.length === 0) next.events = "Choose at least one event.";
    }
    return next;
  };

  const onSave = async () => {
    const localErrors = validate();
    if (Object.keys(localErrors).length > 0) {
      setErrors(localErrors);
      return;
    }
    const templateErrors: EditorErrors = {};
    for (const templateError of preview?.errors ?? []) {
      if (templateError.field === "prompt_template") {
        templateErrors.promptTemplate = templateError.message;
      } else if (templateError.field === "title_template") {
        templateErrors.titleTemplate = templateError.message;
      }
    }
    if (Object.keys(templateErrors).length > 0) {
      setErrors(templateErrors);
      return;
    }
    setErrors({});
    const request = {
      name: draft.name,
      description: draft.description,
      enabled: draft.enabled,
      trigger: {
        trigger:
          draft.triggerKind === "cron"
            ? {
                case: "cron" as const,
                value: { schedule: draft.schedule, timezone: draft.timezone },
              }
            : {
                case: "webhook" as const,
                value: { registrationId: draft.registrationId, events: draft.events },
              },
      },
      action: {
        action: {
          case: "createTask" as const,
          value: {
            profileId: draft.profileId,
            promptTemplate: draft.promptTemplate,
            ...(draft.titleTemplate.trim() ? { titleTemplate: draft.titleTemplate } : {}),
            includeEventContext: draft.includeEventContext,
            ...(draft.planFirst ? { harnessMode: "plan" } : {}),
            ...overrideFields(draft.override),
          },
        },
      },
    };
    try {
      const response =
        mode === "edit" && id
          ? await updateAutomation.mutateAsync({ id, ...request })
          : await createAutomation.mutateAsync(request);
      const saved = response.automation;
      if (!saved) throw new Error("Saved automation was not returned");
      toast.success(mode === "edit" ? "Automation updated" : "Automation created");
      if (mode === "create") {
        await navigate({ to: "/settings/automations/$id", params: { id: saved.id } });
      }
    } catch (saveError) {
      const message = errorMessage(saveError);
      setErrors({ [automationErrorField(message)]: message });
    }
  };

  if (mode === "edit" && existing.isPending) {
    return <p className="text-sm text-muted-foreground">Loading automation…</p>;
  }

  if (mode === "edit" && (existing.error || !existing.data?.automation)) {
    return (
      <div className="space-y-4">
        <Button asChild variant="ghost" size="sm">
          <Link to="/settings/automations">
            <ArrowLeftIcon className="size-4" />
            Automations
          </Link>
        </Button>
        <p className="text-sm text-destructive">
          {existing.error ? errorMessage(existing.error) : "Automation not found"}
        </p>
      </div>
    );
  }

  const pending = createAutomation.isPending || updateAutomation.isPending;
  const availableEvents = events.data?.events ?? [];

  return (
    <div className="space-y-6">
      <PageHeading
        title={mode === "create" ? "New automation" : draft.name || "Automation"}
        actions={
          <>
            <Button asChild variant="outline" size="sm">
              <Link to="/settings/automations">
                <ArrowLeftIcon className="size-3.5" />
                Back
              </Link>
            </Button>
            <Button size="sm" onClick={onSave} disabled={pending}>
              {pending ? (
                <Loader2Icon className="size-3.5 animate-spin" />
              ) : (
                <SaveIcon className="size-3.5" />
              )}
              {mode === "create" ? "Create automation" : "Save changes"}
            </Button>
          </>
        }
      />

      {errors.form && <FieldError>{errors.form}</FieldError>}

      <Card>
        <CardHeader>
          <CardTitle>Identity</CardTitle>
          <CardDescription>Name the outcome this automation owns.</CardDescription>
        </CardHeader>
        <CardContent className="grid gap-5 md:grid-cols-2">
          <Field data-invalid={!!errors.name}>
            <FieldLabel htmlFor="automation-name">Name</FieldLabel>
            <Input
              id="automation-name"
              value={draft.name}
              placeholder="Triage new GitHub issues"
              aria-invalid={!!errors.name}
              onChange={(event) => update("name", event.target.value)}
            />
            {fieldError(errors, "name")}
          </Field>
          <Field>
            <FieldLabel htmlFor="automation-description">Description</FieldLabel>
            <Input
              id="automation-description"
              value={draft.description}
              placeholder="Launches the support triage profile"
              onChange={(event) => update("description", event.target.value)}
            />
          </Field>
          <Field orientation="horizontal" className="md:col-span-2">
            <div>
              <FieldLabel htmlFor="automation-enabled">Enabled</FieldLabel>
              <FieldDescription>
                Start accepting matching events as soon as this is saved.
              </FieldDescription>
            </div>
            <Switch
              id="automation-enabled"
              checked={draft.enabled}
              onCheckedChange={(checked) => update("enabled", checked)}
            />
          </Field>
        </CardContent>
      </Card>

      <Card>
        <CardHeader>
          <CardTitle>Trigger</CardTitle>
          <CardDescription>Choose when this automation gets one chance to launch.</CardDescription>
        </CardHeader>
        <CardContent className="space-y-5">
          <div className="grid grid-cols-2 gap-2 rounded-lg bg-muted p-1">
            <button
              type="button"
              onClick={() => update("triggerKind", "cron")}
              className={`flex items-center justify-center gap-2 rounded-md px-3 py-2 text-sm font-medium transition-colors ${
                draft.triggerKind === "cron" ? "bg-background shadow-xs" : "text-muted-foreground"
              }`}
            >
              <Clock3Icon className="size-4" />
              Schedule
            </button>
            <button
              type="button"
              onClick={() => update("triggerKind", "webhook")}
              className={`flex items-center justify-center gap-2 rounded-md px-3 py-2 text-sm font-medium transition-colors ${
                draft.triggerKind === "webhook"
                  ? "bg-background shadow-xs"
                  : "text-muted-foreground"
              }`}
            >
              <WebhookIcon className="size-4" />
              Webhook event
            </button>
          </div>

          {draft.triggerKind === "cron" ? (
            <div className="grid gap-5 md:grid-cols-2">
              <Field data-invalid={!!errors.schedule}>
                <FieldLabel htmlFor="cron-schedule">Cron schedule</FieldLabel>
                <Input
                  id="cron-schedule"
                  className="font-mono"
                  value={draft.schedule}
                  placeholder="0 9 * * 1-5"
                  aria-invalid={!!errors.schedule}
                  onChange={(event) => update("schedule", event.target.value)}
                />
                <FieldDescription>
                  Five-field cron expression. The server validates the expression on save.
                </FieldDescription>
                {fieldError(errors, "schedule")}
              </Field>
              <Field data-invalid={!!errors.timezone}>
                <FieldLabel htmlFor="cron-timezone">IANA timezone</FieldLabel>
                <Input
                  id="cron-timezone"
                  list="automation-timezones"
                  className="font-mono"
                  value={draft.timezone}
                  placeholder="America/Los_Angeles"
                  aria-invalid={!!errors.timezone}
                  onChange={(event) => update("timezone", event.target.value)}
                />
                <datalist id="automation-timezones">
                  {TIMEZONE_SUGGESTIONS.map((timezone) => (
                    <option key={timezone} value={timezone} />
                  ))}
                </datalist>
                <FieldDescription>
                  {existing.data?.automation?.nextFireAt
                    ? `Next saved fire: ${new Date(existing.data.automation.nextFireAt).toLocaleString()}`
                    : "Preview renders a synthetic occurrence at the current time."}
                </FieldDescription>
                {fieldError(errors, "timezone")}
              </Field>
            </div>
          ) : (
            <div className="space-y-5">
              <Field data-invalid={!!errors.registration}>
                <FieldLabel>Registration</FieldLabel>
                <Select
                  value={draft.registrationId}
                  onValueChange={(value) => {
                    update("registrationId", value);
                    update("events", []);
                    setSelectedSampleId("");
                  }}
                >
                  <SelectTrigger className="w-full" aria-invalid={!!errors.registration}>
                    <SelectValue placeholder="Choose a webhook endpoint" />
                  </SelectTrigger>
                  <SelectContent>
                    {registrations.data?.registrations.map((registration) => (
                      <SelectItem key={registration.id} value={registration.id}>
                        {registration.name} · {registration.id}
                      </SelectItem>
                    ))}
                  </SelectContent>
                </Select>
                {fieldError(errors, "registration")}
                {(registrations.data?.registrations.length ?? 0) === 0 && (
                  <FieldDescription>
                    Create a webhook registration on the{" "}
                    <Link to="/settings/automations">automations list</Link> first.
                  </FieldDescription>
                )}
              </Field>

              <Field data-invalid={!!errors.events}>
                <FieldLabel>Events</FieldLabel>
                {events.isPending ? (
                  <p className="text-sm text-muted-foreground">Loading event taxonomy…</p>
                ) : availableEvents.length > 0 ? (
                  <div className="grid gap-2 md:grid-cols-2">
                    {availableEvents.map((event) => (
                      <Label
                        key={event.key}
                        className="flex cursor-pointer items-center justify-between rounded-md border p-3"
                      >
                        <span className="flex items-center gap-2">
                          <input
                            type="checkbox"
                            className="size-4 accent-primary"
                            checked={draft.events.includes(event.key)}
                            onChange={(change) => toggleEvent(event.key, change.target.checked)}
                          />
                          <span>
                            <span className="block text-sm">{event.displayName || event.key}</span>
                            <span className="block font-mono text-[0.7rem] text-muted-foreground">
                              {event.key}
                            </span>
                          </span>
                        </span>
                        {event.observed && <Badge variant="secondary">observed</Badge>}
                      </Label>
                    ))}
                  </div>
                ) : (
                  <div className="rounded-md border border-dashed p-4 text-sm text-muted-foreground">
                    {draft.registrationId
                      ? "No declared or observed events yet. Send a verified event to populate this list."
                      : "Choose a registration to list its events."}
                  </div>
                )}
                {fieldError(errors, "events")}
              </Field>

              <Field>
                <FieldLabel>Preview sample</FieldLabel>
                <Select value={selectedSampleId} onValueChange={setSelectedSampleId}>
                  <SelectTrigger className="w-full">
                    <SelectValue placeholder="Use the latest captured event" />
                  </SelectTrigger>
                  <SelectContent>
                    {samples.data?.samples.map((sample) => (
                      <SelectItem key={sample.id} value={sample.id}>
                        {sample.eventKey} · {new Date(sample.receivedAt).toLocaleString()}
                      </SelectItem>
                    ))}
                  </SelectContent>
                </Select>
                <FieldDescription>
                  Samples are redacted before storage and are used only for authoring and preview.
                </FieldDescription>
              </Field>
            </div>
          )}
        </CardContent>
      </Card>

      <Card>
        <CardHeader>
          <CardTitle>Session profile</CardTitle>
          <CardDescription>
            The profile supplies the image, harness, integrations, credentials, and network policy.
          </CardDescription>
        </CardHeader>
        <CardContent className="space-y-5">
          <Field data-invalid={!!errors.profile}>
            <FieldLabel>Profile</FieldLabel>
            <Select
              value={draft.profileId}
              onValueChange={(value) => {
                update("profileId", value);
                // Model/effort are option ids on the profile harness's
                // descriptor, so a carried-over selection could name an option
                // the new profile's harness does not have.
                update("override", EMPTY_OVERRIDE);
              }}
            >
              <SelectTrigger className="w-full" aria-invalid={!!errors.profile}>
                <SelectValue placeholder="Choose a session profile" />
              </SelectTrigger>
              <SelectContent>
                {profiles.data?.profiles.map((profile) => (
                  <SelectItem key={profile.id} value={profile.id}>
                    {profile.name}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
            <FieldDescription>
              If the profile declares port exposures, automation runs ignore them: an automation
              session has no user owner to attribute a preview link to. An admin can still expose a
              port by hand on a running automation session.
            </FieldDescription>
            {fieldError(errors, "profile")}
          </Field>
          <Field>
            <FieldLabel>Harness, model, and effort</FieldLabel>
            <SessionHarnessControls
              harnesses={harnesses.data}
              {...(selectedProfile?.harness ? { profileHarness: selectedProfile.harness } : {})}
              {...(selectedProfile?.modelRouter
                ? { profileModelRouter: selectedProfile.modelRouter }
                : {})}
              {...(selectedProfile?.model ? { profileModel: selectedProfile.model } : {})}
              value={draft.override}
              onChange={(next) => update("override", next)}
              audience="programmatic"
            />
            <FieldDescription>
              Every run of this automation uses this selection. A field left on the profile default
              follows the profile. A model or effort choice also pins the harness it belongs to, so
              a later profile edit cannot silently change the model this automation runs.
            </FieldDescription>
          </Field>
        </CardContent>
      </Card>

      <div className="grid items-start gap-6 lg:grid-cols-[minmax(0,1.45fr)_minmax(19rem,0.8fr)]">
        <Card>
          <CardHeader>
            <CardTitle>Prompt template</CardTitle>
            <CardDescription>
              Click a variable to insert it at the active cursor. Missing variables fail before a
              billable session launches.
            </CardDescription>
          </CardHeader>
          <CardContent className="space-y-5">
            <div className="grid min-h-[31rem] gap-4 xl:grid-cols-[minmax(0,1fr)_15rem]">
              <div className="space-y-5">
                <Field data-invalid={!!errors.promptTemplate}>
                  <FieldLabel htmlFor="prompt-template">Prompt</FieldLabel>
                  <Textarea
                    ref={promptRef}
                    id="prompt-template"
                    className="min-h-72 resize-y font-mono text-xs leading-relaxed"
                    value={draft.promptTemplate}
                    placeholder={"Investigate ${{ event.issue.title }} and propose a fix."}
                    aria-invalid={!!errors.promptTemplate}
                    onFocus={() => setActiveTemplate("prompt")}
                    onChange={(event) => update("promptTemplate", event.target.value)}
                  />
                  {fieldError(errors, "promptTemplate")}
                </Field>
                <Field data-invalid={!!errors.titleTemplate}>
                  <FieldLabel htmlFor="title-template">Title template (optional)</FieldLabel>
                  <Input
                    ref={titleRef}
                    id="title-template"
                    className="font-mono"
                    value={draft.titleTemplate}
                    placeholder={"Triage #${{ event.issue.number }}"}
                    aria-invalid={!!errors.titleTemplate}
                    onFocus={() => setActiveTemplate("title")}
                    onChange={(event) => update("titleTemplate", event.target.value)}
                  />
                  {fieldError(errors, "titleTemplate")}
                </Field>
                <Field orientation="horizontal">
                  <div>
                    <FieldLabel htmlFor="include-event-context">Include event context</FieldLabel>
                    <FieldDescription>
                      Append the complete redacted payload in a clearly marked, untrusted context
                      block.
                    </FieldDescription>
                  </div>
                  <Switch
                    id="include-event-context"
                    checked={draft.includeEventContext}
                    onCheckedChange={(checked) => update("includeEventContext", checked)}
                  />
                </Field>
                <Field orientation="horizontal">
                  <div>
                    <FieldLabel htmlFor="plan-first">Plan first</FieldLabel>
                    <FieldDescription>
                      The session designs a plan before implementing. Automation plans auto-approve
                      and stay in the transcript as a reviewable record.
                    </FieldDescription>
                  </div>
                  <Switch
                    id="plan-first"
                    checked={draft.planFirst}
                    onCheckedChange={(checked) => update("planFirst", checked)}
                  />
                </Field>
              </div>

              <aside
                className="min-h-0 rounded-lg border bg-muted/25 p-3"
                aria-label="Template variables"
              >
                <div className="flex items-center gap-2 border-b pb-3">
                  <Code2Icon className="size-4 text-muted-foreground" />
                  <div>
                    <p className="text-sm font-medium">Variables</p>
                    <p className="text-[0.7rem] text-muted-foreground">
                      Inserting into {activeTemplate}
                    </p>
                  </div>
                </div>
                {draft.triggerKind === "cron" ? (
                  <div className="mt-3 space-y-1">
                    <VariableButton path="trigger.scheduled_for" onInsert={insertVariable} />
                    <VariableButton path="trigger.automation.name" onInsert={insertVariable} />
                  </div>
                ) : (
                  <div className="mt-3 max-h-[31rem] space-y-4 overflow-y-auto pr-1">
                    <div>
                      <div className="mb-1.5 flex items-center gap-2">
                        <SparklesIcon className="size-3.5 text-primary" />
                        <p className="text-xs font-medium">Recommended</p>
                      </div>
                      {(events.data?.variables.length ?? 0) > 0 ? (
                        <div className="space-y-1">
                          {events.data?.variables.map((variable) => (
                            <VariableButton
                              key={variable.alias}
                              path={`event.${variable.alias}`}
                              detail={variable.path}
                              onInsert={insertVariable}
                            />
                          ))}
                        </div>
                      ) : (
                        <p className="text-xs text-muted-foreground">
                          No curated variables for this provider.
                        </p>
                      )}
                    </div>
                    <div>
                      <div className="mb-1.5 flex items-center gap-2">
                        <FileJson2Icon className="size-3.5 text-muted-foreground" />
                        <p className="text-xs font-medium">Latest sample · event.raw</p>
                      </div>
                      {raw.length > 0 ? (
                        <div className="space-y-0.5">
                          {raw.map((variable) => (
                            <VariableButton
                              key={variable.path}
                              path={`event.raw.${variable.path}`}
                              depth={Math.min(variable.depth, 4)}
                              onInsert={insertVariable}
                            />
                          ))}
                        </div>
                      ) : (
                        <p className="text-xs text-muted-foreground">
                          Select a captured sample to inspect its redacted payload.
                        </p>
                      )}
                    </div>
                  </div>
                )}
              </aside>
            </div>
          </CardContent>
        </Card>

        <PreviewPanel
          preview={preview}
          pending={testRender.isPending}
          requestError={previewRequestError}
        />
      </div>

      {mode === "edit" && id && <RunHistory automationId={id} />}
    </div>
  );
}

function VariableButton({
  path,
  detail,
  depth = 0,
  onInsert,
}: {
  path: string;
  detail?: string;
  depth?: number;
  onInsert: (path: string) => void;
}) {
  return (
    <button
      type="button"
      className="block w-full rounded px-2 py-1.5 text-left hover:bg-accent focus-visible:ring-2 focus-visible:ring-ring/50 focus-visible:outline-none"
      style={{ paddingLeft: `${8 + depth * 6}px` }}
      title={`Insert \${{ ${path} }}`}
      onClick={() => onInsert(path)}
    >
      <span className="block truncate font-mono text-[0.7rem]">{path}</span>
      {detail && (
        <span className="block truncate text-[0.65rem] text-muted-foreground">{detail}</span>
      )}
    </button>
  );
}
