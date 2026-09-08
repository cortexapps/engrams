/** Settings tab (ADR 0119 phase 3.8): concurrency, deadline, session
 * teardown, archive, duplicate — plus the version history.
 *
 * Built-in editing model: settings are part of the locked graph, so on a
 * built-in they render read-only with the "set by the built-in" hint; only
 * name/description (owned by the editor header) stay editable, and Archive
 * is disabled. Duplicate remains the escape hatch.
 */

import { useEffect, useState } from "react";
import { useNavigate, useParams } from "@tanstack/react-router";
import { toast } from "sonner";

import { SkeletonRows } from "@/components/skeleton-rows";
import { errorMessage } from "@/lib/errors";

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
import {
  useArchiveAutomationV2,
  useDuplicateAutomation,
  useEditorAutomation,
  useUpdateAutomationMetaV2,
} from "@/hooks/useAutomationEditor";
import { parseDefinition, type AutomationDefinition } from "@/lib/automation-blocks";

import { VersionsList } from "./VersionsList";

type Settings = AutomationDefinition["settings"];
type ConcurrencyPolicy = NonNullable<Settings["concurrency"]>["policy"];

const POLICIES: Array<{ value: ConcurrencyPolicy; label: string; help: string }> = [
  { value: "queue", label: "Queue", help: "Later deliveries wait for the active run to finish." },
  {
    value: "supersede",
    label: "Supersede",
    help: "A later delivery stops the active run and takes its place.",
  },
  { value: "skip", label: "Skip", help: "Later deliveries are recorded as filtered, never run." },
  {
    value: "join",
    label: "Join",
    help: "Later deliveries are handed to the active run (Wait for event).",
  },
];

const NONE = "__none__";

export interface SettingsTabProps {
  /** Route param override for tests. */
  automationId?: string;
}

export function settingsFromDraft(draft: {
  policy: string;
  keyTemplate: string;
  deadlineMinutes: string;
  endSessionsOnFinish: boolean;
}): { settings?: Settings; error?: string } {
  const settings: Settings = { endSessionsOnFinish: draft.endSessionsOnFinish };
  if (draft.policy !== NONE) {
    const keyTemplate = draft.keyTemplate.trim();
    if (!keyTemplate) return { error: "A concurrency key template is required." };
    settings.concurrency = { keyTemplate, policy: draft.policy as ConcurrencyPolicy };
  }
  if (draft.deadlineMinutes.trim() !== "") {
    const minutes = Number(draft.deadlineMinutes);
    if (!Number.isInteger(minutes) || minutes < 1 || minutes > 48 * 60) {
      return { error: "Run deadline must be a whole number of minutes between 1 and 2880." };
    }
    settings.runDeadlineSeconds = minutes * 60;
  }
  return { settings };
}

export function SettingsTab({ automationId }: SettingsTabProps) {
  const params = useParams({ strict: false }) as { id?: string };
  const id = automationId ?? params.id;
  const navigate = useNavigate();
  const query = useEditorAutomation(id);
  const automation = query.data?.automation;
  const builtin = automation?.kind === "builtin";
  const settings = parseDefinition(automation?.version?.definitionJson).settings;

  const [policy, setPolicy] = useState<string>(NONE);
  const [keyTemplate, setKeyTemplate] = useState("");
  const [deadlineMinutes, setDeadlineMinutes] = useState("");
  const [endSessionsOnFinish, setEndSessionsOnFinish] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [loadedVersion, setLoadedVersion] = useState<string | null>(null);

  useEffect(() => {
    if (!automation) return;
    const key = `${automation.id}:${automation.currentVersion}`;
    if (key === loadedVersion) return;
    setLoadedVersion(key);
    setPolicy(settings.concurrency?.policy ?? NONE);
    setKeyTemplate(settings.concurrency?.keyTemplate ?? "");
    setDeadlineMinutes(
      settings.runDeadlineSeconds !== undefined ? String(settings.runDeadlineSeconds / 60) : "",
    );
    setEndSessionsOnFinish(settings.endSessionsOnFinish);
    setError(null);
  }, [automation, settings, loadedVersion]);

  const updateMeta = useUpdateAutomationMetaV2();
  const archive = useArchiveAutomationV2();
  const duplicate = useDuplicateAutomation();

  if (!id) return null;
  if (!automation) {
    return <SkeletonRows rows={4} />;
  }

  const dirty =
    !builtin &&
    (policy !== (settings.concurrency?.policy ?? NONE) ||
      keyTemplate !== (settings.concurrency?.keyTemplate ?? "") ||
      deadlineMinutes !==
        (settings.runDeadlineSeconds !== undefined
          ? String(settings.runDeadlineSeconds / 60)
          : "") ||
      endSessionsOnFinish !== settings.endSessionsOnFinish);

  const save = async () => {
    const parsed = settingsFromDraft({ policy, keyTemplate, deadlineMinutes, endSessionsOnFinish });
    if (parsed.error) {
      setError(parsed.error);
      return;
    }
    setError(null);
    try {
      await updateMeta.mutateAsync({ id, settingsJson: JSON.stringify(parsed.settings) });
      toast.success("Settings saved");
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    }
  };

  // Both mutations surface failures: the confirm dialog closes on click
  // regardless of outcome, so a swallowed rejection would leave the operator
  // believing an archive happened while the automation keeps firing.
  const onArchive = async () => {
    try {
      await archive.mutateAsync({ id });
    } catch (err) {
      toast.error(errorMessage(err));
      return;
    }
    toast.success("Automation archived");
    void navigate({ to: "/automations" });
  };

  const onDuplicate = async () => {
    let copy;
    try {
      copy = (await duplicate.mutateAsync({ automationId: id })).automation;
    } catch (err) {
      toast.error(errorMessage(err));
      return;
    }
    if (copy) {
      toast.success("Duplicated");
      void navigate({
        to: "/automations/$id",
        params: { id: copy.id },
        search: { tab: "build" },
      });
    }
  };

  const lockedHint = builtin ? <FieldDescription>Set by the built-in.</FieldDescription> : null;

  return (
    <div className="flex flex-col gap-8">
      <section className="flex max-w-xl flex-col gap-4" aria-label="run settings">
        <h3 className="text-sm font-semibold">Run settings</h3>
        <Field>
          <FieldLabel htmlFor="settings-policy">Concurrency</FieldLabel>
          <Select value={policy} onValueChange={setPolicy} disabled={builtin}>
            <SelectTrigger id="settings-policy" data-testid="settings-policy">
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              <SelectItem value={NONE}>None — every delivery runs</SelectItem>
              {POLICIES.map((p) => (
                <SelectItem key={p.value} value={p.value}>
                  {p.label}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
          {lockedHint ?? (
            <FieldDescription>
              {POLICIES.find((p) => p.value === policy)?.help ?? "Runs never wait on one another."}
            </FieldDescription>
          )}
        </Field>
        {policy !== NONE && (
          <Field>
            <FieldLabel htmlFor="settings-key">Concurrency key</FieldLabel>
            <Input
              id="settings-key"
              value={keyTemplate}
              onChange={(e) => setKeyTemplate(e.target.value)}
              placeholder="${{ event.pull_request.html_url }}"
              disabled={builtin}
            />
            {lockedHint ?? (
              <FieldDescription>
                A template rendered from the trigger; runs with the same key share the policy.
              </FieldDescription>
            )}
          </Field>
        )}
        <Field>
          <FieldLabel htmlFor="settings-deadline">Run deadline (minutes)</FieldLabel>
          <Input
            id="settings-deadline"
            inputMode="numeric"
            value={deadlineMinutes}
            onChange={(e) => setDeadlineMinutes(e.target.value)}
            placeholder="No deadline"
            disabled={builtin}
          />
          {lockedHint ?? (
            <FieldDescription>
              The whole run ends with status “deadline” when this elapses.
            </FieldDescription>
          )}
        </Field>
        <Field orientation="horizontal">
          <Switch
            id="settings-end-sessions"
            checked={endSessionsOnFinish}
            onCheckedChange={setEndSessionsOnFinish}
            disabled={builtin}
          />
          <FieldLabel htmlFor="settings-end-sessions">
            End sessions when the run finishes
          </FieldLabel>
        </Field>
        {!builtin && (
          <FieldDescription>
            Off keeps every session the run created so a person can pick it up.
          </FieldDescription>
        )}
        {error && <FieldError>{error}</FieldError>}
        {!builtin && (
          <div>
            <Button onClick={() => void save()} disabled={!dirty || updateMeta.isPending}>
              Save settings
            </Button>
          </div>
        )}
      </section>

      <VersionsList automationId={id} builtin={builtin} />

      <section className="flex flex-col gap-3" aria-label="danger zone">
        <h3 className="text-sm font-semibold">Manage</h3>
        <div className="flex gap-2">
          <Button
            variant="outline"
            onClick={() => void onDuplicate()}
            disabled={duplicate.isPending}
          >
            Duplicate
          </Button>
          <AlertDialog>
            <AlertDialogTrigger asChild>
              <Button variant="destructive" disabled={builtin || archive.isPending}>
                Archive
              </Button>
            </AlertDialogTrigger>
            <AlertDialogContent>
              <AlertDialogHeader>
                <AlertDialogTitle>Archive “{automation.name}”?</AlertDialogTitle>
                <AlertDialogDescription>
                  The automation stops firing and leaves the list. Its run history stays.
                </AlertDialogDescription>
              </AlertDialogHeader>
              <AlertDialogFooter>
                <AlertDialogCancel>Cancel</AlertDialogCancel>
                <AlertDialogAction onClick={() => void onArchive()}>Archive</AlertDialogAction>
              </AlertDialogFooter>
            </AlertDialogContent>
          </AlertDialog>
        </div>
        {builtin && (
          <FieldDescription>Built-ins cannot be archived; disable them instead.</FieldDescription>
        )}
      </section>
    </div>
  );
}
