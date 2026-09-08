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

import { EmptyState } from "@/components/empty-state";
import { PageHeading } from "@/components/page-heading";
import { SkeletonRows } from "@/components/skeleton-rows";
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
  removeEntrypoint,
  projectEntrypoint,
  mergeEntrypoint,
  MAIN_ENTRYPOINT_ID,
  entrypointIds,
  blockIdsOutsideEntrypoint,
  addEntrypoint,
  applyOverrides,
  diffOverrides,
  EMPTY_DEFINITION,
  parseBlockErrors,
  parseDefinition,
  parseOverrides,
  type AutomationDefinition,
  type BlockErrorRef,
} from "@/lib/automation-blocks";

import { useAutomationTest } from "@/hooks/useAutomationTest";

import { BuildTab } from "./build/BuildTab";
import { EntrypointBar } from "./build/EntrypointBar";
import { DraftRail } from "./DraftRail";
import { TestPanel } from "./build/test/TestPanel";
import { parseInputsJson, parseInputsSchema } from "@/lib/automation-inputs";
import { InputsTab } from "./inputs/InputsTab";
import { WorkstreamsTab } from "./instances/WorkstreamsTab";
import { DryRunButton } from "./build/DryRunButton";

export const EDITOR_TABS = ["build", "inputs", "workstreams", "runs", "settings"] as const;
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
  /** 3.4 mounts TestPanel by default; these override it (tests, siblings). */
  testPanel?: ReactNode;
  variableValues?: Readonly<Record<string, string>>;
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

  // Builder v2: the drafting agent saves versions while this editor is
  // open. A clean editor adopts them silently (the canvas re-renders — the
  // "assembling live" effect); a dirty one gets a banner instead of a
  // clobber. `staleVersion` holds the not-yet-adopted key.
  const [staleVersion, setStaleVersion] = useState<string | null>(null);
  // What the editor LOADED (not the live row): the user-intent baseline.
  // `dirty` below compares against the live effective definition (it drives
  // Save), which flips the moment the agent lands a version — useless as an
  // "has the user edited?" signal. This baseline only moves on load/adopt.
  const [baseline, setBaseline] = useState<{
    name: string;
    description: string;
    draftJson: string;
  } | null>(null);

  const create = useCreateAutomationV2();
  const saveVersion = useSaveVersionV2();
  const setOverrides = useSetBlockOverrides();
  const updateMeta = useUpdateAutomationMetaV2();
  const setEnabled = useSetAutomationEnabledV2();
  const duplicate = useDuplicateAutomation();
  const saving =
    create.isPending || saveVersion.isPending || setOverrides.isPending || updateMeta.isPending;

  // On a built-in the trigger is pinned (structure locked) and the save path
  // sends only block overrides + meta, so a trigger edit could never persist;
  // compare blocks alone there, so Save never lights for a no-op.
  const comparable = (d: typeof draft) => (builtin ? d.blocks : d);
  const dirty =
    mode === "create" ||
    (automation !== undefined &&
      (name !== automation.name ||
        description !== automation.description ||
        JSON.stringify(comparable(draft)) !== JSON.stringify(comparable(effective))));

  const userDirty =
    baseline !== null &&
    (name !== baseline.name ||
      description !== baseline.description ||
      JSON.stringify(comparable(draft)) !== baseline.draftJson);

  const adoptCurrent = () => {
    if (!automation) return;
    setLoadedVersion(
      `${automation.id}:${automation.currentVersion}:${automation.blockOverridesJson}`,
    );
    setName(automation.name);
    setDescription(automation.description);
    setDraft(effective);
    setBaseline({
      name: automation.name,
      description: automation.description,
      draftJson: JSON.stringify(comparable(effective)),
    });
    setErrors([]);
    setStaleVersion(null);
  };

  // (Re)load the draft when the automation (or its version) changes. A
  // dirty editor is never clobbered: the change parks in `staleVersion`
  // and the banner offers the reload.
  useEffect(() => {
    if (!automation) return;
    const key = `${automation.id}:${automation.currentVersion}:${automation.blockOverridesJson}`;
    if (key === loadedVersion) return;
    if (loadedVersion !== null && userDirty && loadedVersion.startsWith(`${automation.id}:`)) {
      // The arriving row may BE the user's own just-committed save (the
      // refetch after SaveVersion/SetBlockOverrides/UpdateMeta): when it
      // matches what is on screen, adopting is visually a no-op and resets
      // the baseline. Only a version that DIFFERS from the screen banners.
      const matchesScreen =
        automation.name === name &&
        automation.description === description &&
        JSON.stringify(comparable(effective)) === JSON.stringify(comparable(draft));
      if (!matchesScreen) {
        setStaleVersion(key);
        return;
      }
    }
    setLoadedVersion(key);
    setName(automation.name);
    setDescription(automation.description);
    setDraft(effective);
    setBaseline({
      name: automation.name,
      description: automation.description,
      draftJson: JSON.stringify(comparable(effective)),
    });
    setErrors([]);
    setStaleVersion(null);
    // eslint-disable-next-line react-hooks/exhaustive-deps -- userDirty is
    // deliberately read, not depended on: only a KEY change re-evaluates.
  }, [automation, effective, loadedVersion]);

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
        void navigate({
          to: "/automations/$id",
          params: { id: created.automation!.id },
        });
        return;
      }
      if (!automation) return;
      if (name.trim() !== automation.name || description !== automation.description) {
        await updateMeta.mutateAsync({
          id: automation.id,
          name: name.trim(),
          description,
        });
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
      void navigate({
        to: "/automations/$id",
        params: { id: copy.automation!.id },
      });
    } catch (error) {
      fail(error);
    }
  };

  // 3.4: "Test with sample" renders the DRAFT (unsaved edits included) and
  // routes server BlockErrors into the inspector; its latest scope feeds the
  // picker's live previews. Only an existing automation can be rendered
  // server-side (TestRender needs a saved row for samples).
  //
  // HOOK ORDER: this must sit ABOVE the early returns below. While the
  // automation loads, the shell returns early with fewer hooks; a hook placed
  // after that return made the hook count grow once data arrived — React
  // #310 ("rendered more hooks than during the previous render"), which
  // crashed the edit page in production.
  // D9: which entrypoint the Build tab edits. Falls back to main when the
  // selected one disappears (removed, or a different automation loaded).
  const [entrypointId, setEntrypointId] = useState<string>(MAIN_ENTRYPOINT_ID);
  const effectiveEntrypointId = entrypointIds(draft).includes(entrypointId)
    ? entrypointId
    : MAIN_ENTRYPOINT_ID;

  const test = useAutomationTest({
    automationId: automation?.id,
    definition: draft,
    entrypointId: effectiveEntrypointId,
    inputsJson: automation?.inputsJson || "{}",
    onErrors: setErrors,
  });

  if (mode === "edit" && existing.isPending) {
    return <SkeletonRows />;
  }
  if (mode === "edit" && (existing.error || !automation)) {
    return (
      <EmptyState
        tone="error"
        action={
          <Link to="/automations" className="text-sm underline">
            Back to automations
          </Link>
        }
      >
        Automation not found.
      </EmptyState>
    );
  }

  const nameError = errors.find((e) => e.blockId === "" && e.field === "name")?.message;
  const triggerSummary = existingSummary(
    automation?.id,
    projectEntrypoint(draft, effectiveEntrypointId),
  );

  const panel = testPanel ?? (automation ? <TestPanel test={test} /> : null);
  const liveValues = variableValues ?? test.variableValues;

  const draftSessionId = automation?.draftSessionId;
  return (
    <div className={draftSessionId ? "flex items-start gap-6" : undefined}>
      {draftSessionId && <DraftRail sessionId={draftSessionId} />}
      <div className="min-w-0 flex-1 space-y-6" data-testid="automation-editor">
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
              {mode === "edit" &&
                automation && (
                  // A dry run executes the SAVED definition; unsaved edits would
                  // mislead, so it waits for a clean editor.
                  <DryRunButton
                    automationId={automation.id}
                    entrypointId={effectiveEntrypointId}
                    disabled={dirty}
                  />
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

        {staleVersion !== null && (
          <div
            className="border-instrument-caution/50 bg-instrument-caution/10 flex items-center gap-3 rounded-md border px-3 py-2 text-sm"
            data-testid="draft-stale-banner"
          >
            <span className="min-w-0 flex-1">
              The drafting agent saved a new version while you were editing. Reloading discards your
              unsaved edits.
            </span>
            <Button type="button" size="sm" variant="outline" onClick={adoptCurrent}>
              Reload
            </Button>
          </div>
        )}
        <Tabs value={tab} onValueChange={(v) => setTab(v as EditorTab)}>
          <TabsList>
            <TabsTrigger value="build">Build</TabsTrigger>
            <TabsTrigger value="inputs" disabled={mode === "create"}>
              Inputs
            </TabsTrigger>
            {draft.settings.instance !== undefined && (
              <TabsTrigger value="workstreams" disabled={mode === "create"}>
                Workstreams
              </TabsTrigger>
            )}
            <TabsTrigger value="runs" disabled={mode === "create"}>
              Runs
            </TabsTrigger>
            <TabsTrigger value="settings" disabled={mode === "create"}>
              Settings
            </TabsTrigger>
          </TabsList>
          <TabsContent value="build" className="space-y-3 pt-4">
            <EntrypointBar
              definition={draft}
              selected={effectiveEntrypointId}
              onSelect={setEntrypointId}
              onAdd={(epId) => setDraft(addEntrypoint(draft, epId))}
              onRemove={(epId) => setDraft(removeEntrypoint(draft, epId))}
              locked={builtin}
            />
            <BuildTab
              key={effectiveEntrypointId}
              definition={projectEntrypoint(draft, effectiveEntrypointId)}
              onChange={(next) => setDraft(mergeEntrypoint(draft, effectiveEntrypointId, next))}
              builtin={builtin}
              errors={errors}
              triggerSummary={triggerSummary}
              testPanel={panel}
              variableValues={liveValues}
              reservedBlockIds={blockIdsOutsideEntrypoint(draft, effectiveEntrypointId)}
            />
          </TabsContent>
          <TabsContent value="inputs" className="pt-4">
            {inputsTab ?? <InputsTab automationId={id} />}
          </TabsContent>
          {draft.settings.instance !== undefined && id !== undefined && (
            <TabsContent value="workstreams" className="pt-4">
              <WorkstreamsTab
                automationId={id}
                inputsSchema={parseInputsSchema(draft.inputsSchema)}
                defaultInputs={parseInputsJson(automation?.inputsJson)}
              />
            </TabsContent>
          )}
          <TabsContent value="runs" className="pt-4">
            {runsTab}
          </TabsContent>
          <TabsContent value="settings" className="pt-4">
            {settingsTab}
          </TabsContent>
        </Tabs>
      </div>
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
