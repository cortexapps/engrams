/** The agent's inspector: who runs (the profile), what it gets on every
 * event (the prompt), how many agents there are (the scope), and where it
 * talks back (the relay). The common case is three controls; the relay's
 * templates, a custom key, and the session's finer settings sit behind
 * disclosures. Edits land on the definition's `agent`, not on a block. */

import { ChevronDown, Lock } from "lucide-react";
import { useMemo, useState } from "react";

import { Collapsible, CollapsibleContent, CollapsibleTrigger } from "@/components/ui/collapsible";
import { Field, FieldDescription, FieldLabel } from "@/components/ui/field";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { Switch } from "@/components/ui/switch";
import { useHarnessCatalog } from "@/hooks/useHarnessCatalog";
import { useProfiles } from "@/hooks/useProfiles";
import {
  CUSTOM_SCOPE_ID,
  getPath,
  SCOPE_PRESETS,
  scopePresetOf,
  setPath,
  type AgentScope,
  type AgentSpec,
  type BlockErrorRef,
  type FieldSpec,
} from "@/lib/automation-blocks";
import { cn } from "@/lib/utils";
import {
  SessionHarnessControls,
  type HarnessOverride,
} from "@/pages/sessions/SessionHarnessControls";

import { GenericField } from "../fields/GenericField";

export interface AgentInspectorProps {
  agent: AgentSpec;
  onChange: (next: AgentSpec) => void;
  /** Built-in: the agent is set by the built-in; every field is pinned. */
  builtin: boolean;
  errors: readonly BlockErrorRef[];
  variablePaths: readonly string[];
  variableValues?: Readonly<Record<string, string>>;
  /** A way in is a Slack event: the thread relay has a thread to read. The
   * switch shows only then (or when a relay is already set). */
  slackTrigger: boolean;
}

/** The relay that answers in the Slack thread an event came from: every
 * field reads the mention or message payload the Slack connector delivers. */
export const SLACK_THREAD_RELAY: Record<string, string> = {
  provider: "slack",
  team: "${{ event.raw.team_id }}",
  channel: "${{ event.raw.event.channel }}",
  threadTs: "${{ event.raw.event.thread_ts | default: event.raw.event.ts }}",
  mentionTs: "${{ event.raw.event.ts }}",
  userId: "${{ event.raw.event.user }}",
  eventId: "${{ event.raw.event_id }}",
};

const RELAY_FIELDS: FieldSpec[] = [
  { type: "template", key: "relay.team", label: "Team id" },
  { type: "template", key: "relay.channel", label: "Channel id" },
  {
    type: "template",
    key: "relay.threadTs",
    label: "Thread",
    help: "The thread root; a top-level message is its own root.",
  },
  {
    type: "template",
    key: "relay.mentionTs",
    label: "Message",
    help: "The message this turn reacts on; defaults to the thread root.",
  },
  { type: "template", key: "relay.userId", label: "Author id" },
  { type: "template", key: "relay.eventId", label: "Event id" },
];

const KEY_FIELDS: FieldSpec[] = [
  {
    type: "template",
    key: "scope.keyTemplate",
    label: "Key",
    help: "Rendered from the event; events that render the same key reach the same agent.",
  },
  {
    type: "template",
    key: "scope.labelTemplate",
    label: "Name",
    help: "What the agent is called on the Agents tab; empty means the key.",
  },
];

const MORE_FIELDS: FieldSpec[] = [
  {
    type: "template",
    key: "openingPromptTemplate",
    label: "Opening prompt",
    multiline: true,
    help: "The first prompt of a new session; empty means the prompt above.",
  },
  {
    type: "template",
    key: "titleTemplate",
    label: "Task title",
    help: "Optional; defaults to the prompt.",
  },
  {
    type: "boolean",
    key: "includeEventContext",
    label: "Append event context",
    help: "Adds the redacted trigger payload to the prompt, marked as untrusted data.",
  },
  {
    type: "boolean",
    key: "keepOnFinish",
    label: "Keep the session when the run ends",
    help: "Kept by default; a session the agent continues must be kept.",
  },
];

function errorFor(errors: readonly BlockErrorRef[], field: string): string | undefined {
  return errors.find((e) => e.field === field || e.field.startsWith(`${field}.`))?.message;
}

function Pinned() {
  return (
    <span className="inline-flex items-center gap-1 text-xs font-normal text-muted-foreground">
      <Lock className="size-3" aria-hidden /> Set by the built-in
    </span>
  );
}

function Disclosure({
  label,
  children,
  testId,
  defaultOpen = false,
}: {
  label: string;
  children: React.ReactNode;
  testId: string;
  defaultOpen?: boolean;
}) {
  const [open, setOpen] = useState(defaultOpen);
  return (
    <Collapsible open={open} onOpenChange={setOpen} data-testid={testId}>
      <CollapsibleTrigger asChild>
        <button
          type="button"
          className="flex w-full items-center gap-1.5 py-1 text-left text-sm font-medium"
          aria-expanded={open}
        >
          <ChevronDown
            className={cn(
              "size-4 text-muted-foreground transition-transform",
              !open && "-rotate-90",
            )}
            aria-hidden
          />
          {label}
        </button>
      </CollapsibleTrigger>
      <CollapsibleContent className="flex flex-col gap-3 pt-2">{children}</CollapsibleContent>
    </Collapsible>
  );
}

export function AgentInspector(props: AgentInspectorProps) {
  const { agent, onChange, builtin, errors } = props;
  const profiles = useProfiles();
  const harnesses = useHarnessCatalog();
  const profile = profiles.data?.profiles.find((p) => p.id === agent.profileId);
  const pinned = builtin;
  const set = (key: string, value: unknown) => onChange(setPath(agent, key, value) as AgentSpec);

  // `keepOnFinish` is kept when absent: the switch reads the default.
  const valueOf = (key: string) => {
    const value = getPath(agent, key);
    return key === "keepOnFinish" && value === undefined ? true : value;
  };
  const field = (spec: FieldSpec) => (
    <GenericField
      key={spec.key}
      spec={spec}
      value={valueOf(spec.key)}
      onChange={(value) => set(spec.key, value)}
      pinned={pinned}
      error={errorFor(errors, spec.key)}
      sessionSources={[]}
      variablePaths={props.variablePaths}
      variableValues={props.variableValues}
    />
  );

  const preset = scopePresetOf(agent.scope);
  const setScope = (id: string) => {
    let scope: AgentScope;
    if (id === CUSTOM_SCOPE_ID) {
      scope = agent.scope.kind === "key" ? agent.scope : { kind: "key", keyTemplate: "" };
    } else {
      scope = SCOPE_PRESETS.find((p) => p.id === id)!.scope;
    }
    onChange({ ...agent, scope });
  };
  const scopeHelp =
    preset === CUSTOM_SCOPE_ID
      ? "One agent per rendered key."
      : SCOPE_PRESETS.find((p) => p.id === preset)!.help;

  const relay = agent["relay"];
  const relayOn =
    typeof relay === "object" &&
    relay !== null &&
    Object.values(relay as Record<string, unknown>).some((v) => typeof v === "string" && v !== "");

  const override: HarnessOverride = useMemo(
    () => ({
      harness: (agent["harness"] as string | undefined) ?? null,
      model: (agent["model"] as string | undefined) ?? null,
      modelRouter: (agent["modelRouter"] as string | undefined) ?? null,
      effort: (agent["effort"] as string | undefined) ?? null,
      mode: (agent["harnessMode"] as string | undefined) ?? null,
    }),
    [agent],
  );
  const setOverride = (next: HarnessOverride) => {
    let config: Record<string, unknown> = agent;
    const assign = (key: string, value: string | null | undefined) => {
      config = setPath(config, key, value ?? undefined);
    };
    assign("harness", next.harness);
    assign("model", next.model);
    assign("modelRouter", next.modelRouter === null ? undefined : next.modelRouter);
    assign("effort", next.effort);
    assign("harnessMode", next.mode);
    onChange(config as AgentSpec);
  };

  return (
    <div className="flex flex-col gap-4" data-testid="agent-inspector">
      {field({ type: "profile", key: "profileId", label: "Profile" })}
      {field({
        type: "template",
        key: "promptTemplate",
        label: "Prompt",
        multiline: true,
        help: "What the agent gets on every event. A continued session keeps its history, so say what is new.",
      })}

      <Field data-testid="field-scope">
        <FieldLabel className="flex items-center gap-1.5">
          One agent for
          {pinned && <Pinned />}
        </FieldLabel>
        <Select value={preset} onValueChange={setScope} disabled={pinned}>
          <SelectTrigger aria-label="One agent for">
            <SelectValue />
          </SelectTrigger>
          <SelectContent>
            {SCOPE_PRESETS.map((option) => (
              <SelectItem key={option.id} value={option.id}>
                {option.label}
              </SelectItem>
            ))}
            <SelectItem value={CUSTOM_SCOPE_ID}>A key of your own</SelectItem>
          </SelectContent>
        </Select>
        <FieldDescription>{scopeHelp}</FieldDescription>
        {errorFor(errors, "scope") && (
          <p className="text-xs text-[var(--instrument-critical-ink)]">
            {errorFor(errors, "scope")}
          </p>
        )}
        {agent.scope.kind === "key" && (
          <Disclosure
            label="The key"
            testId="scope-fields"
            defaultOpen={preset === CUSTOM_SCOPE_ID}
          >
            {KEY_FIELDS.map(field)}
          </Disclosure>
        )}
      </Field>

      {(relayOn || props.slackTrigger) && (
        <Field data-testid="field-relay">
          <div className="flex items-center justify-between gap-3">
            <FieldLabel htmlFor="agent-relay" className="flex items-center gap-1.5">
              Reply in the Slack thread the event came from
              {pinned && <Pinned />}
            </FieldLabel>
            <Switch
              id="agent-relay"
              checked={relayOn}
              disabled={pinned}
              onCheckedChange={(on) => set("relay", on ? { ...SLACK_THREAD_RELAY } : undefined)}
            />
          </div>
          <FieldDescription>
            {relayOn
              ? "The agent's replies land in the thread as it works, its questions arrive as Slack forms, and the message gets a reaction while a turn runs."
              : "Off: the session runs without a relay. Turn it on for a Slack mention or message trigger."}
          </FieldDescription>
          {errorFor(errors, "relay") && (
            <p className="text-xs text-[var(--instrument-critical-ink)]">
              {errorFor(errors, "relay")}
            </p>
          )}
          {relayOn && (
            <Disclosure label="Where the relay reads the thread from" testId="relay-fields">
              {RELAY_FIELDS.map(field)}
            </Disclosure>
          )}
        </Field>
      )}

      <Disclosure label="More" testId="agent-more">
        {MORE_FIELDS.map(field)}
        <Field>
          <FieldLabel className="flex items-center gap-1.5">
            Harness override
            {pinned && <Pinned />}
          </FieldLabel>
          <SessionHarnessControls
            harnesses={harnesses.data}
            profileHarness={profile?.harness || undefined}
            profileModel={profile?.model || undefined}
            value={override}
            onChange={setOverride}
            disabled={pinned}
            audience="programmatic"
          />
          <FieldDescription>
            Leave unset to inherit the profile's harness, model, and effort.
          </FieldDescription>
        </Field>
      </Disclosure>
    </div>
  );
}
