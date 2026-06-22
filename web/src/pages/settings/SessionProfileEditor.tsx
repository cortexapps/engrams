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
import {
  EnvVarsEditor,
  envRowsToMap,
  mapToEnvRows,
  type EnvRow,
} from "../../components/profiles/EnvVarsEditor";
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
  // ADR 0056: capabilities are a free-form list (no catalog) — one
  // "provider:action[@resource]" per line. Local state like envRows; parsed +
  // validated into the payload at submit.
  const [capsText, setCapsText] = useState("");

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
      setCapsText((p.capabilities ?? []).join("\n"));
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
    const capabilities = capsText
      .split("\n")
      .map((s) => s.trim())
      .filter(Boolean);
    const badCap = capabilities.find((c) => !isValidCapability(c));
    if (badCap) {
      form.setError("root", {
        message: `Invalid capability "${badCap}": expected "provider:action[@resource]"`,
      });
      return;
    }
    const payload = { ...v, envVars: envRowsToMap(envRows), capabilities };
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
            <FieldLabel htmlFor="capabilities">Integration capabilities</FieldLabel>
            <Textarea
              id="capabilities"
              value={capsText}
              onChange={(e) => setCapsText(e.target.value)}
              rows={3}
              placeholder={
                "github:contents:write@cortexapps/engrams\ngithub:issues:write\ndatadog:logs:read"
              }
              className="font-mono text-sm"
            />
            <FieldDescription>
              One per line, <code>provider:action[@resource]</code>. Sessions from this profile are
              granted these third-party integration capabilities (ADR 0056). Empty = none.
            </FieldDescription>
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
