/** The automation editor shell — the Builder.
 *
 * A fixed-height frame: a masthead (the name as an inline-editable title over
 * the description and the state chips, then the tabs, On/Paused, Dry run,
 * Save) over the tab body. Build fills the remaining height with the canvas;
 * the other tabs scroll. Tabs are driven by the `?tab=` search param.
 *
 * Saving follows the editing model: a user automation saves a full new
 * version (SaveVersion); a built-in saves only the changed tunable fields
 * as overrides (SetBlockOverrides) — its structure is never sent. */

import { ConnectError } from "@connectrpc/connect";
import { Link, useBlocker, useNavigate, useParams, useSearch } from "@tanstack/react-router";
import { Beaker, Copy, Lock, Play, Sparkles } from "lucide-react";
import { useEffect, useMemo, useState, type ReactNode } from "react";
import { toast } from "sonner";

import { EmptyState } from "@/components/empty-state";
import { SkeletonRows } from "@/components/skeleton-rows";
import {
  AlertDialog,
  AlertDialogAction,
  AlertDialogCancel,
  AlertDialogContent,
  AlertDialogDescription,
  AlertDialogFooter,
  AlertDialogHeader,
  AlertDialogTitle,
} from "@/components/ui/alert-dialog";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { useRunNow } from "@/hooks/useAutomations";
import { FieldError } from "@/components/ui/field";
import { Switch } from "@/components/ui/switch";
import { useRun, useRunList } from "@/hooks/useAutomationRuns";
import { replayMap } from "@/lib/run-replay";
import { relativeTime } from "@/lib/relative-time";
import { cn } from "@/lib/utils";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import {
  useCreateAutomationV2,
  useDuplicateAutomation,
  useAutomationVersions,
  useEditorAutomation,
  useSaveVersionV2,
  useSetAutomationEnabledV2,
  useSetBlockOverrides,
  useUpdateAutomationMetaV2,
} from "@/hooks/useAutomationEditor";
import {
  projectEntrypoint,
  MAIN_ENTRYPOINT_ID,
  entrypointIds,
  applyOverrides,
  blockTypesOf,
  hasScopedAgents,
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
import { DraftingPane, useDrafting } from "./build/DraftingPane";
import { parseInputsJson, parseInputsSchema, stableJson } from "@/lib/automation-inputs";
import { InputsTab } from "./inputs/InputsTab";
import { AgentsTab } from "./agents/AgentsTab";
import { DryRunButton } from "./build/DryRunButton";

export const EDITOR_TABS = ["build", "inputs", "agents", "activity", "settings"] as const;
export type EditorTab = (typeof EDITOR_TABS)[number];

export function isEditorTab(value: unknown): value is EditorTab {
  return typeof value === "string" && (EDITOR_TABS as readonly string[]).includes(value);
}

interface Props {
  mode: "create" | "edit";
  /** Slots for the sibling PRs (3.6 / 3.7 / 3.8). */
  inputsTab?: ReactNode;
  activityTab?: ReactNode;
  settingsTab?: ReactNode;
  /** 3.4 mounts TestPanel by default; these override it (tests, siblings). */
  testPanel?: ReactNode;
  variableValues?: Readonly<Record<string, string>>;
}

export function AutomationEditor({
  mode,
  inputsTab,
  activityTab,
  settingsTab,
  testPanel,
  variableValues,
}: Props) {
  const params = useParams({ strict: false });
  const id = mode === "edit" ? (params as { id?: string }).id : undefined;
  const navigate = useNavigate();
  const search = useSearch({ strict: false }) as { tab?: string; run?: string };
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

  // The drafting agent's session, when this automation was drafted: its
  // conversation sits beside the canvas, and the canvas says what the
  // agent is doing to it.
  const drafting = useDrafting(automation?.draftSessionId);
  const [draftingOpen, setDraftingOpen] = useState(true);

  const create = useCreateAutomationV2();
  const saveVersion = useSaveVersionV2();
  const setOverrides = useSetBlockOverrides();
  const updateMeta = useUpdateAutomationMetaV2();
  const setEnabled = useSetAutomationEnabledV2();
  const runNow = useRunNow();
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
        stableJson(comparable(draft)) !== stableJson(comparable(effective))));

  const userDirty =
    baseline !== null &&
    (name !== baseline.name ||
      description !== baseline.description ||
      stableJson(comparable(draft)) !== baseline.draftJson);

  // Unsaved edits survive a tab change (the same page) but not a move to
  // another page: the router asks first, and the browser asks before the tab
  // closes. A stray key that fires a global shortcut can no longer lose them.
  // A draft that Create just saved is not unsaved: the move to its editor
  // must not ask.
  const [created, setCreated] = useState(false);
  const unsaved =
    mode === "create"
      ? !created && (name !== "" || stableJson(draft) !== stableJson(EMPTY_DEFINITION))
      : userDirty;
  const leave = useBlocker({
    shouldBlockFn: ({ current, next }) => unsaved && current.pathname !== next.pathname,
    enableBeforeUnload: () => unsaved,
    withResolver: true,
  });

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
      draftJson: stableJson(comparable(effective)),
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
        stableJson(comparable(effective)) === stableJson(comparable(draft));
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
      draftJson: stableJson(comparable(effective)),
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
        setCreated(true);
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
      toast.success("Saved");
    } catch (error) {
      fail(error);
    }
  };

  const onDuplicate = async () => {
    if (!automation) return;
    try {
      const copy = await duplicate.mutateAsync({ automationId: automation.id });
      toast.success("Duplicated — edit the copy freely");
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
  // The Activity tab's count: what the ledger holds (it lists up to 50).
  const activity = useRunList(automation?.id, { limit: 50 });
  const activityCount = activity.data?.runs.length;
  // ?run= on the Build tab replays that run on the canvas, drawn on the
  // version it executed (the current one needs no second fetch).
  const replayQuery = useRun(tab === "build" && mode === "edit" ? search.run : undefined);
  const replayRun = replayQuery.data?.run;
  const replayVersion = replayRun?.brief?.version;
  const needsOldVersion =
    replayVersion !== undefined &&
    automation !== undefined &&
    replayVersion !== automation.currentVersion;
  const versions = useAutomationVersions(needsOldVersion ? automation?.id : undefined);
  const replay = useMemo(() => {
    if (!replayRun?.brief || !automation) return undefined;
    const ran = replayRun.brief.version;
    const old = needsOldVersion
      ? versions.data?.versions.find((v) => v.number === ran)?.definitionJson
      : undefined;
    const definition =
      old !== undefined
        ? applyOverrides(parseDefinition(old), parseOverrides(automation.blockOverridesJson))
        : effective;
    const types = blockTypesOf(definition);
    const versionNote = needsOldVersion
      ? old !== undefined
        ? `drawn on version ${ran}; the current version is ${automation.currentVersion}`
        : `ran on version ${ran}, drawn on the current version ${automation.currentVersion}`
      : undefined;
    return {
      definition,
      run: replayRun.brief,
      map: replayMap(replayRun.steps, (id) => types.get(id)),
      triggerJson: replayRun.triggerJson,
      ...(versionNote !== undefined ? { versionNote } : {}),
      now: Date.now(),
      onExit: () => {
        void navigate({
          to: ".",
          search: (prev: Record<string, unknown>) => {
            const { run: _run, ...rest } = prev;
            return rest;
          },
          replace: true,
        });
      },
    };
  }, [replayRun, automation, needsOldVersion, versions.data, effective, navigate]);
  // Try it: render the draft, saved or not, against the latest delivery, and
  const runNowClick = async () => {
    if (!automation) return;
    try {
      await runNow.mutateAsync({ automationId: automation.id });
      toast.success("Run started");
      setTab("activity");
    } catch (error) {
      fail(error);
    }
  };
  // open the drawer that shows what each block would do.
  const [testOpen, setTestOpen] = useState(false);
  const tryIt = () => {
    setTestOpen(true);
    if (tab !== "build") setTab("build");
    void test.runOnce().catch((error: unknown) => fail(error));
  };

  if (mode === "edit" && existing.isPending) {
    return (
      <div className="p-6">
        <SkeletonRows />
      </div>
    );
  }
  if (mode === "edit" && (existing.error || !automation)) {
    return (
      <div className="p-6">
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
      </div>
    );
  }

  const nameError = errors.find((e) => e.blockId === "" && e.field === "name")?.message;
  // The Build tab shows the replayed version's ways in: a way in the run
  // took that the draft has since removed still opens, instead of snapping
  // to main.
  const shownEntrypointId = entrypointIds(replay ? replay.definition : draft).includes(entrypointId)
    ? entrypointId
    : MAIN_ENTRYPOINT_ID;
  const triggerSummaryFor = (way: string) =>
    existingSummary(automation?.id, projectEntrypoint(draft, way));
  const liveValues = variableValues ?? test.variableValues;
  const hasAgents = hasScopedAgents(draft);

  return (
    <div className="flex min-h-0 flex-1 flex-col" data-testid="automation-editor">
      <Tabs
        value={tab}
        onValueChange={(v) => setTab(v as EditorTab)}
        className="flex min-h-0 flex-1 flex-col gap-0"
      >
        {/* The masthead: an identity block over the tabs and the verbs, hairline
            below. The name IS the title — Saira, editable in place — and it gets
            the whole left column, because an automation is named in a sentence
            and a title clipped at 360px hid which one you had open. The chips
            say what the automation IS, so they sit under the name instead of
            taking that width from it. */}
        <header className="@container flex min-h-[60px] min-w-0 shrink-0 items-center gap-3 border-b px-6 py-2.5">
          <div className="flex min-w-0 flex-1 flex-col gap-0.5">
            {builtin ? (
              <h1 className="truncate font-display text-lg font-semibold [font-stretch:108%]">
                {name}
              </h1>
            ) : (
              <input
                aria-label="Automation name"
                value={name}
                onChange={(e) => setName(e.target.value)}
                placeholder="Name your automation"
                aria-invalid={nameError ? true : undefined}
                className={cn(
                  "w-full min-w-0 bg-transparent font-display text-lg font-semibold [font-stretch:108%] outline-none placeholder:text-muted-foreground/60",
                  "rounded-sm focus-visible:ring-2 focus-visible:ring-ring/60",
                )}
              />
            )}
            {nameError && <FieldError>{nameError}</FieldError>}
            {/* The description used to live only in Settings, which meant the
                one line that says what an automation DOES was two clicks away
                from the graph that does it. It reads as a sub-header here: the
                same inline edit as the name, at body weight so it stays
                subordinate to it. */}
            {builtin ? (
              description !== "" && (
                <p className="truncate text-sm text-muted-foreground">{description}</p>
              )
            ) : (
              <input
                aria-label="Automation description"
                value={description}
                onChange={(e) => setDescription(e.target.value)}
                placeholder="Add a description"
                className="w-full min-w-0 rounded-sm bg-transparent text-sm text-muted-foreground outline-none placeholder:text-muted-foreground/60 focus-visible:ring-2 focus-visible:ring-ring/60"
              />
            )}
            <div className="flex min-w-0 items-center gap-2">
              {builtin && (
                <Badge
                  variant="secondary"
                  className="shrink-0 gap-1 rounded-sm"
                  data-testid="builtin-banner"
                  title="Its steps and wiring are fixed. Fields marked as editable, and the Inputs tab, are yours to change; Duplicate makes a fully editable copy."
                >
                  <Lock className="size-3" aria-hidden />
                  built-in
                </Badge>
              )}
              {automation && (
                <span className="truncate rounded-full bg-secondary px-2 py-0.5 font-mono text-xs tabular-nums text-muted-foreground">
                  v{automation.currentVersion} · saved {relativeTime(automation.updatedAt)}
                </span>
              )}
            </div>
          </div>
          <TabsList className="shrink-0">
            <TabsTrigger value="build">Build</TabsTrigger>
            <TabsTrigger value="inputs" disabled={mode === "create"}>
              Inputs
            </TabsTrigger>
            {hasAgents && (
              <TabsTrigger value="agents" disabled={mode === "create"}>
                Agents
              </TabsTrigger>
            )}
            <TabsTrigger value="activity" disabled={mode === "create"}>
              Activity
              {activityCount !== undefined && activityCount > 0 && (
                <span className="font-mono text-2xs tabular-nums text-muted-foreground">
                  {activityCount}
                </span>
              )}
            </TabsTrigger>
            <TabsTrigger value="settings" disabled={mode === "create"}>
              Settings
            </TabsTrigger>
          </TabsList>
          <span className="h-6 w-px shrink-0 bg-border" aria-hidden />
          {automation && (
            <label className="flex shrink-0 items-center gap-2 text-sm">
              <Switch
                checked={automation.enabled}
                disabled={setEnabled.isPending}
                onCheckedChange={(enabled) =>
                  setEnabled.mutate({ id: automation.id, enabled }, { onError: fail })
                }
                aria-label={automation.enabled ? "On" : "Paused"}
              />
              {automation.enabled ? "On" : "Paused"}
            </label>
          )}
          {builtin && (
            <Button
              type="button"
              variant="ghost"
              size="sm"
              onClick={onDuplicate}
              disabled={duplicate.isPending}
            >
              <Copy aria-hidden /> Duplicate
            </Button>
          )}
          {drafting && (
            <Button
              type="button"
              variant={draftingOpen ? "secondary" : "outline"}
              size="sm"
              onClick={() => setDraftingOpen((open) => !open)}
              aria-pressed={draftingOpen}
              data-testid="drafting-toggle"
              title={draftingOpen ? "Hide the drafting agent" : "Show the drafting agent"}
            >
              <Sparkles aria-hidden />
              Drafting agent
            </Button>
          )}
          {mode === "edit" && automation && draft.trigger.kind === "manual" && (
            // The manual trigger's one way in. It runs the SAVED definition,
            // so it waits for a clean editor, as Dry run does.
            <Button
              type="button"
              variant="outline"
              size="sm"
              onClick={runNowClick}
              disabled={runNow.isPending || dirty}
              data-testid="run-now-button"
              title={dirty ? "Save first: Run now starts the saved definition" : "Start a run"}
            >
              <Play aria-hidden />
              {runNow.isPending ? "Starting…" : "Run now"}
            </Button>
          )}
          <Button
            type="button"
            variant="outline"
            size="sm"
            onClick={tryIt}
            disabled={test.running}
            data-testid="try-it-button"
            title="Render every block against the latest delivery, with nothing run"
          >
            <Beaker aria-hidden />
            {test.running ? "Trying…" : "Try it"}
          </Button>
          {mode === "edit" && automation && (
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
            size="sm"
            onClick={save}
            disabled={saving || !dirty}
            data-testid="save-button"
          >
            {mode === "create" ? "Create" : "Save"}
          </Button>
        </header>

        <AlertDialog open={leave.status === "blocked"}>
          <AlertDialogContent data-testid="leave-dialog">
            <AlertDialogHeader>
              <AlertDialogTitle>Leave without saving?</AlertDialogTitle>
              <AlertDialogDescription>
                This automation has edits that are not saved. Leaving discards them.
              </AlertDialogDescription>
            </AlertDialogHeader>
            <AlertDialogFooter>
              <AlertDialogCancel onClick={() => leave.status === "blocked" && leave.reset()}>
                Keep editing
              </AlertDialogCancel>
              <AlertDialogAction onClick={() => leave.status === "blocked" && leave.proceed()}>
                Discard and leave
              </AlertDialogAction>
            </AlertDialogFooter>
          </AlertDialogContent>
        </AlertDialog>

        {staleVersion !== null && (
          <div
            className="flex shrink-0 items-center gap-3 border-b bg-secondary px-6 py-1.5 text-xs"
            data-testid="draft-stale-banner"
          >
            <span className="min-w-0 flex-1">
              The drafting agent saved a new version while you were editing. Reloading discards your
              unsaved edits.
            </span>
            <Button
              type="button"
              size="sm"
              variant="outline"
              className="h-7"
              onClick={adoptCurrent}
            >
              Reload
            </Button>
          </div>
        )}

        <TabsContent value="build" className="flex min-h-0 flex-1 flex-col">
          <BuildTab
            definition={replay ? replay.definition : draft}
            entrypointId={shownEntrypointId}
            onSelectEntrypoint={setEntrypointId}
            onChange={setDraft}
            builtin={builtin}
            errors={errors}
            triggerSummaryFor={triggerSummaryFor}
            test={test}
            testPanel={testPanel}
            variableValues={liveValues}
            testOpen={testOpen}
            onTestOpenChange={setTestOpen}
            {...(replay ? { replay } : {})}
            {...(drafting && draftingOpen
              ? {
                  aside: (
                    <DraftingPane drafting={drafting} onClose={() => setDraftingOpen(false)} />
                  ),
                }
              : {})}
            {...(drafting
              ? {
                  drafting: {
                    working: drafting.working,
                    empty:
                      !userDirty &&
                      effective.blocks.length === 0 &&
                      effective.agent === undefined &&
                      effective.trigger.kind === "manual",
                  },
                }
              : {})}
          />
        </TabsContent>
        <TabsContent value="inputs" className="min-h-0 flex-1 overflow-y-auto p-6">
          {inputsTab ?? <InputsTab automationId={id} />}
        </TabsContent>
        {hasAgents && id !== undefined && (
          <TabsContent value="agents" className="min-h-0 flex-1 overflow-y-auto p-6">
            <AgentsTab
              automationId={id}
              inputsSchema={parseInputsSchema(draft.inputsSchema)}
              defaultInputs={parseInputsJson(automation?.inputsJson)}
              {...(draft.agent?.scope.kind === "automation" ? { agentScope: "automation" } : {})}
            />
          </TabsContent>
        )}
        <TabsContent value="activity" className="min-h-0 flex-1 overflow-y-auto p-6">
          {activityTab}
        </TabsContent>
        <TabsContent value="settings" className="min-h-0 flex-1 overflow-y-auto p-6">
          {settingsTab}
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
