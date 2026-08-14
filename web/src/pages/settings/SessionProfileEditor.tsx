/**
 * SessionProfileEditor (redesign) — capability-first. The spine is "what can
 * sessions from this profile DO": enable a connected integration (binds its
 * credential + opens its egress), then pick exactly which powers. A live
 * Session-policy rail shows the receipts. Network is automatic (deny-by-default,
 * derived from the granted powers); skills / env / custom secrets / user-token
 * live under Advanced.
 *
 * Form stack matches the house pattern (SecretsPanel, ImagesPanel): a single
 * react-hook-form `useForm` + zodResolver drives the draft; scalar fields are
 * labeled `Field`s; the collection editors (powers, skills, env, secrets) are
 * controlled via `useFieldValue` (useController — NOT watch/setValue; see its
 * doc comment). Layout is the house shadcn settings shape —
 * each section a `Card`, the policy rail a `Card` that sticks via a wrapper
 * (never `position: sticky` on the rounded/overflow-hidden card itself — that
 * combo clips the corners in Chromium).
 */

import { useEffect, useMemo, useRef, useState } from "react";
import { useNavigate, useParams, Link } from "@tanstack/react-router";
import { Controller, useController, useForm, type Control } from "react-hook-form";
import { zodResolver } from "@hookform/resolvers/zod";
import * as z from "zod";
import { toast } from "sonner";
import {
  CheckIcon,
  ChevronDownIcon,
  ChevronLeftIcon,
  GlobeIcon,
  LockIcon,
  PlusIcon,
  ShieldCheckIcon,
  TagIcon,
} from "lucide-react";

import {
  useProfile,
  useCreateProfile,
  useUpdateProfile,
  useDiscoverProfileRepos,
} from "../../hooks/useProfiles";
import { useEnabledImages } from "../../hooks/useEnabledImages";
import { useHarnessCatalog } from "../../hooks/useHarnessCatalog";
import { useModelRouters, useRouterModels } from "../../hooks/useModelRouters";
import { RouterModelAudience } from "../../gen/engram/app/v1/model_router_pb";
import { SearchableOptionMenu } from "../sessions/SessionHarnessControls";
import { useSkills, useUploadSkill } from "../../hooks/useSkills";
import { useOrgSecretNames } from "../../hooks/useOrgSecrets";
import { defaultCapabilitiesForGrants } from "../../lib/profileIntegrations";
import { useIntegrationConnections } from "../../hooks/useIntegrations";
import {
  capabilityLabel,
  operationsForEndpoints,
  useConnectorViews,
  type ConnectorCapabilityView,
  type ConnectorView,
} from "../../components/integrations/useConnectorViews";
import { IconPicker } from "../../components/profiles/IconPicker";
import { PowerSelector } from "../../components/profiles/PowerSelector";
import { PolicyRail } from "../../components/profiles/PolicyRail";
import { ProviderTile } from "../../components/integrations/ProviderTile";
import type { IntegrationConnection } from "../../gen/engram/app/v1/integration_pb";
import { HostChip } from "../../components/integrations/chips";
import {
  EnvVarsEditor,
  envRowsToMap,
  mapToEnvRows,
  type EnvRow,
} from "../../components/profiles/EnvVarsEditor";
import {
  ProfileSecretsEditor,
  secretRowsToWire,
  wireToSecretRows,
  type SecretRow,
} from "../../components/profiles/ProfileSecretsEditor";
import { derivePolicy } from "../../lib/profilePolicy";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card";
import { Field, FieldDescription, FieldError, FieldGroup, FieldLabel } from "@/components/ui/field";
import { Input } from "@/components/ui/input";
import { Switch } from "@/components/ui/switch";
import { Text } from "@/components/ui/text";
import { Textarea } from "@/components/ui/textarea";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";

const linesOf = (text: string) =>
  text
    .split("\n")
    .map((s) => s.trim())
    .filter(Boolean);

const optionId = (value: string | null | undefined): string | null => value?.trim() || null;

/** One repo row in the draft — the wire shape minus the server-derived parse. */
export interface RepoRow {
  path: string;
  remoteUrl: string;
}

const schema = z.object({
  name: z.string().trim().min(1, "Name the profile first"),
  description: z.string(),
  icon: z.string(),
  imageId: z.string().min(1, "Select an image"),
  // ADR 0062/0063: default harness (catalog name) + model/effort (option ids).
  // null = inherit the deployment / descriptor default.
  harness: z.string().nullable(),
  modelRouter: z.string().nullable(),
  model: z.string().nullable(),
  effort: z.string().nullable(),
  designation: z.boolean(),
  includeUserTokens: z.boolean(),
  skills: z.array(z.string()),
  integrationGrants: z.array(
    z.object({
      connectionId: z.string(),
      operation: z.string(),
      resourceConstraints: z.array(z.string()),
      // ADR 0115: "" | "org" = the shared org credential; "user" = each
      // member's personal credential (human launches then require it).
      credentialScope: z.string().optional(),
    }),
  ),
  repos: z.array(z.custom<RepoRow>()),
  envRows: z.array(z.custom<EnvRow>()),
  networkDefault: z.enum(["deny", "allow"]),
  allowHostsText: z.string(),
  allowPatternsText: z.string(),
  secretRows: z.array(z.custom<SecretRow>()),
  // ADR 0064 P4: guest ports auto-exposed (private) for every session started
  // from this profile.
  portExposures: z.array(z.number()),
});
type ProfileFormValues = z.infer<typeof schema>;

/**
 * Read/write one form field that has no registered input behind it — the row
 * editors (`envRows`, `secretRows`), the multi-selects (`skills`,
 * `capabilities`), and `portExposures`.
 *
 * These MUST go through `useController`, not `watch(name)` + `setValue(name, v)`.
 * Measured on react-hook-form 7.83: with no registered field behind the name,
 * a `setValue` that changes the array's LENGTH lands, and a `setValue` that
 * edits a row IN PLACE is discarded — the form neither stores it nor re-renders.
 * So adding and removing rows worked while picking an org secret in a secret row
 * silently did nothing, and the profile saved with `secrets: []`. `useController`
 * registers the field, so every write lands and notifies.
 */
function useFieldValue<K extends keyof ProfileFormValues>(
  control: Control<ProfileFormValues>,
  name: K,
): [ProfileFormValues[K], (next: ProfileFormValues[K]) => void] {
  const { field } = useController({ control, name });
  return [field.value as ProfileFormValues[K], field.onChange];
}

/**
 * A per-connection ConnectorView so the shared PowerSelector drives Google
 * grants exactly like every other connector's powers. `capabilities` carries
 * the operations the connection's endpoints enable, plus any orphaned grants
 * the caller wants to keep revocable.
 */
function googleConnectionView(
  connection: IntegrationConnection,
  operations: readonly ConnectorCapabilityView[],
): ConnectorView {
  return {
    provider: connection.provider,
    defaultConnectionId: connection.id,
    name: connection.displayName,
    category: "Infrastructure",
    blurb: "",
    icon: { mono: "GC", color: "#4285f4" },
    credentialSource: "mint",
    hosts: connection.googleCloud?.endpoints ?? [],
    capabilities: [...operations],
    status: "connected",
    builtin: true,
    usedBy: 0,
    usedByProfiles: [],
    connectionModel: "named",
  };
}

const EMPTY: ProfileFormValues = {
  name: "",
  description: "",
  icon: "Bot",
  imageId: "",
  harness: null,
  modelRouter: null,
  model: null,
  effort: null,
  designation: false,
  includeUserTokens: false,
  skills: [],
  integrationGrants: [],
  repos: [],
  envRows: [],
  networkDefault: "deny",
  allowHostsText: "",
  allowPatternsText: "",
  secretRows: [],
  portExposures: [],
};

export function SessionProfileEditor({ mode }: { mode: "create" | "edit" }) {
  const navigate = useNavigate();
  const params = useParams({ strict: false }) as { id?: string };
  const editingId = mode === "edit" ? params.id : undefined;
  const { data: existing } = useProfile(editingId);
  const { data: images } = useEnabledImages(true);
  const { data: harnesses } = useHarnessCatalog(true);
  const { data: modelRouterData } = useModelRouters();
  const { data: skillCatalog } = useSkills();
  const { data: orgSecretNames } = useOrgSecretNames();
  const { data: connectionData } = useIntegrationConnections();
  const { views } = useConnectorViews();
  const create = useCreateProfile();
  const update = useUpdateProfile();

  const [netOpen, setNetOpen] = useState(false);
  const [advanced, setAdvanced] = useState(false);
  const [hydratedProfileId, setHydratedProfileId] = useState<string | null>(null);

  const form = useForm<ProfileFormValues>({
    resolver: zodResolver(schema),
    defaultValues: EMPTY,
  });
  const { control, setValue, watch, reset, handleSubmit, formState } = form;

  // Live draft — the policy rail and derived network recompute as these change.
  // The collections below have no registered input, so they ride useController
  // (see useFieldValue): a bare watch/setValue pair drops in-place row edits.
  const [integrationGrants, setGrants] = useFieldValue(control, "integrationGrants");
  const [skills, setSkills] = useFieldValue(control, "skills");
  const [envRows, setEnvRows] = useFieldValue(control, "envRows");
  const [secretRows, setSecretRows] = useFieldValue(control, "secretRows");
  const [repoRows, setRepoRows] = useFieldValue(control, "repos");
  const [portExposures, setPortExposures] = useFieldValue(control, "portExposures");
  // Derived, never stored: capabilities follow from the granted integrations.
  const capabilities = defaultCapabilitiesForGrants(integrationGrants, views);
  const includeUserTokens = watch("includeUserTokens");
  const imageId = watch("imageId");
  // ADR 0062/0063: the selected harness's descriptor drives the model/effort
  // option lists (they're enums on the harness, not free-form).
  const harness = watch("harness");
  const modelRouter = watch("modelRouter");
  const harnessDescriptor = harnesses?.find((h) => h.name === harness)?.descriptor;
  const routerDescriptor = modelRouterData?.routers.find((router) => router.id === modelRouter);
  const { data: routerModelData } = useRouterModels(
    modelRouter ?? "",
    "",
    RouterModelAudience.ADMIN_CATALOG,
  );
  const networkDefault = watch("networkDefault");
  const allowHostsText = watch("allowHostsText");
  const allowPatternsText = watch("allowPatternsText");

  // Hydrate when editing.
  useEffect(() => {
    const p = existing?.profile;
    if (!p) return;
    reset({
      name: p.name,
      description: p.description,
      icon: p.icon,
      imageId: p.imageId,
      // Normalize legacy empty strings to null: a pre-ADR-0063 profile can carry
      // harness/model/effort = "" (inherit), and `?? null` wouldn't catch "" —
      // leaving the form to submit "" (→ `harness "" is not in the catalog`).
      // The default-harness effect below then fills a concrete harness to edit.
      harness: optionId(p.harness),
      modelRouter: optionId(p.modelRouter),
      model: optionId(p.model),
      effort: optionId(p.effort),
      designation: p.designation === "pr_reviewer",
      includeUserTokens: p.includeUserTokens,
      skills: p.skills ?? [],
      integrationGrants: (p.integrationGrants ?? []).map((grant) => ({
        connectionId: grant.connectionId,
        operation: grant.operation,
        resourceConstraints: [...grant.resourceConstraints],
        ...(grant.credentialScope === "user" ? { credentialScope: "user" } : {}),
      })),
      repos: (p.repos ?? []).map((r) => ({ path: r.path, remoteUrl: r.remoteUrl })),
      envRows: mapToEnvRows(p.envVars),
      networkDefault: p.network?.default === "allow" ? "allow" : "deny",
      allowHostsText: (p.network?.allowHosts ?? []).join("\n"),
      allowPatternsText: (p.network?.allowHostPatterns ?? []).join("\n"),
      secretRows: wireToSecretRows(p.secrets ?? []),
      portExposures: p.portExposures ?? [],
    });
    setHydratedProfileId(p.id);
    if ((p.network?.allowHosts ?? []).length || (p.network?.allowHostPatterns ?? []).length)
      setNetOpen(true);
  }, [existing, reset]);

  // Drop any selected skill that no longer exists in the current set — a retired
  // bundle (e.g. the old headless `playwright`, now folded into `browser`) or an
  // uploaded skill later deleted. Otherwise a skill with no toggle rides hidden
  // in the form and fails validation on save (`unknown skill(s): …`). Prune once
  // the catalog has loaded (guarded so a not-yet-loaded catalog can't wipe a
  // valid selection).
  useEffect(() => {
    if (!skillCatalog) return;
    const known = new Set(skillCatalog.map((s) => s.name));
    const current = form.getValues("skills");
    const pruned = current.filter((s) => known.has(s));
    if (pruned.length !== current.length) setSkills(pruned);
  }, [skillCatalog, existing, form, setSkills]);

  // Default to the first enabled image in create mode (don't clobber a choice).
  useEffect(() => {
    if (mode === "create" && !form.getValues("imageId") && images && images.length > 0)
      setValue("imageId", images[0]!.id);
  }, [mode, images, form, setValue]);

  // ADR 0063: a profile always names a CONCRETE harness (no "inherit deployment
  // default"). Default new profiles immediately. For an edit, wait until the
  // existing profile has hydrated before repairing a legacy empty harness; a
  // temporary default while the profile query is loading can leave the three
  // coupled Select controls out of sync with the saved selection.
  useEffect(() => {
    if (!harnesses?.length) return;
    if (mode === "edit" && !existing?.profile) return;
    if (!form.getValues("harness")) setValue("harness", harnesses[0]!.name);
  }, [mode, harnesses, form, setValue, existing]);

  const network = useMemo(
    () => ({
      default: networkDefault,
      allowHosts: linesOf(allowHostsText),
      allowHostPatterns: linesOf(allowPatternsText),
    }),
    [networkDefault, allowHostsText, allowPatternsText],
  );
  const policy = useMemo(
    () =>
      derivePolicy(
        {
          capabilities,
          network,
          secrets: secretRows.map((r) => ({ ref: r.ref, envVar: r.envVar, mode: r.mode })),
        },
        views,
        // ADR 0117: always-needed harness egress plus the selected route's
        // provider egress is merged server-side at create. Show the same receipt.
        {
          allowHosts: [
            ...(harnessDescriptor?.egress?.allowHosts ?? []),
            ...(routerDescriptor?.egressHosts ?? harnessDescriptor?.nativeEgress?.allowHosts ?? []),
          ],
          allowHostPatterns: [
            ...(harnessDescriptor?.egress?.allowHostPatterns ?? []),
            ...(routerDescriptor ? [] : (harnessDescriptor?.nativeEgress?.allowHostPatterns ?? [])),
          ],
        },
      ),
    [capabilities, network, secretRows, views, harnessDescriptor, routerDescriptor],
  );
  const connected = views.filter(
    (view) => view.status === "connected" && view.connectionModel !== "named",
  );
  // Disabled connections stay visible: editing a connection's endpoints
  // auto-disables it, and a hidden grant on a disabled connection blocked
  // every unrelated save of the profile with no way to remove it (web-H1).
  // Every provider the catalog reports as named-connection. The editor renders
  // a block per connection for each of them, so a second provider needs no
  // change here.
  const namedProviders = views.filter((view) => view.connectionModel === "named");
  const namedProviderKeys = new Set(namedProviders.map((view) => view.provider));
  const googleConnections = (connectionData?.connections ?? []).filter((connection) =>
    namedProviderKeys.has(connection.provider),
  );
  const capabilitiesByProvider = new Map(
    namedProviders.map((view) => [view.provider, view.capabilities]),
  );
  const imageUri = images?.find((i) => i.id === imageId)?.image_uri;

  // --- capability helpers (enable→select) -----------------------------------
  const capOn = (connectionId: string, action: string) => {
    return integrationGrants.some(
      (grant) => grant.connectionId === connectionId && grant.operation === action,
    );
  };
  const toggleCap = (connectionId: string, action: string, on: boolean) => {
    const without = integrationGrants.filter(
      (grant) => grant.connectionId !== connectionId || grant.operation !== action,
    );
    // A new grant inherits the connection's current credential scope so the
    // save-side one-scope-per-connection invariant holds.
    const scope = userScopedOn(connectionId) ? { credentialScope: "user" as const } : {};
    setGrants(
      on
        ? [...without, { connectionId, operation: action, resourceConstraints: [], ...scope }]
        : without,
    );
  };
  // ADR 0115: the per-integration "run as the launching user" toggle writes
  // one scope across ALL of the connection's grants.
  const userScopedOn = (connectionId: string) =>
    integrationGrants.some(
      (grant) => grant.connectionId === connectionId && grant.credentialScope === "user",
    );
  const setUserScoped = (connectionId: string, on: boolean) =>
    setGrants(
      integrationGrants.map((grant) =>
        grant.connectionId === connectionId
          ? on
            ? { ...grant, credentialScope: "user" }
            : {
                connectionId: grant.connectionId,
                operation: grant.operation,
                resourceConstraints: grant.resourceConstraints,
              }
          : grant,
      ),
    );
  const enableProvider = (v: ConnectorView) => {
    const reads = v.capabilities.filter((c) => c.access === "read");
    const pick = reads.length ? reads : v.capabilities.slice(0, 1);
    const next = [...integrationGrants];
    for (const cap of pick) {
      if (!capOn(v.defaultConnectionId, cap.action)) {
        next.push({
          connectionId: v.defaultConnectionId,
          operation: cap.action,
          resourceConstraints: [],
        });
      }
    }
    setGrants(next);
  };
  const disableProvider = (v: ConnectorView) =>
    setGrants(integrationGrants.filter((grant) => grant.connectionId !== v.defaultConnectionId));

  const onSubmit = async (vals: ProfileFormValues) => {
    const payload = {
      name: vals.name.trim(),
      description: vals.description,
      icon: vals.icon,
      imageId: vals.imageId,
      harness: optionId(vals.harness) ?? undefined,
      modelRouter: optionId(vals.modelRouter) ?? undefined,
      model: optionId(vals.model) ?? undefined,
      effort: optionId(vals.effort) ?? undefined,
      includeUserTokens: vals.includeUserTokens,
      skills: vals.skills,
      integrationGrants: vals.integrationGrants,
      envVars: envRowsToMap(vals.envRows),
      network: {
        default: vals.networkDefault,
        allowHosts: linesOf(vals.allowHostsText),
        allowHostPatterns: linesOf(vals.allowPatternsText),
      },
      secrets: secretRowsToWire(vals.secretRows),
      repos: vals.repos
        .map((r) => ({ path: r.path.trim(), remoteUrl: r.remoteUrl.trim() }))
        .filter((r) => r.path !== ""),
      portExposures: vals.portExposures,
    };
    const designationValue = vals.designation ? "pr_reviewer" : "";
    try {
      if (mode === "edit" && editingId) {
        // `designation` is admin-only and role-transferring, so send it ONLY when
        // the toggle actually changed from the loaded snapshot. Otherwise a stale
        // editor tab would re-assert an out-of-date designation on an unrelated
        // save and silently steal (or drop) the reviewer role. The field is
        // optional on the wire; omitting it means the server leaves it untouched.
        const hydratedDesignation = existing?.profile?.designation === "pr_reviewer";
        const designationTouched = vals.designation !== hydratedDesignation;
        await update.mutateAsync({
          id: editingId,
          ...payload,
          ...(designationTouched ? { designation: designationValue } : {}),
        });
        toast.success("Saved changes");
      } else {
        await create.mutateAsync({ ...payload, designation: designationValue });
        toast.success(`Profile "${vals.name.trim()}" created`);
      }
      navigate({ to: "/settings/profiles" });
    } catch (e) {
      form.setError("root", { message: e instanceof Error ? e.message : String(e) });
    }
  };

  const busy = create.isPending || update.isPending;

  // Mount the controlled selectors only after reset() has installed the
  // fetched profile. Otherwise they first mount with EMPTY and Radix notifies
  // their change handlers as the saved values arrive, clearing model/effort.
  if (mode === "edit" && hydratedProfileId !== editingId) {
    return <Text tone="muted">Loading profile…</Text>;
  }

  return (
    <form onSubmit={handleSubmit(onSubmit)} className="mx-auto w-full max-w-5xl pb-4">
      <Button asChild variant="ghost" size="sm" className="-ml-2 mb-2 text-muted-foreground">
        <Link to="/settings/profiles">
          <ChevronLeftIcon className="size-4" />
          Profiles
        </Link>
      </Button>

      <div className="grid items-start gap-6 lg:grid-cols-[minmax(0,1fr)_20rem]">
        {/* left: the configuration spine */}
        <div className="flex flex-col gap-6">
          <Section
            icon={<TagIcon className="size-4" />}
            title="Identity"
            sub="Name and badge this profile so it's easy to pick later."
          >
            <FieldGroup className="gap-4">
              <div className="flex flex-col gap-4 sm:flex-row sm:items-start">
                <Controller
                  control={control}
                  name="icon"
                  render={({ field }) => (
                    <Field className="sm:w-44 sm:shrink-0">
                      <FieldLabel htmlFor="profile-icon">Icon</FieldLabel>
                      <IconPicker id="profile-icon" value={field.value} onChange={field.onChange} />
                    </Field>
                  )}
                />
                <Controller
                  control={control}
                  name="name"
                  render={({ field, fieldState }) => (
                    <Field className="flex-1" data-invalid={fieldState.invalid}>
                      <FieldLabel htmlFor="name">Name</FieldLabel>
                      <Input
                        {...field}
                        id="name"
                        aria-label="Profile name"
                        aria-invalid={fieldState.invalid}
                        placeholder="Backend Agent"
                        className="text-base font-semibold"
                      />
                      {fieldState.invalid && <FieldError errors={[fieldState.error]} />}
                    </Field>
                  )}
                />
              </div>
              <Controller
                control={control}
                name="description"
                render={({ field }) => (
                  <Field>
                    <FieldLabel htmlFor="description">Description</FieldLabel>
                    <Input
                      {...field}
                      id="description"
                      aria-label="Description"
                      placeholder="What is this profile for?"
                    />
                    <FieldDescription>
                      Optional — shown wherever this profile is offered.
                    </FieldDescription>
                  </Field>
                )}
              />
              <Controller
                control={control}
                name="designation"
                render={({ field }) => (
                  <div className="flex items-center gap-3 rounded-md border bg-background px-3 py-2.5">
                    <Switch
                      checked={field.value}
                      onCheckedChange={field.onChange}
                      aria-label="PR reviewer profile"
                    />
                    <div className="flex-1">
                      <div className="text-[0.84rem]">Designate as the PR reviewer</div>
                      <div className="text-[0.74rem] text-muted-foreground">
                        Pull-request reviews run on this profile&apos;s image, model, and skills.
                        Only one profile can be the reviewer.
                      </div>
                    </div>
                  </div>
                )}
              />
            </FieldGroup>
          </Section>

          <Section
            icon={<GlobeIcon className="size-4" />}
            title="Launches"
            sub="The image every session from this profile boots."
          >
            <Controller
              control={control}
              name="imageId"
              render={({ field, fieldState }) => (
                <Field data-invalid={fieldState.invalid}>
                  <FieldLabel htmlFor="image-select">Image</FieldLabel>
                  <Select value={field.value} onValueChange={field.onChange}>
                    <SelectTrigger
                      id="image-select"
                      data-testid="image-select"
                      aria-invalid={fieldState.invalid}
                      className="w-full font-mono"
                    >
                      <SelectValue placeholder="Select an image" />
                    </SelectTrigger>
                    <SelectContent>
                      {(images ?? []).map((i) => (
                        <SelectItem key={i.id} value={i.id} className="font-mono">
                          {i.image_uri}
                        </SelectItem>
                      ))}
                    </SelectContent>
                  </Select>
                  {fieldState.invalid && <FieldError errors={[fieldState.error]} />}
                </Field>
              )}
            />

            <Controller
              control={control}
              name="harness"
              render={({ field }) => (
                <Field>
                  <FieldLabel htmlFor="harness-select">Harness</FieldLabel>
                  <Select
                    // Keep Radix controlled before the async profile hydrate.
                    // An undefined→value transition invokes this coupled
                    // control's change path and clears the saved model/effort.
                    value={field.value ?? ""}
                    onValueChange={(v) => {
                      const next = optionId(v);
                      // Radix can notify when reset() hydrates the controlled
                      // value. Only a real user-visible harness change should
                      // invalidate the dependent selections.
                      if (next === form.getValues("harness")) return;
                      field.onChange(next);
                      const router = modelRouterData?.routers.find(
                        (item) => item.id === form.getValues("modelRouter"),
                      );
                      const nextDescriptor = harnesses?.find(
                        (item) => item.name === next,
                      )?.descriptor;
                      const compatible = Boolean(
                        router &&
                        nextDescriptor?.routerProtocols?.some((protocol) =>
                          router.protocols.includes(protocol),
                        ),
                      );
                      if (!compatible) {
                        setValue("modelRouter", null);
                        setValue("model", null);
                        setValue("effort", null);
                      }
                    }}
                  >
                    <SelectTrigger
                      id="harness-select"
                      data-testid="harness-select"
                      className="w-full"
                    >
                      <SelectValue placeholder="Select a harness…" />
                    </SelectTrigger>
                    <SelectContent>
                      {(harnesses ?? []).map((h) => (
                        <SelectItem key={h.name} value={h.name}>
                          {h.descriptor?.label || h.name}
                        </SelectItem>
                      ))}
                    </SelectContent>
                  </Select>
                  <FieldDescription>
                    The agent harness sessions run. Sessions can override it at create — e.g. Claude
                    Code for planning, a smaller model for execution.
                  </FieldDescription>
                </Field>
              )}
            />

            <div className="grid grid-cols-1 gap-3 sm:grid-cols-3">
              <Controller
                control={control}
                name="modelRouter"
                render={({ field }) => (
                  <Field>
                    <FieldLabel htmlFor="model-router-select">Route</FieldLabel>
                    <Select
                      value={field.value ?? "__direct__"}
                      onValueChange={(value) => {
                        field.onChange(value === "__direct__" ? null : value);
                        setValue("model", null);
                      }}
                    >
                      <SelectTrigger id="model-router-select" className="w-full">
                        <SelectValue />
                      </SelectTrigger>
                      <SelectContent>
                        <SelectItem value="__direct__">Direct</SelectItem>
                        {(modelRouterData?.routers ?? [])
                          .filter((router) =>
                            harnessDescriptor?.routerProtocols?.some((protocol) =>
                              router.protocols.includes(protocol),
                            ),
                          )
                          .map((router) => (
                            <SelectItem key={router.id} value={router.id}>
                              {router.label}
                            </SelectItem>
                          ))}
                      </SelectContent>
                    </Select>
                  </Field>
                )}
              />
              <Controller
                control={control}
                name="model"
                render={({ field }) => (
                  <Field>
                    <FieldLabel htmlFor="model-select">Model</FieldLabel>
                    <SearchableOptionMenu
                      current={
                        modelRouter
                          ? (routerModelData?.models.find((model) => model.id === field.value)
                              ?.name ?? "Default model")
                          : (harnessDescriptor?.models.find((model) => model.id === field.value)
                              ?.label ?? "Default model")
                      }
                      inheritLabel="Default model"
                      options={
                        modelRouter
                          ? (routerModelData?.models ?? []).map((model) => ({
                              id: model.id,
                              label: model.name,
                              detail: `${model.id}${!model.available ? " · unavailable" : !model.enabled ? " · automation blocked" : !model.userEnabled ? " · users blocked" : ""}`,
                            }))
                          : (harnessDescriptor?.models ?? []).map((model) => ({
                              id: model.id,
                              label: model.label || model.id,
                            }))
                      }
                      selected={field.value}
                      onSelect={field.onChange}
                      disabled={!harnessDescriptor}
                      testId="model-select"
                    />
                  </Field>
                )}
              />

              <Controller
                control={control}
                name="effort"
                render={({ field }) => (
                  <Field>
                    <FieldLabel htmlFor="effort-select">Effort</FieldLabel>
                    <Select
                      value={field.value ?? "__inherit__"}
                      onValueChange={(v) =>
                        field.onChange(v === "__inherit__" ? null : optionId(v))
                      }
                      disabled={!harnessDescriptor || harnessDescriptor.effort.length === 0}
                    >
                      <SelectTrigger
                        id="effort-select"
                        data-testid="effort-select"
                        className="w-full"
                      >
                        <SelectValue placeholder="Default" />
                      </SelectTrigger>
                      <SelectContent>
                        <SelectItem value="__inherit__">Default</SelectItem>
                        {(harnessDescriptor?.effort ?? []).map((e) => (
                          <SelectItem key={e.id} value={e.id}>
                            {e.label || e.id}
                          </SelectItem>
                        ))}
                      </SelectContent>
                    </Select>
                  </Field>
                )}
              />
            </div>
          </Section>

          <Section
            icon={<ShieldCheckIcon className="size-4" />}
            title="Integrations"
            sub="Enable an integration to bind its credential and open its egress — then choose exactly which powers sessions get."
          >
            {connected.length === 0 && googleConnections.length === 0 ? (
              <EmptyIntegrations />
            ) : (
              <div className="flex flex-col gap-3">
                {connected.map((v) => {
                  const grantedCount = v.capabilities.filter((c) =>
                    capOn(v.defaultConnectionId, c.action),
                  ).length;
                  const on = grantedCount > 0;
                  return (
                    <div
                      key={v.provider}
                      className={`overflow-hidden rounded-md border ${on ? "border-ring/40" : "bg-background"}`}
                    >
                      <div
                        className={`flex items-center gap-3 px-3.5 py-3 ${on ? "bg-primary/[0.06]" : ""}`}
                      >
                        <ProviderTile {...v.icon} name={v.name} size={32} />
                        <div className="min-w-0 flex-1">
                          <div className="text-[0.92rem] font-semibold">{v.name}</div>
                          {on ? (
                            <div className="flex flex-wrap items-center gap-2.5 text-[0.72rem] text-muted-foreground">
                              <span className="inline-flex items-center gap-1">
                                {v.credentialSource === "mint" ? (
                                  <ShieldCheckIcon className="size-3 text-instrument-nominal" />
                                ) : (
                                  <LockIcon className="size-3 text-instrument-nominal" />
                                )}
                                {v.credentialSource === "mint"
                                  ? `${v.name} token minted per session`
                                  : "brokered at proxy"}
                              </span>
                              <span className="inline-flex items-center gap-1">
                                <GlobeIcon className="size-3" />
                                {v.hosts.join(", ")}
                              </span>
                            </div>
                          ) : (
                            <div className="truncate text-[0.78rem] text-muted-foreground">
                              {v.blurb}
                            </div>
                          )}
                        </div>
                        <div className="flex items-center gap-2.5">
                          <Text
                            variant="label"
                            tone={on ? "inherit" : "muted"}
                            className={`text-[0.62rem] ${on ? "text-instrument-nominal" : ""}`}
                          >
                            {on ? "Enabled" : "Off"}
                          </Text>
                          <Switch
                            checked={on}
                            aria-label={`Enable ${v.name}`}
                            onCheckedChange={(c) => (c ? enableProvider(v) : disableProvider(v))}
                          />
                        </div>
                      </div>
                      {on && (
                        <div className="border-t">
                          <PowerSelector
                            view={v}
                            isOn={(action) => capOn(v.defaultConnectionId, action)}
                            onToggle={(action, value) =>
                              toggleCap(v.defaultConnectionId, action, value)
                            }
                          />
                        </div>
                      )}
                      {on && v.userCredential && (
                        <div className="flex items-center justify-between gap-3 border-t px-3 py-2">
                          <div className="min-w-0">
                            <Text variant="label" className="text-[0.72rem]">
                              Use each member&apos;s personal credential
                            </Text>
                            <div className="text-[0.72rem] text-muted-foreground">
                              Sessions act as the person who starts them; a launch is blocked until
                              they connect {v.name} under Settings → Credentials. Automations keep
                              the org credential.
                            </div>
                          </div>
                          <Switch
                            checked={userScopedOn(v.defaultConnectionId)}
                            aria-label={`Use personal ${v.name} credentials`}
                            onCheckedChange={(c) => setUserScoped(v.defaultConnectionId, c)}
                          />
                        </div>
                      )}
                    </div>
                  );
                })}
                {googleConnections.map((connection) => {
                  const googleCapabilities: ConnectorCapabilityView[] =
                    capabilitiesByProvider.get(connection.provider) ?? [];
                  const endpoints = connection.googleCloud?.endpoints ?? [];
                  const offered = operationsForEndpoints(
                    googleCapabilities,
                    endpoints,
                    Boolean(connection.googleCloud?.cloudSqlPostgresInstance),
                  );
                  const offeredActions = new Set(offered.map(({ action }) => action));
                  const grantedActions = new Set(
                    integrationGrants
                      .filter((grant) => grant.connectionId === connection.id)
                      .map((grant) => grant.operation),
                  );
                  // A grant can outlive its endpoint (the connection's APIs
                  // were edited): keep it visible so it can be removed.
                  const orphaned = googleCapabilities.filter(
                    ({ action }) => grantedActions.has(action) && !offeredActions.has(action),
                  );
                  const view = googleConnectionView(connection, [...offered, ...orphaned]);
                  return (
                    <div
                      key={connection.id}
                      className="overflow-hidden rounded-md border bg-background"
                    >
                      <div className="flex items-center gap-3 bg-blue-600/[0.06] px-3.5 py-3">
                        <span className="flex size-8 items-center justify-center rounded-md bg-blue-600 text-xs font-semibold text-white">
                          GC
                        </span>
                        <div className="min-w-0 flex-1">
                          <div className="flex items-center gap-2 text-[0.92rem] font-semibold">
                            {connection.displayName}
                            {!connection.enabled && <Badge variant="secondary">Disabled</Badge>}
                          </div>
                          <div className="truncate font-mono text-[0.7rem] text-muted-foreground">
                            {connection.googleCloud?.serviceAccountEmail}
                          </div>
                        </div>
                        <Text variant="label" tone={grantedActions.size ? "inherit" : "muted"}>
                          {grantedActions.size} granted
                        </Text>
                      </div>
                      {!connection.enabled && (
                        <p className="border-t px-3.5 py-2 text-[0.74rem] text-muted-foreground">
                          This connection is disabled — sessions cannot use these powers. Test and
                          enable it again from{" "}
                          <Link
                            to="/settings/integrations/$provider/$connectionId/setup"
                            params={{ provider: connection.provider, connectionId: connection.id }}
                            className="underline underline-offset-2"
                          >
                            its setup page
                          </Link>
                          , or remove the grants here.
                        </p>
                      )}
                      <div className="border-t">
                        <PowerSelector
                          view={view}
                          isOn={(action) => capOn(connection.id, action)}
                          onToggle={(action, value) => toggleCap(connection.id, action, value)}
                          labelFor={(action) => capabilityLabel(googleCapabilities, action)}
                          noteFor={(action) =>
                            offeredActions.has(action)
                              ? undefined
                              : "not in this connection's allowed APIs"
                          }
                        />
                      </div>
                    </div>
                  );
                })}
                <Button
                  asChild
                  variant="ghost"
                  size="sm"
                  className="self-start text-muted-foreground"
                >
                  <Link to="/settings/integrations">
                    <PlusIcon className="size-3.5" />
                    Connect another integration
                  </Link>
                </Button>
              </div>
            )}
          </Section>

          <Section
            icon={<LockIcon className="size-4" />}
            title="Network"
            sub="Session egress policy. New profiles deny by default; add extra hosts only if a power can't."
          >
            <div className="flex items-center gap-2.5 rounded-md border bg-background px-3.5 py-2.5">
              <LockIcon className="size-4 shrink-0 text-instrument-nominal" />
              <div className="flex-1 text-[0.82rem]">
                <strong>{networkDefault === "allow" ? "Open egress." : "Automatic egress."}</strong>{" "}
                <span className="text-muted-foreground">
                  {networkDefault === "allow"
                    ? "Hosts not matched below remain reachable."
                    : policy.derivedHosts.length === 0
                      ? "No powers granted — sessions are fully sandboxed."
                      : `${policy.derivedHosts.length} host${policy.derivedHosts.length === 1 ? "" : "s"} opened by granted powers.`}
                </span>
              </div>
            </div>
            {(policy.derivedHosts.length > 0 ||
              policy.extraHosts.length > 0 ||
              policy.extraPatterns.length > 0) && (
              <div className="mt-2.5 flex flex-wrap gap-1.5">
                {policy.derivedHosts.map((h) => (
                  <HostChip key={h} host={h} derived />
                ))}
                {[...policy.extraHosts, ...policy.extraPatterns].map((h) => (
                  <HostChip key={h} host={h} />
                ))}
              </div>
            )}
            <button
              type="button"
              onClick={() => setNetOpen((v) => !v)}
              className="mt-3 inline-flex items-center gap-1.5 text-[0.78rem] text-muted-foreground hover:text-foreground"
            >
              <ChevronDownIcon
                className={`size-3.5 transition-transform ${netOpen ? "rotate-180" : ""}`}
              />
              Add extra hosts
            </button>
            {netOpen && (
              <div className="mt-2.5 grid gap-3 sm:grid-cols-2">
                <Controller
                  control={control}
                  name="allowHostsText"
                  render={({ field }) => (
                    <Field>
                      <FieldLabel htmlFor="allow-hosts">Allowed hosts</FieldLabel>
                      <Textarea
                        {...field}
                        id="allow-hosts"
                        rows={3}
                        placeholder={"db.internal\nregistry.npmjs.org"}
                        className="font-mono text-sm"
                      />
                    </Field>
                  )}
                />
                <Controller
                  control={control}
                  name="allowPatternsText"
                  render={({ field }) => (
                    <Field>
                      <FieldLabel htmlFor="allow-patterns">Host patterns</FieldLabel>
                      <Textarea
                        {...field}
                        id="allow-patterns"
                        rows={3}
                        placeholder={"*.githubusercontent.com\n*.pypi.org"}
                        className="font-mono text-sm"
                      />
                    </Field>
                  )}
                />
              </div>
            )}
          </Section>

          <Section
            icon={<TagIcon className="size-4" />}
            title="Repositories"
            sub="Git checkouts this profile's image contains. Routing uses these to match a request to the right codebase — add them by hand, or scan the image."
          >
            <ReposSection repos={repoRows} setRepos={setRepoRows} profileId={editingId} />
          </Section>

          {/* advanced — a disclosure card, de-emphasized beneath the spine */}
          <Card className="gap-0 py-0">
            <button
              type="button"
              onClick={() => setAdvanced((v) => !v)}
              className="flex w-full items-center gap-2 px-6 py-4 text-left"
            >
              <ChevronDownIcon
                className={`size-4 text-muted-foreground transition-transform ${advanced ? "rotate-180" : ""}`}
              />
              <Text variant="label">Advanced</Text>
              <span className="text-[0.76rem] text-muted-foreground">
                skills · environment · ports · custom secrets · user token
              </span>
            </button>
            {advanced && (
              <CardContent className="border-t pt-6 pb-6">
                <Advanced
                  skillCatalog={skillCatalog ?? []}
                  skills={skills}
                  setSkills={setSkills}
                  envRows={envRows}
                  setEnvRows={setEnvRows}
                  secretRows={secretRows}
                  setSecretRows={setSecretRows}
                  orgSecretNames={orgSecretNames ?? []}
                  includeUserTokens={includeUserTokens}
                  setIncludeUserTokens={(b) =>
                    setValue("includeUserTokens", b, { shouldDirty: true })
                  }
                  portExposures={portExposures}
                  setPortExposures={setPortExposures}
                />
              </CardContent>
            )}
          </Card>

          {formState.errors.root && <FieldError errors={[formState.errors.root]} />}
        </div>

        {/* right: live policy receipts. Sticky lives on the wrapper so the
            rounded/overflow-hidden card never clips its own corners. */}
        <div className="lg:sticky lg:top-6">
          <PolicyRail
            policy={policy}
            imageUri={imageUri}
            skillsCount={skills.length}
            includeUserTokens={includeUserTokens}
          />
        </div>
      </div>

      {/* sticky save bar — solid, hairline-topped (no gradient fade) */}
      <div className="sticky bottom-0 z-10 mt-6 flex justify-end gap-2.5 border-t bg-background py-3.5">
        <Button
          type="button"
          variant="ghost"
          onClick={() => navigate({ to: "/settings/profiles" })}
          disabled={busy}
        >
          Cancel
        </Button>
        <Button type="submit" disabled={busy}>
          <CheckIcon className="size-4" />
          {mode === "edit" ? "Save changes" : "Create profile"}
        </Button>
      </div>
    </form>
  );
}

function Section({
  icon,
  title,
  sub,
  children,
}: {
  icon?: React.ReactNode;
  title: string;
  sub: string;
  children: React.ReactNode;
}) {
  return (
    <Card>
      <CardHeader>
        <CardTitle className="flex items-center gap-2">
          {icon && <span className="text-muted-foreground">{icon}</span>}
          {title}
        </CardTitle>
        <CardDescription className="leading-relaxed">{sub}</CardDescription>
      </CardHeader>
      <CardContent>{children}</CardContent>
    </Card>
  );
}

/**
 * Repo list editor + autodiscovery. Discovery boots a short-lived session from
 * the profile's image server-side, so it needs a SAVED profile (disabled in
 * create mode) and takes tens of seconds. Results land as candidates the user
 * adds explicitly — nothing is saved until the form is.
 */
function ReposSection({
  repos,
  setRepos,
  profileId,
}: {
  repos: RepoRow[];
  setRepos: (r: RepoRow[]) => void;
  profileId: string | undefined;
}) {
  const discover = useDiscoverProfileRepos();
  const [candidates, setCandidates] = useState<{ path: string; remoteUrl: string }[] | null>(null);

  const setRow = (i: number, patch: Partial<RepoRow>) =>
    setRepos(repos.map((r, j) => (j === i ? { ...r, ...patch } : r)));
  const hasPath = (path: string) => repos.some((r) => r.path === path);

  const runDiscovery = async () => {
    if (!profileId) return;
    setCandidates(null);
    try {
      const resp = await discover.mutateAsync({ profileId });
      // Primary remote = origin when present, else the first listed.
      setCandidates(
        resp.repos.map((r) => ({
          path: r.path,
          remoteUrl: (r.remotes.find((m) => m.name === "origin") ?? r.remotes[0])?.url ?? "",
        })),
      );
    } catch (e) {
      toast.error(e instanceof Error ? e.message : String(e));
    }
  };

  return (
    <div className="flex flex-col gap-3">
      {repos.length === 0 && (
        <Text variant="body" className="text-[0.8rem]">
          No repositories configured.
        </Text>
      )}
      {repos.map((r, i) => (
        <div key={i} className="flex items-center gap-2">
          <Input
            value={r.path}
            onChange={(e) => setRow(i, { path: e.target.value })}
            placeholder="/workspace/my-repo"
            aria-label={`Repo path ${i + 1}`}
            className="font-mono text-sm"
          />
          <Input
            value={r.remoteUrl}
            onChange={(e) => setRow(i, { remoteUrl: e.target.value })}
            placeholder="git@github.com:org/repo.git (optional)"
            aria-label={`Repo remote ${i + 1}`}
            className="font-mono text-sm"
          />
          <Button
            type="button"
            variant="ghost"
            size="sm"
            onClick={() => setRepos(repos.filter((_, j) => j !== i))}
          >
            Remove
          </Button>
        </div>
      ))}
      <div className="flex items-center gap-2">
        <Button
          type="button"
          variant="outline"
          size="sm"
          onClick={() => setRepos([...repos, { path: "", remoteUrl: "" }])}
        >
          <PlusIcon className="size-3.5" /> Add repo
        </Button>
        <Button
          type="button"
          variant="outline"
          size="sm"
          disabled={!profileId || discover.isPending}
          onClick={runDiscovery}
          data-testid="repo-autodiscover"
        >
          {discover.isPending ? "Scanning image… (about a minute)" : "Autodiscover"}
        </Button>
        {!profileId && (
          <Text variant="body" className="text-[0.76rem]">
            Save the profile first to scan its image.
          </Text>
        )}
      </div>
      {candidates !== null && (
        <div className="rounded-md border p-3" data-testid="repo-candidates">
          <Text variant="label" className="text-[0.8rem]">
            Found {candidates.length} {candidates.length === 1 ? "checkout" : "checkouts"}
          </Text>
          {candidates.length === 0 && (
            <Text variant="body" className="mt-1 text-[0.78rem]">
              No git checkouts in the image's workspace roots.
            </Text>
          )}
          <div className="mt-2 flex flex-col gap-1.5">
            {candidates.map((c) => (
              <div key={c.path} className="flex items-center gap-2 text-[0.82rem]">
                <span className="font-mono">{c.path}</span>
                {c.remoteUrl && (
                  <span className="truncate font-mono text-muted-foreground">{c.remoteUrl}</span>
                )}
                <Button
                  type="button"
                  variant="ghost"
                  size="sm"
                  className="ml-auto"
                  disabled={hasPath(c.path)}
                  onClick={() => setRepos([...repos, { path: c.path, remoteUrl: c.remoteUrl }])}
                >
                  {hasPath(c.path) ? "Added" : "Add"}
                </Button>
              </div>
            ))}
          </div>
        </div>
      )}
    </div>
  );
}

function EmptyIntegrations() {
  return (
    <div className="rounded-md border border-dashed p-6 text-center">
      <p className="text-[0.84rem] text-muted-foreground">
        No integrations connected yet — there are no powers to grant.
      </p>
      <Button asChild variant="outline" size="sm" className="mt-3">
        <Link to="/settings/integrations">Go to Integrations</Link>
      </Button>
    </div>
  );
}

interface Skill {
  name: string;
  label: string;
  description: string;
  builtin: boolean;
}

function Advanced({
  skillCatalog,
  skills,
  setSkills,
  envRows,
  setEnvRows,
  secretRows,
  setSecretRows,
  orgSecretNames,
  includeUserTokens,
  setIncludeUserTokens,
  portExposures,
  setPortExposures,
}: {
  skillCatalog: Skill[];
  skills: string[];
  setSkills: (s: string[]) => void;
  envRows: EnvRow[];
  setEnvRows: (r: EnvRow[]) => void;
  secretRows: SecretRow[];
  setSecretRows: (r: SecretRow[]) => void;
  orgSecretNames: string[];
  includeUserTokens: boolean;
  setIncludeUserTokens: (b: boolean) => void;
  portExposures: number[];
  setPortExposures: (p: number[]) => void;
}) {
  const uploadSkill = useUploadSkill();
  const [skillName, setSkillName] = useState("");
  const [skillDesc, setSkillDesc] = useState("");
  const [skillFile, setSkillFile] = useState<File | null>(null);
  const [uploadErr, setUploadErr] = useState<string | null>(null);
  const skillFileInput = useRef<HTMLInputElement>(null);
  const [portInput, setPortInput] = useState("");
  const [portErr, setPortErr] = useState<string | null>(null);

  const addPort = () => {
    const p = Number(portInput);
    if (!Number.isInteger(p) || p < 1 || p > 65535) {
      setPortErr("Enter a port between 1 and 65535");
      return;
    }
    if (portExposures.includes(p)) {
      setPortErr(`Port ${p} is already added`);
      setPortInput("");
      return;
    }
    setPortErr(null);
    setPortExposures([...portExposures, p].sort((a, b) => a - b));
    setPortInput("");
  };

  const onUploadSkill = async () => {
    if (!skillName.trim() || !skillFile) {
      setUploadErr("A name and a SKILL.md (or .tar.gz / .zip) are required.");
      return;
    }
    setUploadErr(null);
    try {
      const payloadTar = new Uint8Array(await skillFile.arrayBuffer());
      await uploadSkill.mutateAsync({
        name: skillName.trim(),
        description: skillDesc.trim(),
        payloadTar,
      });
      toast.success(`Uploaded skill "${skillName.trim()}"`);
      setSkillName("");
      setSkillDesc("");
      setSkillFile(null);
      if (skillFileInput.current) skillFileInput.current.value = "";
    } catch (e) {
      setUploadErr(e instanceof Error ? e.message : String(e));
    }
  };

  return (
    <div className="flex flex-col gap-6">
      {/* skills */}
      <div>
        <Text variant="label">Skills</Text>
        <div className="mt-2 flex flex-col gap-1.5">
          {skillCatalog.map((s) => {
            const on = skills.includes(s.name);
            return (
              <div
                key={s.name}
                className="flex items-center gap-3 rounded-md border bg-background px-3 py-2"
              >
                <Switch
                  checked={on}
                  data-testid={`skill-${s.name}`}
                  onCheckedChange={(v) =>
                    setSkills(v ? [...skills, s.name] : skills.filter((n) => n !== s.name))
                  }
                />
                <div className="flex-1">
                  <div className="flex items-center gap-2 text-[0.84rem]">
                    {s.label}
                    {!s.builtin && (
                      <span className="text-[0.62rem] text-muted-foreground">uploaded</span>
                    )}
                  </div>
                  <div className="text-[0.74rem] text-muted-foreground">{s.description}</div>
                </div>
              </div>
            );
          })}
        </div>
        <div className="mt-2 flex flex-col gap-2 rounded-md border border-dashed p-3">
          <Text variant="label">Upload a skill</Text>
          <Input
            data-testid="skill-upload-name"
            placeholder="skill name (lowercase, dashes)"
            value={skillName}
            onChange={(e) => setSkillName(e.target.value)}
          />
          <Input
            data-testid="skill-upload-desc"
            placeholder="short description"
            value={skillDesc}
            onChange={(e) => setSkillDesc(e.target.value)}
          />
          <p className="text-sm text-muted-foreground">
            Upload a lone SKILL.md or an archive with SKILL.md at its root.
          </p>
          <input
            ref={skillFileInput}
            data-testid="skill-upload-file"
            type="file"
            accept=".md,.markdown,.tar,.tar.gz,.tgz,.zip"
            className="sr-only"
            onChange={(e) => {
              setSkillFile(e.target.files?.[0] ?? null);
              setUploadErr(null);
            }}
          />
          <p className="text-sm text-muted-foreground" aria-live="polite">
            {skillFile ? `Selected: ${skillFile.name}` : "No skill file selected"}
          </p>
          {uploadErr && <p className="text-sm text-destructive">{uploadErr}</p>}
          <Button
            type="button"
            variant="outline"
            size="sm"
            className="self-start"
            data-testid="skill-upload-submit"
            disabled={uploadSkill.isPending}
            onClick={() => {
              if (!skillFile) {
                setUploadErr(null);
                skillFileInput.current?.click();
                return;
              }
              void onUploadSkill();
            }}
          >
            {uploadSkill.isPending
              ? "Uploading…"
              : skillFile
                ? "Upload skill"
                : "Choose skill file"}
          </Button>
        </div>
      </div>

      {/* env vars */}
      <div>
        <Text variant="label">Environment variables</Text>
        <div className="mt-2">
          <EnvVarsEditor rows={envRows} onChange={setEnvRows} />
        </div>
      </div>

      {/* auto-exposed ports (ADR 0064 P4) */}
      <div>
        <Text variant="label">Auto-exposed ports</Text>
        <p className="mt-1 text-[0.74rem] text-muted-foreground">
          Guest ports every session from this profile exposes as private live-host previews (e.g. a
          dev server on 3000). Manage individual previews from a session's Diagnostics → Ports.
        </p>
        <div className="mt-2 flex flex-wrap items-center gap-1.5">
          {portExposures.length === 0 && (
            <span className="text-[0.74rem] text-muted-foreground">
              None — sessions expose nothing by default.
            </span>
          )}
          {portExposures.map((p) => (
            <span
              key={p}
              data-testid={`port-chip-${p}`}
              className="inline-flex items-center gap-1 rounded-md border bg-background px-2 py-1 font-mono text-xs"
            >
              :{p}
              <button
                type="button"
                aria-label={`remove port ${p}`}
                data-testid={`port-remove-${p}`}
                className="text-muted-foreground hover:text-foreground"
                onClick={() => setPortExposures(portExposures.filter((n) => n !== p))}
              >
                ×
              </button>
            </span>
          ))}
        </div>
        <div className="mt-2 flex items-center gap-2">
          <Input
            type="number"
            min={1}
            max={65535}
            placeholder="port (e.g. 3000)"
            className="w-36"
            value={portInput}
            data-testid="port-add-input"
            onChange={(e) => setPortInput(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter") {
                e.preventDefault();
                addPort();
              }
            }}
          />
          <Button
            type="button"
            variant="outline"
            size="sm"
            data-testid="port-add-btn"
            onClick={addPort}
          >
            Add
          </Button>
        </div>
        {portErr && (
          <p className="mt-1 text-sm text-destructive" data-testid="port-add-error">
            {portErr}
          </p>
        )}
      </div>

      {/* custom secrets */}
      <div>
        <Text variant="label">Custom injected secrets</Text>
        <p className="mt-1 mb-2 text-[0.74rem] text-muted-foreground">
          For values not tied to an integration (a DB URL, an internal token). Broker keeps them out
          of the sandbox.
        </p>
        <ProfileSecretsEditor
          rows={secretRows}
          onChange={setSecretRows}
          secretNames={orgSecretNames}
        />
      </div>

      {/* user token */}
      <div className="flex items-center gap-3 rounded-md border bg-background px-3 py-2.5">
        <Switch
          checked={includeUserTokens}
          onCheckedChange={setIncludeUserTokens}
          aria-label="Include the launching user's other saved tokens"
        />
        <div className="flex-1">
          <div className="text-[0.84rem]">Include the launching user's other tokens</div>
          <div className="text-[0.74rem] text-muted-foreground">
            The harness's own credential always rides along. This additionally carries the
            developer's other saved tokens into the sandbox. Leave off for untrusted images.
          </div>
        </div>
      </div>
    </div>
  );
}
