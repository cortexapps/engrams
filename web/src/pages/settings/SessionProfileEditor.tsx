/**
 * SessionProfileEditor (redesign) — capability-first. The spine is "what can
 * sessions from this profile DO": enable a connected integration (binds its
 * credential + opens its egress), then pick exactly which powers. A live
 * Session-policy rail shows the receipts. Network is automatic (deny-by-default,
 * derived from the granted powers); skills / env / custom secrets / user-token
 * live under Advanced.
 */

import { useEffect, useMemo, useState } from "react";
import { useNavigate, useParams, Link } from "@tanstack/react-router";
import { toast } from "sonner";
import {
  CheckIcon,
  ChevronDownIcon,
  ChevronLeftIcon,
  GlobeIcon,
  LockIcon,
  PlusIcon,
  ShieldCheckIcon,
} from "lucide-react";

import { useProfile, useCreateProfile, useUpdateProfile } from "../../hooks/useProfiles";
import { useEnabledImages } from "../../hooks/useEnabledImages";
import { useSkills, useUploadSkill } from "../../hooks/useSkills";
import { useOrgSecretNames } from "../../hooks/useOrgSecrets";
import {
  useConnectorViews,
  type ConnectorView,
} from "../../components/integrations/useConnectorViews";
import { IconPicker } from "../../components/profiles/IconPicker";
import { PowerSelector } from "../../components/profiles/PowerSelector";
import { PolicyRail } from "../../components/profiles/PolicyRail";
import { ProviderTile } from "../../components/integrations/ProviderTile";
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
import { Button } from "@/components/ui/button";
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

export function SessionProfileEditor({ mode }: { mode: "create" | "edit" }) {
  const navigate = useNavigate();
  const params = useParams({ strict: false }) as { id?: string };
  const editingId = mode === "edit" ? params.id : undefined;
  const { data: existing } = useProfile(editingId);
  const { data: images } = useEnabledImages(true);
  const { data: skillCatalog } = useSkills();
  const { data: orgSecretNames } = useOrgSecretNames();
  const { views } = useConnectorViews();
  const create = useCreateProfile();
  const update = useUpdateProfile();

  const [name, setName] = useState("");
  const [description, setDescription] = useState("");
  const [icon, setIcon] = useState("Bot");
  const [imageId, setImageId] = useState("");
  const [includeUserTokens, setIncludeUserTokens] = useState(false);
  const [skills, setSkills] = useState<string[]>([]);
  const [capabilities, setCapabilities] = useState<string[]>([]);
  const [envRows, setEnvRows] = useState<EnvRow[]>([]);
  const [allowHostsText, setAllowHostsText] = useState("");
  const [allowPatternsText, setAllowPatternsText] = useState("");
  const [secretRows, setSecretRows] = useState<SecretRow[]>([]);
  const [netOpen, setNetOpen] = useState(false);
  const [advanced, setAdvanced] = useState(false);
  const [error, setError] = useState<string | null>(null);

  // Hydrate when editing.
  useEffect(() => {
    const p = existing?.profile;
    if (!p) return;
    setName(p.name);
    setDescription(p.description);
    setIcon(p.icon);
    setImageId(p.imageId);
    setIncludeUserTokens(p.includeUserTokens);
    setSkills(p.skills ?? []);
    setCapabilities(p.capabilities ?? []);
    setEnvRows(mapToEnvRows(p.envVars));
    setAllowHostsText((p.network?.allowHosts ?? []).join("\n"));
    setAllowPatternsText((p.network?.allowHostPatterns ?? []).join("\n"));
    setSecretRows(wireToSecretRows(p.secrets ?? []));
    if ((p.network?.allowHosts ?? []).length || (p.network?.allowHostPatterns ?? []).length)
      setNetOpen(true);
  }, [existing]);

  // Default to the first enabled image in create mode.
  useEffect(() => {
    if (mode === "create" && !imageId && images && images.length > 0) setImageId(images[0]!.id);
  }, [mode, imageId, images]);

  const network = useMemo(
    () => ({
      default: "deny" as const,
      allowHosts: linesOf(allowHostsText),
      allowHostPatterns: linesOf(allowPatternsText),
    }),
    [allowHostsText, allowPatternsText],
  );
  const secretsWire = useMemo(() => secretRowsToWire(secretRows), [secretRows]);
  const policy = useMemo(
    () =>
      derivePolicy(
        {
          capabilities,
          network,
          secrets: secretRows.map((r) => ({ ref: r.ref, envVar: r.envVar, mode: r.mode })),
        },
        views,
      ),
    [capabilities, network, secretRows, views],
  );
  const connected = views.filter((v) => v.status === "connected");
  const imageUri = images?.find((i) => i.id === imageId)?.image_uri;

  // --- capability helpers (enable→select) -----------------------------------
  const capOn = (provider: string, action: string) => {
    const cap = `${provider}:${action}`;
    return capabilities.some((x) => x === cap || x.startsWith(`${cap}@`));
  };
  const toggleCap = (provider: string, action: string, on: boolean) => {
    const cap = `${provider}:${action}`;
    setCapabilities((caps) => {
      const without = caps.filter((x) => x !== cap && !x.startsWith(`${cap}@`));
      return on ? [...without, cap] : without;
    });
  };
  const enableProvider = (v: ConnectorView) => {
    const reads = v.capabilities.filter((c) => c.access === "read");
    const pick = (reads.length ? reads : v.capabilities.slice(0, 1)).map(
      (c) => `${v.provider}:${c.action}`,
    );
    setCapabilities((caps) => [...new Set([...caps, ...pick])]);
  };
  const disableProvider = (v: ConnectorView) =>
    setCapabilities((caps) => caps.filter((x) => !x.startsWith(`${v.provider}:`)));

  const save = async () => {
    if (!name.trim()) {
      setError("Name the profile first");
      return;
    }
    if (!imageId) {
      setError("Select an image");
      return;
    }
    setError(null);
    const payload = {
      name: name.trim(),
      description,
      icon,
      imageId,
      includeUserTokens,
      skills,
      capabilities,
      envVars: envRowsToMap(envRows),
      network,
      secrets: secretsWire,
    };
    try {
      if (mode === "edit" && editingId) {
        await update.mutateAsync({ id: editingId, ...payload });
        toast.success("Saved changes");
      } else {
        await create.mutateAsync(payload);
        toast.success(`Profile "${name.trim()}" created`);
      }
      navigate({ to: "/settings/profiles" });
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  };

  const busy = create.isPending || update.isPending;

  return (
    <div className="mx-auto max-w-5xl">
      <Button asChild variant="ghost" size="sm" className="-ml-2 mb-3 text-muted-foreground">
        <Link to="/settings/profiles">
          <ChevronLeftIcon className="size-4" />
          Profiles
        </Link>
      </Button>

      <div className="grid items-start gap-7 lg:grid-cols-[minmax(0,1fr)_312px]">
        <div className="flex flex-col gap-6">
          {/* identity */}
          <div className="flex items-start gap-3.5">
            <IconPicker value={icon} onChange={setIcon} />
            <div className="flex flex-1 flex-col gap-2">
              <Input
                value={name}
                onChange={(e) => setName(e.target.value)}
                placeholder="Profile name"
                aria-label="Profile name"
                className="h-auto py-2 font-display text-lg font-semibold"
              />
              <Input
                value={description}
                onChange={(e) => setDescription(e.target.value)}
                placeholder="What is this profile for?"
                aria-label="Description"
                className="text-sm"
              />
            </div>
          </div>

          <Block
            icon={<GlobeIcon className="size-4" />}
            title="Launches"
            sub="The image every session from this profile boots."
          >
            <Select value={imageId} onValueChange={setImageId}>
              <SelectTrigger data-testid="image-select" className="max-w-md font-mono">
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
          </Block>

          <Block
            icon={<ShieldCheckIcon className="size-4" />}
            title="Integrations"
            sub="Enable an integration to bind its credential and open its egress — then choose exactly which powers sessions get."
          >
            {connected.length === 0 ? (
              <EmptyIntegrations />
            ) : (
              <div className="flex flex-col gap-3">
                {connected.map((v) => {
                  const grantedCount = v.capabilities.filter((c) =>
                    capOn(v.provider, c.action),
                  ).length;
                  const on = grantedCount > 0;
                  return (
                    <div
                      key={v.provider}
                      className={`overflow-hidden rounded-lg border ${on ? "border-ring/40" : "border-border"}`}
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
                            isOn={(action) => capOn(v.provider, action)}
                            onToggle={(action, value) => toggleCap(v.provider, action, value)}
                          />
                        </div>
                      )}
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
          </Block>

          {/* network */}
          <Block
            icon={<LockIcon className="size-4" />}
            title="Network"
            sub="Deny by default. Sessions reach only what their powers open — add extra hosts only if a power can't."
          >
            <div className="flex items-center gap-2.5 rounded-md border bg-card px-3.5 py-2.5">
              <LockIcon className="size-4 text-instrument-nominal" />
              <div className="flex-1 text-[0.82rem]">
                <strong>Automatic egress.</strong>{" "}
                <span className="text-muted-foreground">
                  {policy.derivedHosts.length === 0
                    ? "No powers granted — sessions are fully sandboxed."
                    : `${policy.derivedHosts.length} host${policy.derivedHosts.length === 1 ? "" : "s"} opened by granted powers.`}
                </span>
              </div>
            </div>
            <div className="mt-2.5 flex flex-wrap gap-1.5">
              {policy.derivedHosts.map((h) => (
                <HostChip key={h} host={h} derived />
              ))}
              {[...policy.extraHosts, ...policy.extraPatterns].map((h) => (
                <HostChip key={h} host={h} />
              ))}
            </div>
            <button
              type="button"
              onClick={() => setNetOpen((v) => !v)}
              className="mt-3 inline-flex items-center gap-1.5 text-[0.78rem] text-muted-foreground"
            >
              <ChevronDownIcon
                className={`size-3.5 transition-transform ${netOpen ? "rotate-180" : ""}`}
              />
              Add extra hosts
            </button>
            {netOpen && (
              <div className="mt-2.5 grid grid-cols-2 gap-3">
                <label className="flex flex-col gap-1.5">
                  <Text variant="label">Allowed hosts</Text>
                  <Textarea
                    rows={3}
                    value={allowHostsText}
                    onChange={(e) => setAllowHostsText(e.target.value)}
                    placeholder={"db.internal\nregistry.npmjs.org"}
                    className="font-mono text-sm"
                  />
                </label>
                <label className="flex flex-col gap-1.5">
                  <Text variant="label">Host patterns</Text>
                  <Textarea
                    rows={3}
                    value={allowPatternsText}
                    onChange={(e) => setAllowPatternsText(e.target.value)}
                    placeholder={"*.githubusercontent.com\n*.pypi.org"}
                    className="font-mono text-sm"
                  />
                </label>
              </div>
            )}
          </Block>

          {/* advanced */}
          <div>
            <button
              type="button"
              onClick={() => setAdvanced((v) => !v)}
              className="flex w-full items-center gap-2 border-t py-2.5 text-left"
            >
              <ChevronDownIcon
                className={`size-4 text-muted-foreground transition-transform ${advanced ? "rotate-180" : ""}`}
              />
              <Text variant="label">Advanced</Text>
              <span className="text-[0.76rem] text-muted-foreground">
                skills · environment · custom secrets · user token
              </span>
            </button>
            {advanced && (
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
                setIncludeUserTokens={setIncludeUserTokens}
              />
            )}
          </div>

          {error && <p className="text-sm text-destructive">{error}</p>}
        </div>

        <PolicyRail
          policy={policy}
          imageUri={imageUri}
          skillsCount={skills.length}
          includeUserTokens={includeUserTokens}
        />
      </div>

      {/* sticky save bar */}
      <div className="sticky bottom-0 mt-7 flex justify-end gap-2.5 bg-gradient-to-t from-background to-transparent py-3.5">
        <Button
          variant="ghost"
          onClick={() => navigate({ to: "/settings/profiles" })}
          disabled={busy}
        >
          Cancel
        </Button>
        <Button onClick={save} disabled={busy}>
          <CheckIcon className="size-4" />
          {mode === "edit" ? "Save changes" : "Create profile"}
        </Button>
      </div>
    </div>
  );
}

function Block({
  icon,
  title,
  sub,
  children,
}: {
  icon: React.ReactNode;
  title: string;
  sub: string;
  children: React.ReactNode;
}) {
  return (
    <section>
      <div className="mb-1 flex items-center gap-2 text-muted-foreground">
        {icon}
        <h2 className="font-display text-base font-semibold text-foreground">{title}</h2>
      </div>
      <p className="mb-3 ml-6 text-[0.8rem] leading-relaxed text-muted-foreground">{sub}</p>
      <div className="ml-6">{children}</div>
    </section>
  );
}

function EmptyIntegrations() {
  return (
    <div className="rounded-lg border border-dashed p-6 text-center">
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
}) {
  const uploadSkill = useUploadSkill();
  const [skillName, setSkillName] = useState("");
  const [skillDesc, setSkillDesc] = useState("");
  const [skillFile, setSkillFile] = useState<File | null>(null);
  const [uploadErr, setUploadErr] = useState<string | null>(null);

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
    } catch (e) {
      setUploadErr(e instanceof Error ? e.message : String(e));
    }
  };

  return (
    <div className="ml-6 mt-4 flex flex-col gap-6">
      {/* skills */}
      <div>
        <Text variant="label">Skills</Text>
        <div className="mt-2 flex flex-col gap-1.5">
          {skillCatalog.map((s) => {
            const on = skills.includes(s.name);
            return (
              <div
                key={s.name}
                className="flex items-center gap-3 rounded-md border bg-card px-3 py-2"
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
          <input
            data-testid="skill-upload-file"
            type="file"
            accept=".md,.markdown,.tar,.tar.gz,.tgz,.zip"
            className="text-sm"
            onChange={(e) => setSkillFile(e.target.files?.[0] ?? null)}
          />
          {uploadErr && <p className="text-sm text-destructive">{uploadErr}</p>}
          <Button
            type="button"
            variant="outline"
            size="sm"
            className="self-start"
            data-testid="skill-upload-submit"
            disabled={uploadSkill.isPending}
            onClick={onUploadSkill}
          >
            {uploadSkill.isPending ? "Uploading…" : "Upload skill"}
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
      <div className="flex items-center gap-3 rounded-md border bg-card px-3 py-2.5">
        <Switch
          checked={includeUserTokens}
          onCheckedChange={setIncludeUserTokens}
          aria-label="Include the launching user's token"
        />
        <div className="flex-1">
          <div className="text-[0.84rem]">Include the launching user's token</div>
          <div className="text-[0.74rem] text-muted-foreground">
            Carries the developer's Claude token into the sandbox. Leave off for untrusted images.
          </div>
        </div>
      </div>
    </div>
  );
}
