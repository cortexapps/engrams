/** The Agent block's inspector: who runs (the profile), what it gets on
 * every event (the prompt), whether the agent keeps one session, and
 * where it talks back. The common case is three controls; the relay's six
 * templates and the session's finer settings sit behind disclosures. */

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
  getPath,
  isTunable,
  setPath,
  type BlockDef,
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
  block: BlockDef;
  onChange: (next: BlockDef) => void;
  builtin: boolean;
  errors: readonly BlockErrorRef[];
  sessionSources: readonly string[];
  variablePaths: readonly string[];
  variableValues?: Readonly<Record<string, string>>;
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

const SESSION_MODES: Array<{ value: "workstream" | "never"; label: string; help: string }> = [
  {
    value: "workstream",
    label: "One session per agent",
    help: "Continues the agent's kept session when it is alive; starts one when it is not.",
  },
  {
    value: "never",
    label: "A new session every time",
    help: "Every event starts its own session.",
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
}: {
  label: string;
  children: React.ReactNode;
  testId: string;
}) {
  const [open, setOpen] = useState(false);
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
  const { block, onChange, builtin, errors } = props;
  const profiles = useProfiles();
  const harnesses = useHarnessCatalog();
  const profile = profiles.data?.profiles.find((p) => p.id === block.config["profileId"]);
  const editable = (key: string) => (builtin ? isTunable(block, key) : true);
  const set = (key: string, value: unknown) =>
    onChange({ ...block, config: setPath(block.config, key, value) });

  const field = (spec: FieldSpec) => (
    <GenericField
      key={spec.key}
      spec={spec}
      value={getPath(block.config, spec.key)}
      onChange={(value) => set(spec.key, value)}
      pinned={!editable(spec.key)}
      error={errorFor(errors, spec.key)}
      sessionSources={props.sessionSources}
      variablePaths={props.variablePaths}
      variableValues={props.variableValues}
    />
  );

  const reuse = block.config["reuse"] === "never" ? "never" : "workstream";
  const relay = block.config["relay"];
  const relayOn =
    typeof relay === "object" &&
    relay !== null &&
    Object.values(relay as Record<string, unknown>).some((v) => typeof v === "string" && v !== "");
  const relayPinned = !editable("relay");

  const override: HarnessOverride = useMemo(
    () => ({
      harness: (block.config["harness"] as string | undefined) ?? null,
      model: (block.config["model"] as string | undefined) ?? null,
      modelRouter: (block.config["modelRouter"] as string | undefined) ?? null,
      effort: (block.config["effort"] as string | undefined) ?? null,
      mode: (block.config["harnessMode"] as string | undefined) ?? null,
    }),
    [block.config],
  );
  const setOverride = (next: HarnessOverride) => {
    let config = block.config;
    const assign = (key: string, value: string | null | undefined) => {
      config = setPath(config, key, value ?? undefined);
    };
    assign("harness", next.harness);
    assign("model", next.model);
    assign("modelRouter", next.modelRouter === null ? undefined : next.modelRouter);
    assign("effort", next.effort);
    assign("harnessMode", next.mode);
    onChange({ ...block, config });
  };
  const harnessPinned = !editable("harness");

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

      <Field data-testid="field-reuse">
        <FieldLabel className="flex items-center gap-1.5">
          Session
          {!editable("reuse") && <Pinned />}
        </FieldLabel>
        <Select
          value={reuse}
          onValueChange={(next) => set("reuse", next)}
          disabled={!editable("reuse")}
        >
          <SelectTrigger aria-label="Session">
            <SelectValue />
          </SelectTrigger>
          <SelectContent>
            {SESSION_MODES.map((mode) => (
              <SelectItem key={mode.value} value={mode.value}>
                {mode.label}
              </SelectItem>
            ))}
          </SelectContent>
        </Select>
        <FieldDescription>{SESSION_MODES.find((m) => m.value === reuse)!.help}</FieldDescription>
      </Field>

      <Field data-testid="field-relay">
        <div className="flex items-center justify-between gap-3">
          <FieldLabel htmlFor={`${block.id}-relay`} className="flex items-center gap-1.5">
            Reply in the Slack thread the event came from
            {relayPinned && <Pinned />}
          </FieldLabel>
          <Switch
            id={`${block.id}-relay`}
            checked={relayOn}
            disabled={relayPinned}
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

      <Disclosure label="More" testId="agent-more">
        {MORE_FIELDS.map(field)}
        <Field>
          <FieldLabel className="flex items-center gap-1.5">
            Harness override
            {harnessPinned && <Pinned />}
          </FieldLabel>
          <SessionHarnessControls
            harnesses={harnesses.data}
            profileHarness={profile?.harness || undefined}
            profileModel={profile?.model || undefined}
            value={override}
            onChange={setOverride}
            disabled={harnessPinned}
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
