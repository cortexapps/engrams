/**
 * StartScreen — the /sessions landing. A composer-first "start a task" surface
 * (the one canonical create path, replacing the old New-task dialog and the
 * /launch picker): type what the agent should do, confirm or switch the profile
 * — preselected to the one you last launched — glance at what that profile lets
 * the session reach, and launch. Recent tasks sit beneath for quick re-entry.
 *
 * A profile is the unit of "what this session can do": CreateTask takes only a
 * profile + prompt, so the profile control here switches + inspects; editing a
 * profile's powers lives in Settings (admins). Calls the same CreateTask path
 * the rest of the app does.
 */

import { useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import { useWindowHeight } from "@react-hook/window-size";
import { Link, useNavigate } from "@tanstack/react-router";
import { useQueryClient } from "@tanstack/react-query";
import { useMutation, createConnectQueryKey } from "@connectrpc/connect-query";
import {
  Check,
  ChevronDown,
  ChevronsUpDown,
  CornerDownLeft,
  Eye,
  KeyRound,
  Lock,
  Pencil,
  Settings2,
  ShieldCheck,
} from "lucide-react";

import { createTask, listTasks } from "../../gen/engram/app/v1/task-TaskService_connectquery";
import { useProfiles } from "../../hooks/useProfiles";
import { useIntegrationCatalog } from "../../hooks/useIntegrations";
import { useEnabledImages } from "../../hooks/useEnabledImages";
import { useHarnessCatalog } from "../../hooks/useHarnessCatalog";
import { useHarnessEnv } from "../../hooks/useHarnessEnv";
import { useTasksAsSessionList } from "../../hooks/useTasks";
import { useNow } from "../../hooks/useNow";
import {
  SessionHarnessControls,
  EMPTY_OVERRIDE,
  type HarnessOverride,
} from "./SessionHarnessControls";
import { useAuth } from "../../auth/AuthProvider";
import { useKeyboardUi } from "../../keyboard/store";
import { MOD_LABEL } from "../../keyboard/platform";
import { isSubmitKey, useEnterToSend } from "../../hooks/useEnterToSend";
import { catalogToViews } from "../../components/integrations/useConnectorViews";
import { ProviderTile } from "../../components/integrations/ProviderTile";
import { ProfileIcon } from "../../components/profiles/ProfileIcon";
import { ProfileChip } from "../../components/profiles/ProfileChip";
import { StatusGlyph } from "../../components/Glyph";
import { humanizeAction } from "../../lib/connectorModel";
import { derivePolicy, type DerivedPolicy } from "../../lib/profilePolicy";
import { compareSessions, relativeTime, shortId } from "./session-format";
import type { SessionListItem } from "../../lib/types";
import { Button } from "@/components/ui/button";
import { Textarea } from "@/components/ui/textarea";
import { Text } from "@/components/ui/text";
import { Skeleton } from "@/components/ui/skeleton";
import { Tooltip, TooltipContent, TooltipProvider, TooltipTrigger } from "@/components/ui/tooltip";
import {
  Command,
  CommandEmpty,
  CommandGroup,
  CommandInput,
  CommandItem,
  CommandList,
  CommandSeparator,
} from "@/components/ui/command";
import { Popover, PopoverContent, PopoverTrigger } from "@/components/ui/popover";
import { Collapsible, CollapsibleContent, CollapsibleTrigger } from "@/components/ui/collapsible";
import { toast } from "sonner";
import { cn } from "@/lib/utils";

// The profile you launched last is the sticky default — written on a successful
// launch, read back on the next visit. localStorage (not server state) keeps it
// instant and per-device; the most-recent task's profile is the fallback when
// this browser has no memory yet.
const LAST_PROFILE_KEY = "engrams:lastProfileId";
function readLastProfileId(): string | null {
  try {
    return localStorage.getItem(LAST_PROFILE_KEY);
  } catch {
    return null;
  }
}
function writeLastProfileId(id: string): void {
  try {
    localStorage.setItem(LAST_PROFILE_KEY, id);
  } catch {
    /* private mode / storage disabled — preselection just won't persist */
  }
}

export function StartScreen() {
  const navigate = useNavigate();
  const qc = useQueryClient();
  const { principal } = useAuth();
  const isAdmin = principal.is_admin;

  const {
    data: profilesData,
    isPending: profilesPending,
    error: profilesError,
    refetch: refetchProfiles,
  } = useProfiles(false);
  const { data: catalog } = useIntegrationCatalog();
  const { data: images } = useEnabledImages(true);
  const { data: harnesses } = useHarnessCatalog(true);
  const { data: harnessEnvVars } = useHarnessEnv(true);
  const { data: taskList } = useTasksAsSessionList({ scope: "mine", pageSize: 25 });
  const createTaskMutation = useMutation(createTask, {
    onSuccess: () =>
      qc.invalidateQueries({
        queryKey: createConnectQueryKey({ schema: listTasks, cardinality: undefined }),
      }),
  });

  const profiles = profilesData?.profiles ?? [];
  const views = useMemo(() => catalogToViews(catalog?.providers ?? []), [catalog]);
  const recent = useMemo<SessionListItem[]>(
    () => [...(taskList ?? [])].sort(compareSessions),
    [taskList],
  );

  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [prompt, setPrompt] = useState("");
  const [switcherOpen, setSwitcherOpen] = useState(false);
  const [error, setError] = useState<string | null>(null);
  // ADR 0063 B2: per-session harness/model/effort override (null = inherit the
  // profile default). Reset on profile switch so a stale pick can't carry over.
  const [harnessOverride, setHarnessOverride] = useState<HarnessOverride>(EMPTY_OVERRIDE);
  useEffect(() => {
    setHarnessOverride(EMPTY_OVERRIDE);
  }, [selectedId]);

  // Preselect once profiles resolve: last-launched (this device) → the most
  // recent task's profile → the first profile. Only seeds when nothing's chosen
  // yet, so a refetch never yanks the user's selection out from under them.
  useEffect(() => {
    if (selectedId || profiles.length === 0) return;
    const stored = readLastProfileId();
    const recentProfileId = recent.find((r) => r.profile)?.profile?.id;
    const pick =
      (stored && profiles.some((p) => p.id === stored) && stored) ||
      (recentProfileId && profiles.some((p) => p.id === recentProfileId) && recentProfileId) ||
      profiles[0]!.id;
    setSelectedId(pick);
  }, [profiles, recent, selectedId]);

  const selected = profiles.find((p) => p.id === selectedId) ?? null;
  const policy: DerivedPolicy | null = selected
    ? derivePolicy(
        {
          capabilities: selected.capabilities ?? [],
          network: {
            default: selected.network?.default === "allow" ? "allow" : "deny",
            allowHosts: selected.network?.allowHosts ?? [],
            allowHostPatterns: selected.network?.allowHostPatterns ?? [],
          },
          secrets: [],
        },
        views,
      )
    : null;
  const imageName = selected
    ? images?.find((i) => i.id === selected.imageId)?.image_uri
    : undefined;

  const canLaunch = !!selected && !!prompt.trim() && !createTaskMutation.isPending;

  const launch = async () => {
    if (!selected || !prompt.trim() || createTaskMutation.isPending) return;
    setError(null);
    try {
      const res = await createTaskMutation.mutateAsync({
        type: "chat",
        profileId: selected.id,
        prompt: prompt.trim(),
        // Only the explicitly-overridden fields ride along; the rest fall back to
        // the profile / descriptor default server-side (ADR 0063 B2).
        ...(harnessOverride.harness ? { harness: harnessOverride.harness } : {}),
        ...(harnessOverride.model ? { model: harnessOverride.model } : {}),
        ...(harnessOverride.effort ? { effort: harnessOverride.effort } : {}),
      });
      writeLastProfileId(selected.id);
      const sessionId = res.task?.sessions[0]?.sessionId;
      if (sessionId) navigate({ to: "/sessions/$id", params: { id: sessionId } });
      else setError("Task created but no session id returned.");
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  };

  // Composer focus: on mount, and whenever the keymap (`c` / ⌘K "start task")
  // bumps the nonce after navigating here.
  const composerRef = useRef<HTMLTextAreaElement>(null);
  const focusNonce = useKeyboardUi((s) => s.composerFocusNonce);
  const [enterToSend] = useEnterToSend();
  useEffect(() => {
    composerRef.current?.focus();
  }, [focusNonce]);

  // Vertically center the whole section on the page — computed in JS against the
  // viewport, not flex `justify-center`, so a growing composer extends DOWNWARD
  // from a fixed top instead of dragging the section up the page. Recomputes on
  // mount, on viewport-height change (useWindowHeight), and when the data-driven
  // resting height changes (profiles / recent) — but NEVER on a keystroke.
  // offsetHeight excludes the margin we set, so the measurement isn't circular;
  // clamp at 0 so a section taller than the viewport just top-anchors and scrolls.
  // wait: 1 — all but eliminate the hook's 100ms debounce so the section
  // re-centers in lockstep with a drag-resize instead of lagging behind it.
  const windowHeight = useWindowHeight({ wait: 1 });
  const scrollRef = useRef<HTMLDivElement>(null);
  const sectionRef = useRef<HTMLDivElement>(null);
  const [topPad, setTopPad] = useState(0);
  useLayoutEffect(() => {
    const scroll = scrollRef.current;
    const section = sectionRef.current;
    if (!scroll || !section) return;
    setTopPad(Math.max(0, (scroll.clientHeight - section.offsetHeight) / 2));
  }, [windowHeight, profilesPending, profiles.length, recent.length]);

  const onComposerKeyDown = (e: React.KeyboardEvent<HTMLTextAreaElement>) => {
    if (isSubmitKey(e, enterToSend)) {
      e.preventDefault();
      void launch();
    }
  };

  // A failed ListProfiles (no cached data) surfaces as a toast with a retry, not
  // a silent fall-through to the empty state — which would wrongly tell the user
  // to create a profile while the only launch path is blocked. Keyed on the
  // boolean so a background refetch failing again doesn't stack toasts.
  const hasProfilesError = !!profilesError;
  useEffect(() => {
    if (!hasProfilesError) return;
    toast.error("Couldn’t load profiles.", {
      action: { label: "Retry", onClick: () => void refetchProfiles() },
    });
  }, [hasProfilesError, refetchProfiles]);

  // ADR 0063 B3: nudge to set the selected harness's user credential when the
  // profile will inject it (includeUserTokens) but the user hasn't saved it.
  const effectiveHarnessName =
    harnessOverride.harness ??
    selected?.harness ??
    (harnesses?.length === 1 ? harnesses[0]?.name : undefined);
  const effectiveHarness = harnesses?.find((h) => h.name === effectiveHarnessName);
  const effectiveUserEnv = effectiveHarness?.descriptor?.auth?.userEnv;
  const userEnvMissing =
    !!effectiveUserEnv &&
    (harnessEnvVars?.some((v) => v.envVar === effectiveUserEnv && !v.present) ?? false);
  const showTokenNudge = !isAdmin && !!selected?.includeUserTokens && userEnvMissing;
  const noProfiles = !profilesPending && !profilesError && profiles.length === 0;

  return (
    <div ref={scrollRef} className="flex-1 overflow-auto">
      <div className="mx-auto flex min-h-full w-full max-w-4xl flex-col px-4 pb-12">
        {/* topPad vertically centers the section at the initial paint / on resize
            (see the useWindowHeight effect). It's a fixed margin, not flex
            centering, so a growing composer extends DOWNWARD from here instead of
            re-centering and dragging the heading up the page. */}
        <div ref={sectionRef} style={{ marginTop: topPad }} className="flex flex-col gap-5">
          <Text as="h1" variant="display" className="text-balance">
            Start a task
          </Text>

          {showTokenNudge && (
            <div className="flex flex-wrap items-center justify-between gap-x-4 gap-y-1.5 rounded-lg border bg-secondary/60 px-3 py-2 text-sm">
              <span className="flex items-center gap-2">
                <KeyRound className="size-4 shrink-0 text-instrument-caution" />
                No <code className="font-mono">{effectiveUserEnv}</code> saved —{" "}
                {effectiveHarness?.descriptor?.label || effectiveHarnessName} sessions need it.
              </span>
              <Link
                to="/settings/tokens"
                className="shrink-0 font-medium underline underline-offset-4"
              >
                Add credential
              </Link>
            </div>
          )}

          {/* The composer is the one focal object: a single raised surface whose
              focus ring belongs to the whole card, so the borderless textarea and
              the control row read as one input. */}
          <div className="rounded-xl border bg-card shadow-sm transition-[color,box-shadow] focus-within:border-ring focus-within:ring-[3px] focus-within:ring-ring/40">
            <Textarea
              ref={composerRef}
              value={prompt}
              onChange={(e) => setPrompt(e.target.value)}
              onKeyDown={onComposerKeyDown}
              rows={3}
              aria-label="Task"
              placeholder="Fix the flaky billing-gateway integration test and open a PR."
              className="max-h-[calc(10lh+1.125rem)] min-h-[5.25rem] resize-none overflow-y-auto border-0 bg-transparent px-4 pt-3.5 pb-1 text-[0.95rem] leading-relaxed shadow-none focus-visible:ring-0 md:text-[0.95rem] dark:bg-transparent"
            />
            <div className="flex items-center gap-2 px-2.5 pt-1 pb-2.5">
              <ProfileSwitcher
                profiles={profiles}
                selected={selected}
                pending={profilesPending}
                open={switcherOpen}
                onOpenChange={setSwitcherOpen}
                onSelect={(id) => {
                  setSelectedId(id);
                  setSwitcherOpen(false);
                }}
                isAdmin={isAdmin}
                onManage={() => {
                  setSwitcherOpen(false);
                  navigate({ to: "/settings/profiles" });
                }}
              />
              {/* The credential heads-up, kept terse: a lock beside the profile
                  whose meaning lives in the tooltip, not a sentence in the flow. */}
              {selected?.includeUserTokens && (
                <TooltipProvider delayDuration={100}>
                  <Tooltip>
                    <TooltipTrigger asChild>
                      <button
                        type="button"
                        aria-label="This profile carries your user tokens into the sandbox"
                        className="inline-flex size-7 shrink-0 items-center justify-center rounded-md text-instrument-caution transition-colors hover:bg-accent focus-visible:ring-[3px] focus-visible:ring-ring/50 focus-visible:outline-none"
                      >
                        <Lock className="size-3.5" />
                      </button>
                    </TooltipTrigger>
                    <TooltipContent>
                      This profile carries your user tokens into the sandbox.
                    </TooltipContent>
                  </Tooltip>
                </TooltipProvider>
              )}
              <SessionHarnessControls
                harnesses={harnesses}
                profileHarness={selected?.harness}
                value={harnessOverride}
                onChange={setHarnessOverride}
                disabled={createTaskMutation.isPending}
              />
              <Button
                className="ml-auto"
                onClick={() => void launch()}
                disabled={!canLaunch}
                data-testid="launch-task"
              >
                {createTaskMutation.isPending ? "Launching…" : "Launch"}
                {!createTaskMutation.isPending && (
                  // The shortcut lives on the button it triggers, toned to sit on
                  // the lime fill (the button's own ink, dialled back) rather than
                  // floating beside it as separate chrome.
                  <kbd
                    aria-hidden
                    className="ml-0.5 hidden items-center gap-0.5 rounded border border-primary-foreground/25 px-1 font-sans text-[0.65rem] font-medium tracking-normal text-primary-foreground/80 normal-case sm:inline-flex"
                  >
                    {!enterToSend && MOD_LABEL}
                    <CornerDownLeft className="size-3" />
                  </kbd>
                )}
              </Button>
            </div>
          </div>

          {/* The receipt earns its place when the profile grants outside reach —
              connector powers OR raw network egress. A profile with neither has
              nothing to disclose (every session is sandboxed by default). */}
          {policy && (policy.capCount > 0 || policy.reachable.length > 0) && (
            <PolicyReceipt policy={policy} imageName={imageName} />
          )}

          {noProfiles &&
            (isAdmin ? (
              <p className="text-sm text-muted-foreground">
                No profiles yet.{" "}
                <Link to="/settings/profiles/new" className="underline underline-offset-4">
                  Create your first profile →
                </Link>
              </p>
            ) : (
              <p className="text-sm text-muted-foreground">
                No profiles configured — ask an admin to set one up.
              </p>
            ))}

          {error && (
            <p role="alert" className="text-sm text-destructive">
              {error}
            </p>
          )}

          {/* Recent rides with the composer in the centered cluster — re-entry
              right under the box, not stranded at the foot of the page. */}
          <RecentTasks rows={recent} />
        </div>
      </div>
    </div>
  );
}

type ProfileLike = NonNullable<ReturnType<typeof useProfiles>["data"]>["profiles"][number];

function ProfileSwitcher({
  profiles,
  selected,
  pending,
  open,
  onOpenChange,
  onSelect,
  isAdmin,
  onManage,
}: {
  profiles: ProfileLike[];
  selected: ProfileLike | null;
  pending: boolean;
  open: boolean;
  onOpenChange: (open: boolean) => void;
  onSelect: (id: string) => void;
  isAdmin: boolean;
  onManage: () => void;
}) {
  return (
    <Popover open={open} onOpenChange={onOpenChange}>
      <PopoverTrigger asChild>
        <button
          type="button"
          disabled={pending || profiles.length === 0}
          aria-label="Choose a profile"
          data-testid="profile-switcher"
          className="inline-flex max-w-[16rem] items-center gap-1.5 rounded-md border bg-background px-2.5 py-1.5 text-sm transition-colors hover:bg-accent focus-visible:ring-[3px] focus-visible:ring-ring/50 focus-visible:outline-none disabled:opacity-50"
        >
          {pending ? (
            <Skeleton className="h-4 w-24" />
          ) : selected ? (
            <>
              <ProfileIcon name={selected.icon} className="size-4 shrink-0 text-muted-foreground" />
              <span className="truncate font-medium">{selected.name}</span>
            </>
          ) : (
            <span className="text-muted-foreground">Choose a profile</span>
          )}
          <ChevronsUpDown className="size-3.5 shrink-0 text-muted-foreground/70" />
        </button>
      </PopoverTrigger>
      <PopoverContent align="start" className="w-72 p-0">
        <Command loop label="Profiles">
          <CommandInput placeholder="Search profiles…" autoFocus />
          <CommandList className="max-h-64">
            <CommandEmpty>No matches.</CommandEmpty>
            <CommandGroup>
              {profiles.map((p) => {
                const isChosen = selected?.id === p.id;
                return (
                  <CommandItem
                    key={p.id}
                    value={`${p.name} ${p.description}`}
                    data-testid={`profile-option-${p.id}`}
                    onSelect={() => onSelect(p.id)}
                    className="items-start gap-2.5 py-2"
                  >
                    <ProfileIcon
                      name={p.icon}
                      className="mt-0.5 size-4 shrink-0 text-muted-foreground"
                    />
                    <span className="flex min-w-0 flex-1 flex-col">
                      <span className="font-medium text-foreground">
                        {p.name}
                        {isChosen && <span className="sr-only"> (selected)</span>}
                      </span>
                      <span className="truncate text-xs text-muted-foreground">
                        {p.description}
                      </span>
                    </span>
                    {isChosen && <Check aria-hidden className="mt-0.5 size-4 shrink-0" />}
                  </CommandItem>
                );
              })}
            </CommandGroup>
            {isAdmin && (
              <>
                <CommandSeparator />
                <CommandGroup>
                  <CommandItem value="manage edit profiles settings" onSelect={onManage}>
                    <Settings2 className="size-4 text-muted-foreground" />
                    Manage profiles…
                  </CommandItem>
                </CommandGroup>
              </>
            )}
          </CommandList>
        </Command>
      </PopoverContent>
    </Popover>
  );
}

// The "this session will be able to" receipt, demoted to a glance: a one-line
// summary that's always visible (provider marks + power count + reach), with the
// full provider→action breakdown a click away. The summary IS the default, so
// expanding only enhances what's already shown — never gates content on a class.
function PolicyReceipt({
  policy,
  imageName,
}: {
  policy: DerivedPolicy;
  imageName: string | undefined;
}) {
  const [open, setOpen] = useState(false);

  return (
    <Collapsible
      open={open}
      onOpenChange={setOpen}
      className="overflow-hidden rounded-lg border bg-card/60"
    >
      <CollapsibleTrigger className="flex w-full items-center gap-2 px-3 py-2.5 text-left transition-colors hover:bg-accent/50 focus-visible:ring-[3px] focus-visible:ring-ring/40 focus-visible:outline-none">
        <ShieldCheck className="size-3.5 shrink-0 text-instrument-nominal" />
        <span className="flex min-w-0 flex-1 flex-wrap items-center gap-x-2 gap-y-1 text-[0.78rem]">
          <span className="text-muted-foreground">This session can reach</span>
          {policy.providers.length > 0 && (
            <span className="flex items-center gap-1">
              {policy.providers.map((pr) => (
                <ProviderTile
                  key={pr.view.provider}
                  {...pr.view.icon}
                  name={pr.view.name}
                  size={14}
                />
              ))}
            </span>
          )}
          {policy.capCount > 0 && (
            <span className="font-mono text-[0.7rem] text-muted-foreground">
              {policy.capCount} {policy.capCount === 1 ? "power" : "powers"}
            </span>
          )}
          {policy.reachable.length > 0 && (
            <span className="text-[0.7rem] text-muted-foreground">
              {policy.reachable.length} {policy.reachable.length === 1 ? "host" : "hosts"}
            </span>
          )}
        </span>
        <ChevronDown
          aria-hidden
          className={cn(
            "size-4 shrink-0 text-muted-foreground transition-transform duration-200 motion-reduce:transition-none",
            open && "rotate-180",
          )}
        />
      </CollapsibleTrigger>
      <CollapsibleContent>
        <div className="flex flex-col gap-3 border-t px-3 py-3">
          {policy.providers.map((pr) => (
            <div key={pr.view.provider} className="flex flex-col gap-1">
              <div className="flex items-center gap-2">
                <ProviderTile {...pr.view.icon} name={pr.view.name} size={16} />
                <span className="text-[0.78rem] font-semibold">{pr.view.name}</span>
              </div>
              {pr.caps.map((c) => (
                <div key={c.action} className="flex items-center gap-2 pl-6">
                  {c.access === "write" ? (
                    <Pencil className="size-3 text-instrument-caution" />
                  ) : (
                    <Eye className="size-3 text-muted-foreground" />
                  )}
                  <span className="text-[0.76rem] text-muted-foreground">
                    {humanizeAction(c.action)}
                  </span>
                </div>
              ))}
            </div>
          ))}
          {policy.reachable.length > 0 && (
            <div
              className={cn(
                "flex flex-col gap-1.5",
                policy.providers.length > 0 && "border-t pt-2.5",
              )}
            >
              <Text variant="label" tone="muted" className="text-[0.6rem]">
                Reaches
              </Text>
              <div className="flex flex-wrap gap-1.5">
                {policy.reachable.map((h) => (
                  <span
                    key={h}
                    className="rounded-full border bg-secondary px-2 py-px font-mono text-[0.68rem]"
                  >
                    {h}
                  </span>
                ))}
              </div>
            </div>
          )}
          {imageName && (
            <span className="font-mono text-[0.7rem] text-muted-foreground">boots {imageName}</span>
          )}
        </div>
      </CollapsibleContent>
    </Collapsible>
  );
}

// Re-entry, not a data grid: the few most-recent tasks in the same vocabulary as
// the rail (glyph + short id + profile + age), each a link into that session.
// The full table is one click away.
function RecentTasks({ rows }: { rows: SessionListItem[] }) {
  const now = useNow();
  if (rows.length === 0) {
    return (
      <section className="mt-1">
        <Text variant="label" tone="muted">
          Recent
        </Text>
        <p className="mt-2.5 text-sm text-muted-foreground">Tasks you start show up here.</p>
      </section>
    );
  }
  return (
    <section className="mt-1 flex flex-col gap-2">
      <div className="flex items-center justify-between">
        <Text variant="label" tone="muted">
          Recent
        </Text>
        <Link
          to="/sessions/list"
          className="text-xs text-muted-foreground transition-colors hover:text-foreground"
        >
          See all →
        </Link>
      </div>
      <ul className="flex flex-col">
        {rows.slice(0, 5).map((s) => (
          <li key={s.id} data-testid="recent-row">
            <Link
              to="/sessions/$id"
              params={{ id: s.id }}
              title={s.id}
              className="flex items-center gap-3 rounded-md px-2 py-2 outline-none transition-colors hover:bg-accent/60 focus-visible:bg-accent/60 focus-visible:ring-[3px] focus-visible:ring-ring/40"
            >
              <span className="inline-flex w-3 shrink-0 justify-center text-[0.7rem] leading-none">
                <StatusGlyph status={s.status} />
              </span>
              <span className="shrink-0 font-mono text-sm">{shortId(s.id)}</span>
              <ProfileChip
                profile={s.profile}
                fallbackImage={s.image}
                className="min-w-0 text-xs"
              />
              <span className="ml-auto shrink-0 font-mono text-xs tabular-nums text-muted-foreground">
                {relativeTime(s.last_active_at, now)}
              </span>
            </Link>
          </li>
        ))}
      </ul>
    </section>
  );
}
