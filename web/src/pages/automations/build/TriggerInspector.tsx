/** Trigger editing (ADR 0119 phase 3.3): integration event picker from the
 * catalog, cron schedule, custom generic webhook, or manual. The trigger
 * lives in `definition_json`; a built-in's trigger is pinned except its
 * scope, which an input usually supplies. */

import { Lock } from "lucide-react";
import { useEffect } from "react";

import { Badge } from "@/components/ui/badge";
import { Field, FieldDescription, FieldError, FieldLabel } from "@/components/ui/field";
import { Input } from "@/components/ui/input";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { Switch } from "@/components/ui/switch";
import { useEditorWebhookRegistrations, useEventCatalog } from "@/hooks/useAutomationEditor";
import type { BlockErrorRef, TriggerSpec } from "@/lib/automation-blocks";

const PROVIDERS = ["github", "slack", "linear"] as const;
const KINDS: ReadonlyArray<{ kind: TriggerSpec["kind"]; label: string; help: string }> = [
  {
    kind: "integration",
    label: "Integration event",
    help: "An event from a connected integration (GitHub, Slack, Linear).",
  },
  { kind: "cron", label: "Schedule", help: "A cron schedule in an IANA timezone." },
  {
    kind: "webhook",
    label: "Custom webhook",
    help: "A generic signed webhook for systems without a connector.",
  },
  { kind: "manual", label: "Manual", help: "Only runs from the Run now button." },
];

export interface TriggerInspectorProps {
  trigger: TriggerSpec;
  onChange: (next: TriggerSpec) => void;
  /** Built-in: the whole trigger is pinned, scope included (narrow via the
   * Inputs tab when the scope reads from an input). */
  builtin: boolean;
  errors: readonly BlockErrorRef[];
}

function errorFor(errors: readonly BlockErrorRef[], field: string): string | undefined {
  return errors.find((e) => e.field === field || e.field === `trigger.${field}`)?.message;
}

function Pinned() {
  return (
    <span className="text-muted-foreground inline-flex items-center gap-1 text-xs font-normal">
      <Lock className="size-3" aria-hidden /> Set by the built-in
    </span>
  );
}

export function TriggerInspector({ trigger, onChange, builtin, errors }: TriggerInspectorProps) {
  const defaultFor = (kind: TriggerSpec["kind"]): TriggerSpec => {
    switch (kind) {
      case "integration":
        return { kind, provider: "github", connectionId: "", eventKeys: [] };
      case "cron":
        return { kind, schedule: "0 9 * * 1-5", timezone: "UTC" };
      case "webhook":
        return { kind, registrationId: "", events: [] };
      case "manual":
        return { kind };
    }
  };

  return (
    <div className="flex flex-col gap-4" data-testid="trigger-inspector">
      <Field data-testid="field-trigger.kind">
        <FieldLabel className="flex items-center gap-1.5">Kind {builtin && <Pinned />}</FieldLabel>
        <Select
          value={trigger.kind}
          onValueChange={(kind) => onChange(defaultFor(kind as TriggerSpec["kind"]))}
          disabled={builtin}
        >
          <SelectTrigger>
            <SelectValue />
          </SelectTrigger>
          <SelectContent>
            {KINDS.map((k) => (
              <SelectItem key={k.kind} value={k.kind}>
                {k.label}
              </SelectItem>
            ))}
          </SelectContent>
        </Select>
        <FieldDescription>{KINDS.find((k) => k.kind === trigger.kind)?.help}</FieldDescription>
      </Field>

      {trigger.kind === "integration" && (
        <IntegrationTrigger
          trigger={trigger}
          onChange={onChange}
          builtin={builtin}
          errors={errors}
        />
      )}
      {trigger.kind === "cron" && (
        <CronTrigger trigger={trigger} onChange={onChange} builtin={builtin} errors={errors} />
      )}
      {trigger.kind === "webhook" && (
        <WebhookTrigger trigger={trigger} onChange={onChange} builtin={builtin} errors={errors} />
      )}
    </div>
  );
}

function IntegrationTrigger({ trigger, onChange, builtin, errors }: TriggerInspectorProps) {
  const provider = typeof trigger["provider"] === "string" ? trigger["provider"] : "";
  const eventKeys = Array.isArray(trigger["eventKeys"]) ? (trigger["eventKeys"] as string[]) : [];
  const scope = trigger["scope"] as { values?: string[]; fromInput?: string } | undefined;
  const catalog = useEventCatalog(provider || undefined);
  const visible = (catalog.data?.events ?? []).filter((e) => !e.hidden);

  const toggle = (key: string, on: boolean) =>
    onChange({
      ...trigger,
      eventKeys: on ? [...eventKeys, key] : eventKeys.filter((k) => k !== key),
    });

  // The default connection id comes with the catalog; pin it onto the
  // trigger so the definition validates.
  const connectionId = catalog.data?.defaultConnectionId ?? "";
  useEffect(() => {
    if (connectionId && trigger["connectionId"] !== connectionId && !builtin) {
      onChange({ ...trigger, connectionId });
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps -- only the id matters
  }, [connectionId, builtin]);

  const scopeMode =
    scope?.fromInput !== undefined ? "input" : scope?.values !== undefined ? "values" : "any";

  return (
    <>
      <Field data-testid="field-trigger.provider">
        <FieldLabel className="flex items-center gap-1.5">
          Integration {builtin && <Pinned />}
        </FieldLabel>
        <Select
          value={provider}
          onValueChange={(p) =>
            onChange({ ...trigger, provider: p, connectionId: "", eventKeys: [], scope: undefined })
          }
          disabled={builtin}
        >
          <SelectTrigger>
            <SelectValue placeholder="Choose…" />
          </SelectTrigger>
          <SelectContent>
            {PROVIDERS.map((p) => (
              <SelectItem key={p} value={p}>
                {p}
              </SelectItem>
            ))}
          </SelectContent>
        </Select>
      </Field>

      <Field
        data-invalid={errorFor(errors, "eventKeys") ? true : undefined}
        data-testid="field-trigger.eventKeys"
      >
        <FieldLabel className="flex items-center gap-1.5">
          Events {builtin && <Pinned />}
        </FieldLabel>
        {catalog.isLoading && <p className="text-muted-foreground text-sm">Loading catalog…</p>}
        <ul className="flex flex-col gap-1.5">
          {visible.map((event) => {
            const on = eventKeys.includes(event.key);
            return (
              <li key={event.key} className="flex items-start gap-2">
                <Switch
                  id={`ev-${event.key}`}
                  checked={on}
                  onCheckedChange={(next) => toggle(event.key, next)}
                  disabled={builtin}
                  aria-label={event.label}
                />
                <label htmlFor={`ev-${event.key}`} className="flex min-w-0 flex-col text-sm">
                  <span className="flex items-center gap-2">
                    {event.label}
                    <code className="text-muted-foreground text-xs">{event.key}</code>
                    {event.observed && <Badge variant="secondary">seen</Badge>}
                  </span>
                  {event.description && (
                    <span className="text-muted-foreground text-xs">{event.description}</span>
                  )}
                </label>
              </li>
            );
          })}
        </ul>
        {errorFor(errors, "eventKeys") && <FieldError>{errorFor(errors, "eventKeys")}</FieldError>}
      </Field>

      {catalog.data?.scope && (
        <Field data-testid="field-trigger.scope">
          <FieldLabel>{catalog.data.scope.label}</FieldLabel>
          <Select
            value={scopeMode}
            disabled={builtin}
            onValueChange={(mode) =>
              onChange({
                ...trigger,
                scope:
                  mode === "any"
                    ? undefined
                    : mode === "values"
                      ? { values: scope?.values ?? [] }
                      : { fromInput: scope?.fromInput ?? "" },
              })
            }
          >
            <SelectTrigger>
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              <SelectItem value="any">Any</SelectItem>
              <SelectItem value="values">Only these</SelectItem>
              <SelectItem value="input">From an input</SelectItem>
            </SelectContent>
          </Select>
          {scopeMode === "values" && (
            <Input
              className="font-mono text-sm"
              placeholder="owner/repo, owner/other"
              value={(scope?.values ?? []).join(", ")}
              disabled={builtin}
              onChange={(e) =>
                onChange({
                  ...trigger,
                  scope: {
                    values: e.target.value
                      .split(",")
                      .map((s) => s.trim())
                      .filter(Boolean),
                  },
                })
              }
            />
          )}
          {scopeMode === "input" && (
            <Input
              className="font-mono text-sm"
              placeholder="repos"
              value={scope?.fromInput ?? ""}
              disabled={builtin}
              onChange={(e) => onChange({ ...trigger, scope: { fromInput: e.target.value } })}
            />
          )}
          <FieldDescription>
            {builtin
              ? scopeMode === "input"
                ? `Set by the built-in: the keys of the "${scope?.fromInput ?? ""}" input narrow which deliveries match — edit that input on the Inputs tab.`
                : "Set by the built-in."
              : scopeMode === "input"
                ? "The keys of a map input (or items of a list input) narrow which deliveries match."
                : "Narrow deliveries to a subset."}
          </FieldDescription>
        </Field>
      )}
    </>
  );
}

function CronTrigger({ trigger, onChange, builtin, errors }: TriggerInspectorProps) {
  return (
    <>
      <Field
        data-invalid={errorFor(errors, "schedule") ? true : undefined}
        data-testid="field-trigger.schedule"
      >
        <FieldLabel className="flex items-center gap-1.5">
          Schedule {builtin && <Pinned />}
        </FieldLabel>
        <Input
          className="font-mono"
          value={typeof trigger["schedule"] === "string" ? trigger["schedule"] : ""}
          onChange={(e) => onChange({ ...trigger, schedule: e.target.value })}
          disabled={builtin}
        />
        {errorFor(errors, "schedule") ? (
          <FieldError>{errorFor(errors, "schedule")}</FieldError>
        ) : (
          <FieldDescription>
            Five-field cron, e.g. 0 9 * * 1-5 (weekdays at 09:00).
          </FieldDescription>
        )}
      </Field>
      <Field
        data-invalid={errorFor(errors, "timezone") ? true : undefined}
        data-testid="field-trigger.timezone"
      >
        <FieldLabel className="flex items-center gap-1.5">
          Timezone {builtin && <Pinned />}
        </FieldLabel>
        <Input
          value={typeof trigger["timezone"] === "string" ? trigger["timezone"] : ""}
          onChange={(e) => onChange({ ...trigger, timezone: e.target.value })}
          disabled={builtin}
        />
        {errorFor(errors, "timezone") && <FieldError>{errorFor(errors, "timezone")}</FieldError>}
      </Field>
    </>
  );
}

function WebhookTrigger({ trigger, onChange, builtin, errors }: TriggerInspectorProps) {
  const registrations = useEditorWebhookRegistrations();
  const events = Array.isArray(trigger["events"]) ? (trigger["events"] as string[]) : [];
  return (
    <>
      <Field
        data-invalid={errorFor(errors, "registrationId") ? true : undefined}
        data-testid="field-trigger.registrationId"
      >
        <FieldLabel className="flex items-center gap-1.5">
          Registration {builtin && <Pinned />}
        </FieldLabel>
        <Select
          value={typeof trigger["registrationId"] === "string" ? trigger["registrationId"] : ""}
          onValueChange={(id) => onChange({ ...trigger, registrationId: id })}
          disabled={builtin}
        >
          <SelectTrigger>
            <SelectValue placeholder="Choose a webhook…" />
          </SelectTrigger>
          <SelectContent>
            {(registrations.data?.registrations ?? []).map((r) => (
              <SelectItem key={r.id} value={r.id}>
                {r.name}{" "}
                <code className="text-muted-foreground ml-2 text-xs">/api/v1/hooks/{r.id}</code>
              </SelectItem>
            ))}
          </SelectContent>
        </Select>
        {errorFor(errors, "registrationId") && (
          <FieldError>{errorFor(errors, "registrationId")}</FieldError>
        )}
        <FieldDescription>Manage webhooks from the Automations list page.</FieldDescription>
      </Field>
      <Field
        data-invalid={errorFor(errors, "events") ? true : undefined}
        data-testid="field-trigger.events"
      >
        <FieldLabel className="flex items-center gap-1.5">
          Event keys {builtin && <Pinned />}
        </FieldLabel>
        <Input
          className="font-mono text-sm"
          placeholder="deploy.finished, deploy.failed"
          value={events.join(", ")}
          onChange={(e) =>
            onChange({
              ...trigger,
              events: e.target.value
                .split(",")
                .map((s) => s.trim())
                .filter(Boolean),
            })
          }
          disabled={builtin}
        />
        {errorFor(errors, "events") ? (
          <FieldError>{errorFor(errors, "events")}</FieldError>
        ) : (
          <FieldDescription>
            The X-Engrams-Event values this automation responds to.
          </FieldDescription>
        )}
      </Field>
    </>
  );
}
