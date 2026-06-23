import { useEffect, useState } from "react";
import { useNavigate, useParams } from "@tanstack/react-router";
import { useForm, Controller } from "react-hook-form";
import { zodResolver } from "@hookform/resolvers/zod";
import * as z from "zod";
import { toast } from "sonner";
import { useProfile, useCreateProfile, useUpdateProfile } from "../../hooks/useProfiles";
import { useEnabledImages } from "../../hooks/useEnabledImages";
import { useSkills, useUploadSkill } from "../../hooks/useSkills";
import { IconPicker } from "../../components/profiles/IconPicker";
import { CapabilityPicker } from "../../components/profiles/CapabilityPicker";
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
import { useOrgSecretNames } from "../../hooks/useOrgSecrets";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Textarea } from "@/components/ui/textarea";
import { Switch } from "@/components/ui/switch";
import {
  Field,
  FieldDescription,
  FieldError,
  FieldGroup,
  FieldLabel,
  FieldLegend,
  FieldSet,
} from "@/components/ui/field";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";

const schema = z.object({
  name: z.string().trim().min(1, "Name is required"),
  description: z.string(),
  icon: z.string().min(1),
  imageId: z.string().min(1, "Select an image"),
  includeUserTokens: z.boolean(),
  // ADR 0055: dynamic skill bundle names this profile's sessions mount.
  skills: z.array(z.string()),
});
type Values = z.infer<typeof schema>;

// ADR 0056: a capability is "provider:action[@resource]" (mirrors
// engram_core::types::Capability::parse + the orchestrator's validator). The
// orchestrator + coordinator re-validate; this just keeps the editor honest.
function isValidCapability(c: string): boolean {
  const at = c.indexOf("@");
  const head = at === -1 ? c : c.slice(0, at);
  const resource = at === -1 ? null : c.slice(at + 1);
  const colon = head.indexOf(":");
  const provider = colon === -1 ? "" : head.slice(0, colon);
  const action = colon === -1 ? "" : head.slice(colon + 1);
  return Boolean(provider) && Boolean(action) && (at === -1 || Boolean(resource));
}

export function SessionProfileEditor({ mode }: { mode: "create" | "edit" }) {
  const navigate = useNavigate();
  const params = useParams({ strict: false }) as { id?: string };
  const editingId = mode === "edit" ? params.id : undefined;
  const { data: existing } = useProfile(editingId);
  const { data: images } = useEnabledImages(true);
  const { data: skills } = useSkills();
  const uploadSkill = useUploadSkill();
  const create = useCreateProfile();
  const update = useUpdateProfile();
  const [envRows, setEnvRows] = useState<EnvRow[]>([]);
  // ADR 0057 D1: capabilities are picked from the connector catalog
  // (provider:action[@resource]); local state assembled into the payload at
  // submit. The orchestrator + coordinator re-validate against the registry.
  const [capabilities, setCapabilities] = useState<string[]>([]);

  // ADR 0057: profile-defined egress network policy + injected secrets (lifted
  // off the image manifest). Local state like envRows/capsText, assembled into
  // the payload at submit.
  const [networkDefault, setNetworkDefault] = useState<"deny" | "allow">("deny");
  const [allowHostsText, setAllowHostsText] = useState("");
  const [allowPatternsText, setAllowPatternsText] = useState("");
  const [secretRows, setSecretRows] = useState<SecretRow[]>([]);
  const { data: orgSecretNames } = useOrgSecretNames();

  // ADR 0055 P2: inline skill upload (admin). Local state, separate from the
  // react-hook-form; on success the catalog query invalidates and the new skill
  // appears as a toggle.
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
      // The coordinator sniffs the form (tar / .tar.gz / .zip / lone SKILL.md)
      // by magic bytes, so the raw file bytes ride the payloadTar field. `owner`
      // is intentionally not set — the orchestrator stamps it from the session.
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

  const form = useForm<Values>({
    resolver: zodResolver(schema),
    defaultValues: {
      name: "",
      description: "",
      icon: "Bot",
      imageId: "",
      includeUserTokens: false,
      skills: [],
    },
  });

  // Hydrate when editing (existing arrives async).
  useEffect(() => {
    if (existing?.profile) {
      const p = existing.profile;
      form.reset({
        name: p.name,
        description: p.description,
        icon: p.icon,
        imageId: p.imageId,
        includeUserTokens: p.includeUserTokens,
        skills: p.skills ?? [],
      });
      setEnvRows(mapToEnvRows(p.envVars));
      setCapabilities(p.capabilities ?? []);
      setNetworkDefault(p.network?.default === "allow" ? "allow" : "deny");
      setAllowHostsText((p.network?.allowHosts ?? []).join("\n"));
      setAllowPatternsText((p.network?.allowHostPatterns ?? []).join("\n"));
      setSecretRows(wireToSecretRows(p.secrets ?? []));
    }
  }, [existing, form]);

  // Create mode: default to the first enabled image until the admin picks one.
  const imageId = form.watch("imageId");
  useEffect(() => {
    if (mode === "create" && !imageId && images && images.length > 0) {
      form.setValue("imageId", images[0].id);
    }
  }, [mode, imageId, images, form]);

  const onSubmit = async (v: Values) => {
    const badCap = capabilities.find((c) => !isValidCapability(c));
    if (badCap) {
      form.setError("root", {
        message: `Invalid capability "${badCap}": expected "provider:action[@resource]"`,
      });
      return;
    }
    const linesOf = (text: string) =>
      text
        .split("\n")
        .map((s) => s.trim())
        .filter(Boolean);
    const network = {
      default: networkDefault,
      allowHosts: linesOf(allowHostsText),
      allowHostPatterns: linesOf(allowPatternsText),
    };
    const secrets = secretRowsToWire(secretRows);
    const payload = { ...v, envVars: envRowsToMap(envRows), capabilities, network, secrets };
    try {
      if (mode === "edit" && editingId) {
        await update.mutateAsync({ id: editingId, ...payload });
        toast.success("Saved changes");
      } else {
        await create.mutateAsync(payload);
        toast.success("Profile created");
      }
      navigate({ to: "/settings/profiles" });
    } catch (e) {
      form.setError("root", { message: e instanceof Error ? e.message : String(e) });
    }
  };

  const busy = form.formState.isSubmitting || create.isPending || update.isPending;

  return (
    <form onSubmit={form.handleSubmit(onSubmit)} className="flex max-w-2xl flex-col gap-6">
      <h1 className="text-lg font-semibold">{mode === "edit" ? "Edit profile" : "New profile"}</h1>

      <FieldSet>
        <FieldLegend>Identity</FieldLegend>
        <FieldGroup>
          <Controller
            name="name"
            control={form.control}
            render={({ field, fieldState }) => (
              <Field data-invalid={fieldState.invalid}>
                <FieldLabel htmlFor="name">Name</FieldLabel>
                <Input {...field} id="name" aria-invalid={fieldState.invalid} />
                {fieldState.invalid && <FieldError errors={[fieldState.error]} />}
              </Field>
            )}
          />
          <Controller
            name="description"
            control={form.control}
            render={({ field }) => (
              <Field>
                <FieldLabel htmlFor="description">Description</FieldLabel>
                <Textarea {...field} id="description" rows={2} />
              </Field>
            )}
          />
          <Controller
            name="icon"
            control={form.control}
            render={({ field }) => (
              <Field>
                <FieldLabel>Icon</FieldLabel>
                <IconPicker value={field.value} onChange={field.onChange} />
              </Field>
            )}
          />
        </FieldGroup>
      </FieldSet>

      <FieldSet>
        <FieldLegend>Launch</FieldLegend>
        <FieldGroup>
          <Controller
            name="imageId"
            control={form.control}
            render={({ field, fieldState }) => (
              <Field data-invalid={fieldState.invalid}>
                <FieldLabel htmlFor="imageId">Image</FieldLabel>
                <Select value={field.value} onValueChange={field.onChange}>
                  <SelectTrigger id="imageId" data-testid="image-select">
                    <SelectValue placeholder="Select an image" />
                  </SelectTrigger>
                  <SelectContent>
                    {(images ?? []).map((i) => (
                      <SelectItem key={i.id} value={i.id}>
                        {i.image_uri}
                      </SelectItem>
                    ))}
                  </SelectContent>
                </Select>
                {fieldState.invalid && <FieldError errors={[fieldState.error]} />}
              </Field>
            )}
          />
        </FieldGroup>
      </FieldSet>

      <FieldSet>
        <FieldLegend>Network</FieldLegend>
        <FieldGroup>
          <Field>
            <FieldLabel htmlFor="net-default">Default egress</FieldLabel>
            <Select
              value={networkDefault}
              onValueChange={(v) => setNetworkDefault(v as "deny" | "allow")}
            >
              <SelectTrigger id="net-default" className="w-40">
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                <SelectItem value="deny">deny</SelectItem>
                <SelectItem value="allow">allow</SelectItem>
              </SelectContent>
            </Select>
            <FieldDescription>
              Which hosts a session may reach. <strong>deny</strong> (recommended) blocks everything
              except the allow-lists below; <strong>allow</strong> opens all egress. Lifted off the
              image manifest (ADR 0057; enforced at boot once B2 lands).
            </FieldDescription>
          </Field>
          <Field>
            <FieldLabel htmlFor="net-hosts">Allowed hosts</FieldLabel>
            <Textarea
              id="net-hosts"
              value={allowHostsText}
              onChange={(e) => setAllowHostsText(e.target.value)}
              rows={3}
              placeholder={"api.github.com\nsentry.io"}
              className="font-mono text-sm"
            />
            <FieldDescription>One exact hostname per line.</FieldDescription>
          </Field>
          <Field>
            <FieldLabel htmlFor="net-patterns">Allowed host patterns</FieldLabel>
            <Textarea
              id="net-patterns"
              value={allowPatternsText}
              onChange={(e) => setAllowPatternsText(e.target.value)}
              rows={2}
              placeholder={"*.pypi.org\n*.githubusercontent.com"}
              className="font-mono text-sm"
            />
            <FieldDescription>
              One leading-wildcard glob per line (e.g. <code>*.example.com</code>).
            </FieldDescription>
          </Field>
        </FieldGroup>
      </FieldSet>

      <FieldSet>
        <FieldLegend>Skills</FieldLegend>
        <FieldGroup>
          <Controller
            name="skills"
            control={form.control}
            render={({ field }) => (
              <>
                {(skills ?? []).map((s) => {
                  const checked = field.value.includes(s.name);
                  return (
                    <Field key={s.name} orientation="horizontal">
                      <Switch
                        id={`skill-${s.name}`}
                        data-testid={`skill-${s.name}`}
                        checked={checked}
                        onCheckedChange={(on) =>
                          field.onChange(
                            on ? [...field.value, s.name] : field.value.filter((n) => n !== s.name),
                          )
                        }
                      />
                      <div>
                        <FieldLabel htmlFor={`skill-${s.name}`}>
                          {s.label}
                          {!s.builtin && (
                            <span className="ml-2 text-xs text-muted-foreground">(uploaded)</span>
                          )}
                        </FieldLabel>
                        <FieldDescription>{s.description}</FieldDescription>
                      </div>
                    </Field>
                  );
                })}
              </>
            )}
          />
          {/* ADR 0055 P2: upload a skill (admin). A lone SKILL.md, or a .tar.gz /
              .zip of the skill dir; the coordinator sniffs + packs it. */}
          <Field>
            <FieldLabel htmlFor="skill-upload-name">Upload a skill</FieldLabel>
            <FieldDescription>
              A skill is a <code>SKILL.md</code> (or a <code>.tar.gz</code> / <code>.zip</code> of
              the skill directory). It joins the org-shared catalog for any profile to select.
            </FieldDescription>
            <Input
              id="skill-upload-name"
              data-testid="skill-upload-name"
              placeholder="skill name (lowercase, dashes)"
              value={skillName}
              onChange={(e) => setSkillName(e.target.value)}
            />
            <Input
              id="skill-upload-desc"
              data-testid="skill-upload-desc"
              placeholder="short description"
              value={skillDesc}
              onChange={(e) => setSkillDesc(e.target.value)}
            />
            <input
              id="skill-upload-file"
              data-testid="skill-upload-file"
              type="file"
              accept=".md,.markdown,.tar,.tar.gz,.tgz,.zip"
              className="text-sm"
              onChange={(e) => setSkillFile(e.target.files?.[0] ?? null)}
            />
            {uploadErr && (
              <p className="text-sm text-destructive" data-testid="skill-upload-error">
                {uploadErr}
              </p>
            )}
            <div>
              <Button
                type="button"
                variant="outline"
                data-testid="skill-upload-submit"
                disabled={uploadSkill.isPending}
                onClick={onUploadSkill}
              >
                {uploadSkill.isPending ? "Uploading…" : "Upload skill"}
              </Button>
            </div>
          </Field>
        </FieldGroup>
      </FieldSet>

      <FieldSet>
        <FieldLegend>Environment</FieldLegend>
        <FieldGroup>
          <Controller
            name="includeUserTokens"
            control={form.control}
            render={({ field }) => (
              <Field orientation="horizontal">
                <Switch
                  id="includeUserTokens"
                  checked={field.value}
                  onCheckedChange={field.onChange}
                />
                <div>
                  <FieldLabel htmlFor="includeUserTokens">Include user tokens</FieldLabel>
                  <FieldDescription>
                    Sessions started from this profile may carry the user's Claude token into the
                    sandbox. Leave off for untrusted or externally-facing images.
                  </FieldDescription>
                </div>
              </Field>
            )}
          />
          <Field>
            <FieldLabel>Environment variables</FieldLabel>
            <EnvVarsEditor rows={envRows} onChange={setEnvRows} />
          </Field>
          <Field>
            <FieldLabel>Integration capabilities</FieldLabel>
            <CapabilityPicker value={capabilities} onChange={setCapabilities} />
            <FieldDescription>
              Toggle the third-party capabilities sessions from this profile are granted (ADR 0056).
              Options come from the connector catalog (Settings → Integrations). Empty = none.
            </FieldDescription>
          </Field>
        </FieldGroup>
      </FieldSet>

      <FieldSet>
        <FieldLegend>Secrets</FieldLegend>
        <FieldGroup>
          <Field>
            <FieldLabel>Injected secrets</FieldLabel>
            <ProfileSecretsEditor
              rows={secretRows}
              onChange={setSecretRows}
              secretNames={orgSecretNames ?? []}
            />
          </Field>
        </FieldGroup>
      </FieldSet>

      {form.formState.errors.root && <FieldError errors={[form.formState.errors.root]} />}

      <div className="flex gap-2">
        <Button type="submit" disabled={busy}>
          {mode === "edit" ? "Save changes" : "Create profile"}
        </Button>
        <Button
          type="button"
          variant="ghost"
          onClick={() => navigate({ to: "/settings/profiles" })}
          disabled={busy}
        >
          Cancel
        </Button>
      </div>
    </form>
  );
}
