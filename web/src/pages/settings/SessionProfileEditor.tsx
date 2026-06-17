import { useEffect, useState } from "react";
import { useNavigate, useParams } from "@tanstack/react-router";
import { useForm, Controller } from "react-hook-form";
import { zodResolver } from "@hookform/resolvers/zod";
import * as z from "zod";
import { toast } from "sonner";
import { useProfile, useCreateProfile, useUpdateProfile } from "../../hooks/useProfiles";
import { useEnabledImages } from "../../hooks/useEnabledImages";
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
});
type Values = z.infer<typeof schema>;

export function SessionProfileEditor({ mode }: { mode: "create" | "edit" }) {
  const navigate = useNavigate();
  const params = useParams({ strict: false }) as { id?: string };
  const editingId = mode === "edit" ? params.id : undefined;
  const { data: existing } = useProfile(editingId);
  const { data: images } = useEnabledImages(true);
  const create = useCreateProfile();
  const update = useUpdateProfile();
  const [envRows, setEnvRows] = useState<EnvRow[]>([]);

  const form = useForm<Values>({
    resolver: zodResolver(schema),
    defaultValues: {
      name: "",
      description: "",
      icon: "Bot",
      imageId: "",
      includeUserTokens: false,
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
      });
      setEnvRows(mapToEnvRows(p.envVars));
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
    const payload = { ...v, envVars: envRowsToMap(envRows) };
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
    <form onSubmit={form.handleSubmit(onSubmit)} className="mx-auto flex max-w-2xl flex-col gap-6">
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
                    Sessions started from this profile may carry the user's Claude credentials into
                    the sandbox. Leave off for untrusted or externally-facing images.
                  </FieldDescription>
                </div>
              </Field>
            )}
          />
          <Field>
            <FieldLabel>Environment variables</FieldLabel>
            <EnvVarsEditor rows={envRows} onChange={setEnvRows} />
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
