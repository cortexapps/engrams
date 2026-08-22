/** The automation editor shell (ADR 0119 phase 3.3).
 *
 * Header (name, enabled, built-in banner + Duplicate) and four tabs driven by
 * the `?tab=` search param. This PR ships Build; Inputs (3.6), Runs (3.7),
 * and Settings (3.8) mount into the tab slots below.
 *
 * Saving follows the editing model: a user automation saves a full new
 * version (SaveVersion); a built-in saves only the changed tunable fields
 * as overrides (SetBlockOverrides) — its structure is never sent. */

import { ConnectError } from "@connectrpc/connect";
import { Link, useNavigate, useParams, useSearch } from "@tanstack/react-router";
import { Copy, Lock } from "lucide-react";
import { useEffect, useMemo, useState, type ReactNode } from "react";
import { toast } from "sonner";

import { PageHeading } from "@/components/page-heading";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Field, FieldError, FieldLabel } from "@/components/ui/field";
import { Input } from "@/components/ui/input";
import { Switch } from "@/components/ui/switch";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import {
  useCreateAutomationV2,
  useDuplicateAutomation,
  useEditorAutomation,
  useSaveVersionV2,
  useSetAutomationEnabledV2,
  useSetBlockOverrides,
  useUpdateAutomationMetaV2,
} from "@/hooks/useAutomationEditor";
import {
  applyOverrides,
  diffOverrides,
  EMPTY_DEFINITION,
  parseBlockErrors,
  parseDefinition,
  parseOverrides,
  type AutomationDefinition,
  type BlockErrorRef,
} from "@/lib/automation-blocks";

import { BuildTab } from "./build/BuildTab";

export const EDITOR_TABS = ["build", "inputs", "runs", "settings"] as const;
export type EditorTab = (typeof EDITOR_TABS)[number];

export function isEditorTab(value: unknown): value is EditorTab {
  return typeof value === "string" && (EDITOR_TABS as readonly string[]).includes(value);
}

interface Props {
  mode: "create" | "edit";
  /** Slots for the sibling PRs (3.6 / 3.7 / 3.8). */
  inputsTab?: ReactNode;
  runsTab?: ReactNode;
  settingsTab?: ReactNode;
  /** 3.4: TestPanel mounted under the Build inspector. */
  testPanel?: ReactNode;
  variableValues?: Readonly<Record<string, string>>;
}

function Placeholder({ item }: { item: string }) {
  return (
    <p className="text-muted-foreground rounded-lg border border-dashed p-6 text-sm">
      {/* TODO: filled in by stack item {item}. */}
      Coming in {item}.
    </p>
  );
}

export function AutomationEditor({
  mode,
  inputsTab,
  runsTab,
  settingsTab,
  testPanel,
  variableValues,
}: Props) {
  const params = useParams({ strict: false });
  const id = mode === "edit" ? (params as { id?: string }).id : undefined;
  const navigate = useNavigate();
  const search = useSearch({ strict: false }) as { tab?: string };
  const tab: EditorTab = isEditorTab(search.tab) ? search.tab : "build";

  const existing = useEditorAutomation(id);
  const automation = existing.data?.automation;
  const builtin = automation?.kind === "builtin";

  // The shipped definition (what SaveVersion / overrides diff against) and
  // the effective one the editor shows (shipped + overrides).
  const shipped = useMemo(
    () => (automation ? parseDefinition(automation.version?.definitionJson) : EMPTY_DEFINITION),
    [automation],
  );
  const effective = useMemo(
    () =>
      automation
        ? applyOverrides(shipped, parseOverrides(automation.blockOverridesJson))
        : EMPTY_DEFINITION,
    [automation, shipped],
  );

  const [name, setName] = useState("");
  const [description, setDescription] = useState("");
  const [draft, setDraft] = useState<AutomationDefinition>(EMPTY_DEFINITION);
  const [errors, setErrors] = useState<BlockErrorRef[]>([]);
  const [loadedVersion, setLoadedVersion] = useState<string | null>(null);

  // (Re)load the draft when the automation (or its version) changes.
  useEffect(() => {
    if (!automation) return;
    const key = `${automation.id}:${automation.currentVersion}:${automation.blockOverridesJson}`;
    if (key === loadedVersion) return;
    setLoadedVersion(key);
    setName(automation.name);
    setDescription(automation.description);
    setDraft(effective);
    setErrors([]);
  }, [automation, effective, loadedVersion]);

  const create = useCreateAutomationV2();
  const saveVersion = useSaveVersionV2();
  const setOverrides = useSetBlockOverrides();
  const updateMeta = useUpdateAutomationMetaV2();
  const setEnabled = useSetAutomationEnabledV2();
  const duplicate = useDuplicateAutomation();
  const saving =
    create.isPending || saveVersion.isPending || setOverrides.isPending || updateMeta.isPending;

  const dirty =
    mode === "create" ||
    (automation !== undefined &&
      (name !== automation.name ||
        description !== automation.description ||
        JSON.stringify(draft) !== JSON.stringify(effective)));

  const setTab = (next: EditorTab) => {
    void navigate({
      to: ".",
      search: (prev: Record<string, unknown>) => ({ ...prev, tab: next }),
      replace: true,
    });
  };

  const fail = (error: unknown) => {
    const message =
      error instanceof ConnectError
        ? error.rawMessage
        : error instanceof Error
          ? error.message
          : String(error);
    const parsed = parseBlockErrors(message);
    setErrors(parsed);
    toast.error(
      parsed.length === 1 && parsed[0]!.blockId === "" ? message : "Fix the highlighted fields",
    );
  };

  const save = async () => {
    setErrors([]);
    if (!name.trim()) {
      setErrors([{ blockId: "", field: "name", message: "Name is required" }]);
      return;
    }
    try {
      if (mode === "create") {
        const created = await create.mutateAsync({
          name: name.trim(),
          description,
          enabled: false,
          definitionJson: JSON.stringify(draft),
          inputsJson: "{}",
        });
        toast.success("Automation created");
        void navigate({ to: "/settings/automations/$id", params: { id: created.automation!.id } });
        return;
      }
      if (!automation) return;
      if (name.trim() !== automation.name || description !== automation.description) {
        await updateMeta.mutateAsync({ id: automation.id, name: name.trim(), description });
      }
      if (builtin) {
        // Only changed tunable fields travel; the structure never does.
        const overrides = diffOverrides(shipped, draft);
        await setOverrides.mutateAsync({
          automationId: automation.id,
          overridesJson: JSON.stringify(overrides),
        });
      } else if (JSON.stringify(draft) !== JSON.stringify(effective)) {
        await saveVersion.mutateAsync({
          automationId: automation.id,
          definitionJson: JSON.stringify(draft),
        });
      }
      toast.success(builtin ? "Overrides saved" : "Saved as a new version");
    } catch (error) {
      fail(error);
    }
  };

  const onDuplicate = async () => {
    if (!automation) return;
    try {
      const copy = await duplicate.mutateAsync({ automationId: automation.id });
      toast.success("Duplicated — the copy is fully editable");
      void navigate({ to: "/settings/automations/$id", params: { id: copy.automation!.id } });
    } catch (error) {
      fail(error);
    }
  };

  if (mode === "edit" && existing.isPending) {
    return <p className="text-muted-foreground text-sm">Loading…</p>;
  }
  if (mode === "edit" && (existing.error || !automation)) {
    return (
      <div className="space-y-3">
        <p className="text-destructive text-sm">Automation not found.</p>
        <Link to="/settings/automations" className="text-sm underline">
          Back to automations
        </Link>
      </div>
    );
  }

  const nameError = errors.find((e) => e.blockId === "" && e.field === "name")?.message;
  const triggerSummary = existingSummary(automation?.id, draft);

  return (
    <div className="space-y-6" data-testid="automation-editor">
      <PageHeading
        title={mode === "create" ? "New automation" : name || "Automation"}
        actions={
          <div className="flex items-center gap-2">
            {automation && (
              <label className="flex items-center gap-2 text-sm">
                <Switch
                  checked={automation.enabled}
                  disabled={setEnabled.isPending}
                  onCheckedChange={(enabled) =>
                    setEnabled.mutate({ id: automation.id, enabled }, { onError: fail })
                  }
                  aria-label="Enabled"
                />
                Enabled
              </label>
            )}
            {builtin && (
              <Button
                type="button"
                variant="outline"
                onClick={onDuplicate}
                disabled={duplicate.isPending}
              >
                <Copy className="size-4" aria-hidden /> Duplicate
              </Button>
            )}
            <Button
              type="button"
              onClick={save}
              disabled={saving || !dirty}
              data-testid="save-button"
            >
              {mode === "create" ? "Create" : builtin ? "Save overrides" : "Save version"}
            </Button>
          </div>
        }
      />

      {builtin && (
        <div
          className="bg-muted flex items-start gap-2 rounded-lg border p-3 text-sm"
          data-testid="builtin-banner"
        >
          <Lock className="mt-0.5 size-4 shrink-0" aria-hidden />
          <div>
            <span className="font-medium">Built-in automation</span>
            <Badge variant="secondary" className="ml-2">
              {automation?.builtinKey}
            </Badge>
            <p className="text-muted-foreground mt-0.5">
              Its blocks and wiring are fixed. Properties marked as editable, and the Inputs tab,
              are yours to change; Duplicate makes a fully editable copy.
            </p>
          </div>
        </div>
      )}

      <div className="grid gap-4 md:grid-cols-2">
        <Field data-invalid={nameError ? true : undefined}>
          <FieldLabel htmlFor="automation-name">Name</FieldLabel>
          <Input
            id="automation-name"
            value={name}
            onChange={(e) => setName(e.target.value)}
            disabled={builtin}
          />
          {nameError && <FieldError>{nameError}</FieldError>}
        </Field>
        <Field>
          <FieldLabel htmlFor="automation-description">Description</FieldLabel>
          <Input
            id="automation-description"
            value={description}
            onChange={(e) => setDescription(e.target.value)}
            disabled={builtin}
          />
        </Field>
      </div>

      <Tabs value={tab} onValueChange={(v) => setTab(v as EditorTab)}>
        <TabsList>
          <TabsTrigger value="build">Build</TabsTrigger>
          <TabsTrigger value="inputs" disabled={mode === "create"}>
            Inputs
          </TabsTrigger>
          <TabsTrigger value="runs" disabled={mode === "create"}>
            Runs
          </TabsTrigger>
          <TabsTrigger value="settings" disabled={mode === "create"}>
            Settings
          </TabsTrigger>
        </TabsList>
        <TabsContent value="build" className="pt-4">
          <BuildTab
            definition={draft}
            onChange={setDraft}
            builtin={builtin}
            errors={errors}
            triggerSummary={triggerSummary}
            testPanel={testPanel}
            variableValues={variableValues}
          />
        </TabsContent>
        <TabsContent value="inputs" className="pt-4">
          {inputsTab ?? <Placeholder item="3.6 (Inputs)" />}
        </TabsContent>
        <TabsContent value="runs" className="pt-4">
          {runsTab ?? <Placeholder item="3.7 (Runs)" />}
        </TabsContent>
        <TabsContent value="settings" className="pt-4">
          {settingsTab ?? <Placeholder item="3.8 (Settings)" />}
        </TabsContent>
      </Tabs>
    </div>
  );
}

/** A short trigger readout for the list row; the server renders the
 * authoritative `trigger_summary` on ListAutomations, this mirrors it for
 * unsaved drafts. */
function existingSummary(_id: string | undefined, definition: AutomationDefinition): string {
  const t = definition.trigger;
  switch (t.kind) {
    case "integration": {
      const keys = Array.isArray(t["eventKeys"]) ? (t["eventKeys"] as string[]) : [];
      return `${String(t["provider"] ?? "integration")} · ${keys.length ? keys.join(", ") : "no events"}`;
    }
    case "cron":
      return `Schedule · ${String(t["schedule"] ?? "")} ${String(t["timezone"] ?? "")}`.trim();
    case "webhook":
      return `Webhook · ${String(t["registrationId"] ?? "unset")}`;
    case "manual":
      return "Manual";
  }
}
