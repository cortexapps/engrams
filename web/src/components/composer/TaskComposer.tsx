import { useEffect, useMemo, useRef, useState, type KeyboardEvent } from "react";
import { Link, useNavigate } from "@tanstack/react-router";
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
import { toast } from "sonner";

import { useAuth } from "@/auth/AuthProvider";
import { ModeChip } from "@/components/ModeChip";
import { ProviderTile } from "@/components/integrations/ProviderTile";
import { catalogToViews } from "@/components/integrations/useConnectorViews";
import { ProfileIcon } from "@/components/profiles/ProfileIcon";
import {
  InlineUploadComposer,
  type InlineUploadComposerHandle,
} from "@/components/session-files/InlineUploadComposer";
import { UploadButton } from "@/components/session-files/UploadButton";
import type { UploadToken } from "@/components/session-files/useSessionUploads";
import { Button } from "@/components/ui/button";
import { Collapsible, CollapsibleContent, CollapsibleTrigger } from "@/components/ui/collapsible";
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
import { Skeleton } from "@/components/ui/skeleton";
import { Text } from "@/components/ui/text";
import { Tooltip, TooltipContent, TooltipProvider, TooltipTrigger } from "@/components/ui/tooltip";
import { useCredentials } from "@/hooks/useCredentials";
import { useEnabledImages } from "@/hooks/useEnabledImages";
import { isSubmitKey, useEnterToSend } from "@/hooks/useEnterToSend";
import { useHarnessCatalog } from "@/hooks/useHarnessCatalog";
import { useHarnessEnv } from "@/hooks/useHarnessEnv";
import { useIntegrationCatalog } from "@/hooks/useIntegrations";
import { useModelRouters } from "@/hooks/useModelRouters";
import { useProfiles } from "@/hooks/useProfiles";
import { MOD_LABEL } from "@/keyboard/platform";
import { useKeyboardUi } from "@/keyboard/store";
import { humanizeAction } from "@/lib/connectorModel";
import { defaultCapabilitiesForGrants } from "@/lib/profileIntegrations";
import { derivePolicy, type DerivedPolicy } from "@/lib/profilePolicy";
import { cn } from "@/lib/utils";
import {
  EMPTY_OVERRIDE,
  SessionHarnessControls,
  type HarnessOverride,
} from "@/pages/sessions/SessionHarnessControls";

const LAST_PROFILE_KEY = "engrams:lastProfileId";

function readLastProfileId(): string | null {
  try {
    return localStorage.getItem(LAST_PROFILE_KEY);
  } catch {
    return null;
  }
}

export function rememberLastProfileId(id: string): void {
  try {
    localStorage.setItem(LAST_PROFILE_KEY, id);
  } catch {
    /* Private mode or disabled storage means the preselection does not persist. */
  }
}

export interface TaskComposerState {
  prompt: string;
  profileId: string | null;
  harnessOverride: HarnessOverride;
  valid: boolean;
}

export interface TaskComposerUploads {
  tokens: UploadToken[];
  busy: boolean;
  addFiles: (files: FileList | readonly File[]) => UploadToken[];
  addCanonicalPath: (path: string) => boolean;
  remove: (id: string) => void;
  retry: (id: string) => Promise<void>;
}

export interface TaskComposerProps {
  submitLabel: string;
  pendingLabel: string;
  pending: boolean;
  onSubmit: (state: TaskComposerState) => void | Promise<void>;
  onStateChange?: (state: TaskComposerState) => void;
  recentProfileId?: string;
  disabled?: boolean;
  submitTestId?: string;
  ariaLabel?: string;
  placeholder?: string;
  uploads?: TaskComposerUploads;
}

/**
 * The product task composer. It owns selection and input state, but it does not
 * create a task or call an HTTP endpoint. Its owner supplies the submit path.
 */
export function TaskComposer({
  submitLabel,
  pendingLabel,
  pending,
  onSubmit,
  onStateChange,
  recentProfileId,
  disabled = false,
  submitTestId,
  ariaLabel = "Task",
  placeholder = "Fix the flaky billing-gateway integration test and open a PR.",
  uploads,
}: TaskComposerProps) {
  const navigate = useNavigate();
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
  const { data: modelRouterData } = useModelRouters();
  const { data: harnessEnvVars } = useHarnessEnv(true);
  const { data: credentials } = useCredentials(true);

  const profiles = profilesData?.profiles ?? [];
  const views = useMemo(() => catalogToViews(catalog?.providers ?? []), [catalog]);
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [prompt, setPrompt] = useState("");
  const [switcherOpen, setSwitcherOpen] = useState(false);
  const [harnessOverride, setHarnessOverride] = useState<HarnessOverride>(EMPTY_OVERRIDE);

  useEffect(() => {
    setHarnessOverride(EMPTY_OVERRIDE);
  }, [selectedId]);

  useEffect(() => {
    if (selectedId || profiles.length === 0) return;
    const stored = readLastProfileId();
    const pick =
      (stored && profiles.some((profile) => profile.id === stored) && stored) ||
      (recentProfileId &&
        profiles.some((profile) => profile.id === recentProfileId) &&
        recentProfileId) ||
      profiles[0]!.id;
    setSelectedId(pick);
  }, [profiles, recentProfileId, selectedId]);

  const selected = profiles.find((profile) => profile.id === selectedId) ?? null;
  const effectiveHarnessName =
    harnessOverride.harness ??
    selected?.harness ??
    (harnesses?.length === 1 ? harnesses[0]?.name : undefined);
  const effectiveHarness = harnesses?.find((harness) => harness.name === effectiveHarnessName);
  const effectiveRouterId =
    harnessOverride.modelRouter === null
      ? selected?.modelRouter
      : harnessOverride.modelRouter || undefined;
  const effectiveRouter = modelRouterData?.routers.find(
    (router) => router.id === effectiveRouterId,
  );
  const planModes = (effectiveHarness?.descriptor?.modes ?? [])
    .filter((mode) => mode.id !== "default")
    .map((mode) => ({ id: mode.id, label: mode.label || mode.id }));

  const policy: DerivedPolicy | null = selected
    ? derivePolicy(
        {
          capabilities: defaultCapabilitiesForGrants(selected.integrationGrants ?? [], views),
          network: {
            default: selected.network?.default === "allow" ? "allow" : "deny",
            allowHosts: selected.network?.allowHosts ?? [],
            allowHostPatterns: selected.network?.allowHostPatterns ?? [],
          },
          secrets: [],
        },
        views,
        {
          allowHosts: [
            ...(effectiveHarness?.descriptor?.egress?.allowHosts ?? []),
            ...(effectiveRouter?.egressHosts ??
              effectiveHarness?.descriptor?.nativeEgress?.allowHosts ??
              []),
          ],
          allowHostPatterns: [
            ...(effectiveHarness?.descriptor?.egress?.allowHostPatterns ?? []),
            ...(effectiveRouter
              ? []
              : (effectiveHarness?.descriptor?.nativeEgress?.allowHostPatterns ?? [])),
          ],
        },
      )
    : null;
  const imageName = selected
    ? images?.find((image) => image.id === selected.imageId)?.image_uri
    : undefined;

  const effectiveUserEnv = effectiveHarness?.descriptor?.auth?.userEnv;
  const effectiveOAuth = effectiveHarness?.descriptor?.auth?.userOauth?.provider;
  const userEnvHint = effectiveHarness?.descriptor?.auth?.userEnvHint;
  const userEnvMissing =
    !effectiveRouter &&
    !!effectiveUserEnv &&
    !!harnessEnvVars?.some((value) => value.envVar === effectiveUserEnv && !value.present);
  const oauthMissing =
    !effectiveRouter &&
    !!effectiveOAuth &&
    credentials !== undefined &&
    !credentials.some(
      (credential) =>
        credential.kind === "oauth" &&
        credential.provider === effectiveOAuth &&
        credential.connected,
    );
  const userScopedProviders = useMemo(() => {
    if (!selected) return new Set<string>();
    const byConnection = new Map(
      views
        .filter((view) => view.defaultConnectionId !== "")
        .map((view) => [view.defaultConnectionId, view.provider]),
    );
    return new Set(
      (selected.integrationGrants ?? [])
        .filter((grant) => grant.credentialScope === "user")
        .map((grant) => byConnection.get(grant.connectionId))
        .filter((provider): provider is string => provider !== undefined),
    );
  }, [selected, views]);
  const connectorsMissing = useMemo(() => {
    if (!selected || credentials === undefined) return [];
    const byConnection = new Map(
      views
        .filter((view) => view.defaultConnectionId !== "")
        .map((view) => [view.defaultConnectionId, view]),
    );
    const required = (selected.integrationGrants ?? [])
      .filter((grant) => grant.credentialScope === "user")
      .map((grant) => byConnection.get(grant.connectionId))
      .filter((view) => view?.userCredential !== undefined);
    const unique = [...new Map(required.map((view) => [view!.provider, view!])).values()];
    return unique.filter(
      (view) =>
        !credentials.some(
          (credential) =>
            credential.kind === "connector" &&
            credential.provider === view.provider &&
            credential.connected &&
            credential.status === "connected",
        ),
    );
  }, [selected, views, credentials]);
  const credentialMissing = userEnvMissing || oauthMissing;
  const showCredentialBox = credentialMissing || connectorsMissing.length > 0;
  const hasContent = prompt.trim().length > 0 || (uploads?.tokens.length ?? 0) > 0;
  const valid =
    !!selected &&
    hasContent &&
    !pending &&
    !disabled &&
    !(uploads?.busy ?? false) &&
    !credentialMissing;
  const state = useMemo<TaskComposerState>(
    () => ({ prompt, profileId: selectedId, harnessOverride, valid }),
    [prompt, selectedId, harnessOverride, valid],
  );

  useEffect(() => {
    onStateChange?.(state);
  }, [onStateChange, state]);

  const submit = () => {
    if (!valid) return;
    void onSubmit(state);
  };

  const composerRef = useRef<InlineUploadComposerHandle>(null);
  const focusNonce = useKeyboardUi((keyboardState) => keyboardState.composerFocusNonce);
  const [enterToSend] = useEnterToSend();
  useEffect(() => {
    composerRef.current?.focus();
  }, [focusNonce]);

  const onComposerKeyDown = (event: KeyboardEvent<HTMLDivElement>) => {
    if (isSubmitKey(event, enterToSend)) {
      event.preventDefault();
      submit();
    }
  };

  const hasProfilesError = !!profilesError;
  useEffect(() => {
    if (!hasProfilesError) return;
    toast.error("Couldn’t load profiles.", {
      action: { label: "Retry", onClick: () => void refetchProfiles() },
    });
  }, [hasProfilesError, refetchProfiles]);

  const noProfiles = !profilesPending && !profilesError && profiles.length === 0;
  const composerUploads: TaskComposerUploads = uploads ?? {
    tokens: [],
    busy: false,
    addFiles: () => [],
    addCanonicalPath: () => false,
    remove: () => {},
    retry: async () => {},
  };

  return (
    <div className="flex flex-col gap-5">
      {showCredentialBox && (
        <div className="flex flex-col gap-1.5 rounded-lg border border-instrument-caution/40 bg-secondary/60 px-3 py-2.5 text-sm">
          <div className="flex flex-wrap items-center justify-between gap-x-4 gap-y-1.5">
            <span className="flex items-center gap-2">
              <KeyRound className="size-4 shrink-0 text-instrument-caution" />
              {userEnvMissing || oauthMissing ? (
                <>
                  {oauthMissing ? (
                    <>OpenAI is not connected</>
                  ) : (
                    <>
                      No <code className="font-mono">{effectiveUserEnv}</code> saved
                    </>
                  )}{" "}
                  — {effectiveHarness?.descriptor?.label || effectiveHarnessName} sessions need it
                  to launch.
                </>
              ) : (
                <>
                  {connectorsMissing.map((view) => view.name).join(", ")}{" "}
                  {connectorsMissing.length === 1 ? "is" : "are"} turned off for this session — this
                  profile runs {connectorsMissing.length === 1 ? "it" : "them"} as you, and you
                  haven&apos;t connected a personal credential.
                </>
              )}
            </span>
            <Link
              to="/settings/credentials"
              className="shrink-0 font-medium underline underline-offset-4"
            >
              Add credential
            </Link>
          </div>
          {(userEnvMissing || oauthMissing) && userEnvHint && (
            <p className="text-muted-foreground">{userEnvHint}</p>
          )}
          {!userEnvMissing && !oauthMissing && connectorsMissing[0]?.userCredential?.tokenHint && (
            <p className="text-muted-foreground">{connectorsMissing[0].userCredential.tokenHint}</p>
          )}
        </div>
      )}

      <div
        className="@container/composer rounded-xl border bg-card shadow-sm transition-[color,box-shadow] focus-within:border-ring focus-within:ring-[3px] focus-within:ring-ring/40"
        onDragOver={(event) => event.preventDefault()}
        onDrop={(event) => {
          event.preventDefault();
          if (uploads && event.dataTransfer.files.length > 0) {
            composerRef.current?.addFiles(event.dataTransfer.files);
          }
        }}
      >
        <InlineUploadComposer
          ref={composerRef}
          value={prompt}
          tokens={composerUploads.tokens}
          onChange={setPrompt}
          onFiles={composerUploads.addFiles}
          onCanonicalPath={composerUploads.addCanonicalPath}
          onRemove={composerUploads.remove}
          onRetry={(id) => void composerUploads.retry(id)}
          onKeyDown={onComposerKeyDown}
          ariaLabel={ariaLabel}
          placeholder={placeholder}
          className="max-h-[calc(10lh+1.125rem)] min-h-[5.25rem] px-4 pt-3.5 pb-1 text-[0.95rem] leading-relaxed md:text-[0.95rem]"
        />
        <div className="flex flex-col gap-2 px-2.5 pt-1 pb-2.5 @md/composer:flex-row @md/composer:items-end">
          <div className="flex flex-wrap items-center gap-2 @md/composer:min-w-0 @md/composer:flex-1">
            {uploads && (
              <UploadButton
                onFiles={(files) => composerRef.current?.addFiles(files)}
                disabled={pending}
              />
            )}
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
            {selected?.includeUserTokens && (
              <TooltipProvider delayDuration={100}>
                <Tooltip>
                  <TooltipTrigger asChild>
                    <button
                      type="button"
                      aria-label="This profile also carries your other saved tokens into the sandbox"
                      className="inline-flex size-7 shrink-0 items-center justify-center rounded-md text-instrument-caution transition-colors hover:bg-accent focus-visible:ring-[3px] focus-visible:ring-ring/50 focus-visible:outline-none"
                    >
                      <Lock className="size-3.5" />
                    </button>
                  </TooltipTrigger>
                  <TooltipContent>
                    This profile also carries your other saved tokens into the sandbox.
                  </TooltipContent>
                </Tooltip>
              </TooltipProvider>
            )}
            <ModeChip
              modes={planModes}
              value={harnessOverride.mode}
              onChange={(mode) => setHarnessOverride({ ...harnessOverride, mode })}
              disabled={pending}
            />
            <div className="@md/composer:ml-auto">
              <SessionHarnessControls
                harnesses={harnesses}
                profileHarness={selected?.harness}
                profileModelRouter={selected?.modelRouter}
                profileModel={selected?.model}
                value={harnessOverride}
                onChange={setHarnessOverride}
                disabled={pending}
              />
            </div>
          </div>
          <Button
            className="shrink-0 self-end"
            onClick={submit}
            disabled={!valid}
            data-testid={submitTestId}
          >
            {pending ? pendingLabel : submitLabel}
            {!pending && (
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

      {policy && (policy.capCount > 0 || policy.reachable.length > 0) && (
        <PolicyReceipt
          policy={policy}
          imageName={imageName}
          userScopedProviders={userScopedProviders}
        />
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
              {profiles.map((profile) => {
                const isChosen = selected?.id === profile.id;
                return (
                  <CommandItem
                    key={profile.id}
                    value={profile.name + " " + profile.description}
                    data-testid={"profile-option-" + profile.id}
                    onSelect={() => onSelect(profile.id)}
                    className="items-start gap-2.5 py-2"
                  >
                    <ProfileIcon
                      name={profile.icon}
                      className="mt-0.5 size-4 shrink-0 text-muted-foreground"
                    />
                    <span className="flex min-w-0 flex-1 flex-col">
                      <span className="font-medium text-foreground">
                        {profile.name}
                        {isChosen && <span className="sr-only"> (selected)</span>}
                      </span>
                      <span className="truncate text-xs text-muted-foreground">
                        {profile.description}
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

function PolicyReceipt({
  policy,
  imageName,
  userScopedProviders,
}: {
  policy: DerivedPolicy;
  imageName: string | undefined;
  userScopedProviders: ReadonlySet<string>;
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
              {policy.providers.map((provider) => (
                <ProviderTile
                  key={provider.view.provider}
                  {...provider.view.icon}
                  name={provider.view.name}
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
          {policy.providers.map((provider) => (
            <div key={provider.view.provider} className="flex flex-col gap-1">
              <div className="flex items-center gap-2">
                <ProviderTile {...provider.view.icon} name={provider.view.name} size={16} />
                <span className="text-[0.78rem] font-semibold">{provider.view.name}</span>
                {userScopedProviders.has(provider.view.provider) && (
                  <span className="rounded-sm bg-secondary px-1.5 py-0.5 text-[0.66rem] text-muted-foreground">
                    uses your credential
                  </span>
                )}
              </div>
              {provider.caps.map((capability) => (
                <div key={capability.action} className="flex items-center gap-2 pl-6">
                  {capability.access === "write" ? (
                    <Pencil className="size-3 text-instrument-caution" />
                  ) : (
                    <Eye className="size-3 text-muted-foreground" />
                  )}
                  <span className="text-[0.76rem] text-muted-foreground">
                    {humanizeAction(capability.action)}
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
                {policy.reachable.map((host) => (
                  <span
                    key={host}
                    className="rounded-full border bg-secondary px-2 py-px font-mono text-[0.68rem]"
                  >
                    {host}
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
