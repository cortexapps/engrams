/**
 * The "New spec" creation flow (ADR 0114 D3, R2-R6, mock 2b).
 *
 * The sheet asks for the problem first, then where the session runs, then which
 * template shapes the document. The template cards show structure AND process
 * at the point of choice (R2), because that choice is locked once the session
 * starts (R3). Creation lives here and not in the session composer (R6).
 */

import { useEffect, useMemo, useState } from "react";
import { useNavigate } from "@tanstack/react-router";
import { Loader2, Lock } from "lucide-react";

import { Button } from "@/components/ui/button";
import {
  Field,
  FieldDescription,
  FieldError,
  FieldGroup,
  FieldLabel,
  FieldLegend,
  FieldSet,
} from "@/components/ui/field";
import { Input } from "@/components/ui/input";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import {
  Sheet,
  SheetContent,
  SheetDescription,
  SheetFooter,
  SheetHeader,
  SheetTitle,
} from "@/components/ui/sheet";
import { Textarea } from "@/components/ui/textarea";
import { useCreateSpec } from "@/hooks/useSpecCreate";
import { useProfiles } from "@/hooks/useProfiles";
import {
  useSpecTemplates,
  type SpecTemplate,
  type SpecTemplateStageMode,
} from "@/hooks/useSpecTemplates";
import { TEMPLATE_LOCK_REASON } from "./template-lock";

/** The stage words, kept identical to the template editor's own labels. */
const STAGES = [
  { key: "alternatives", label: "Alternatives" },
  { key: "talkItThrough", label: "Talk it through" },
  { key: "gapCheck", label: "Gap check" },
] as const;

const STAGE_MODE_LABEL: Record<SpecTemplateStageMode, string> = {
  on: "On",
  suggested: "Suggested",
  off: "Off",
};

/**
 * The organization default.
 *
 * A built-in template belongs to no organization, so it is the one every
 * organization starts from. An organization that has none falls back to the
 * first template in the catalog, which is the same order the Templates tab
 * shows.
 */
function defaultTemplateId(templates: readonly SpecTemplate[]): string {
  return (templates.find((template) => template.builtIn) ?? templates[0])?.id ?? "";
}

export function NewSpecSheet({
  open,
  onOpenChange,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
}) {
  const navigate = useNavigate();
  const templates = useSpecTemplates();
  const profiles = useProfiles();
  const create = useCreateSpec();

  const [problemStatement, setProblemStatement] = useState("");
  const [title, setTitle] = useState("");
  const [templateId, setTemplateId] = useState("");
  const [profileId, setProfileId] = useState("");

  const templateList = useMemo(() => templates.data ?? [], [templates.data]);
  const profileList = useMemo(() => profiles.data?.profiles ?? [], [profiles.data]);

  // Preselect the organization default and the first profile as soon as each
  // list arrives, and let a later choice stand.
  useEffect(() => {
    setTemplateId((current) => (current ? current : defaultTemplateId(templateList)));
  }, [templateList]);
  useEffect(() => {
    setProfileId((current) => (current ? current : (profileList[0]?.id ?? "")));
  }, [profileList]);

  // One key per distinct request. A resend of the same request replays the
  // first create; an edited request gets a new key and creates its own spec.
  const signature = JSON.stringify([problemStatement, title, templateId, profileId]);
  const [idempotencyKey, setIdempotencyKey] = useState(() => crypto.randomUUID());
  useEffect(() => setIdempotencyKey(crypto.randomUUID()), [signature]);

  const ready = problemStatement.trim().length > 0 && templateId !== "" && profileId !== "";

  const submit = (event: React.FormEvent) => {
    event.preventDefault();
    if (!ready || create.isPending) return;
    create.mutate(
      {
        templateId,
        profileId,
        problemStatement: problemStatement.trim(),
        idempotencyKey,
        ...(title.trim() ? { title: title.trim() } : {}),
      },
      {
        onSuccess: (spec) => {
          onOpenChange(false);
          setProblemStatement("");
          setTitle("");
          void navigate({ to: "/specs/$specId", params: { specId: spec.id } });
        },
      },
    );
  };

  return (
    <Sheet open={open} onOpenChange={onOpenChange}>
      <SheetContent side="right" className="w-full gap-0 p-0 sm:max-w-2xl">
        <SheetHeader className="border-b">
          <SheetTitle>New spec</SheetTitle>
          <SheetDescription>
            The session opens on your problem statement, reads the repository, and drafts into the
            document.
          </SheetDescription>
        </SheetHeader>

        <form onSubmit={submit} className="flex min-h-0 flex-1 flex-col">
          <div className="flex-1 overflow-y-auto p-4">
            <FieldGroup>
              <Field>
                <FieldLabel htmlFor="new-spec-problem">What problem are you solving?</FieldLabel>
                <Textarea
                  id="new-spec-problem"
                  autoFocus
                  rows={5}
                  value={problemStatement}
                  onChange={(event) => setProblemStatement(event.target.value)}
                  placeholder="Describe the problem, who it affects, and why it matters now."
                />
                <FieldDescription>
                  The agent starts from this statement. It names what it found in the repository
                  before it asks you anything.
                </FieldDescription>
              </Field>

              <Field>
                <FieldLabel htmlFor="new-spec-profile">Profile</FieldLabel>
                <Select value={profileId} onValueChange={setProfileId}>
                  <SelectTrigger id="new-spec-profile" aria-label="Profile">
                    <SelectValue placeholder="Choose a profile" />
                  </SelectTrigger>
                  <SelectContent>
                    {profileList.map((profile) => (
                      <SelectItem key={profile.id} value={profile.id}>
                        {profile.name}
                      </SelectItem>
                    ))}
                  </SelectContent>
                </Select>
                <FieldDescription>
                  A spec session is an ordinary session. It gets this profile's repositories and
                  tools.
                </FieldDescription>
              </Field>

              <FieldSet>
                <FieldLegend>Template</FieldLegend>
                <FieldDescription className="flex items-center gap-1.5">
                  <Lock aria-hidden className="size-3.5" />
                  {TEMPLATE_LOCK_REASON}
                </FieldDescription>
                {templates.isPending && <p className="text-sm text-muted-foreground">Loading…</p>}
                {templates.error && (
                  <p className="text-sm text-destructive">
                    The templates did not load. {templates.error.message}
                  </p>
                )}
                <div role="radiogroup" aria-label="Template" className="grid gap-3">
                  {templateList.map((template) => (
                    <TemplateCard
                      key={template.id}
                      template={template}
                      selected={template.id === templateId}
                      isDefault={template.id === defaultTemplateId(templateList)}
                      onSelect={() => setTemplateId(template.id)}
                    />
                  ))}
                </div>
              </FieldSet>

              <Field>
                <FieldLabel htmlFor="new-spec-title">Title</FieldLabel>
                <Input
                  id="new-spec-title"
                  value={title}
                  onChange={(event) => setTitle(event.target.value)}
                  placeholder="Named from the problem statement when you leave this empty"
                />
                <FieldDescription>The session takes the same title.</FieldDescription>
              </Field>

              {create.error && <FieldError>{create.error.message}</FieldError>}
            </FieldGroup>
          </div>

          <SheetFooter className="flex-row justify-end border-t">
            <Button type="button" variant="outline" onClick={() => onOpenChange(false)}>
              Cancel
            </Button>
            <Button type="submit" disabled={!ready || create.isPending}>
              {create.isPending && <Loader2 aria-hidden className="animate-spin" />}
              Create spec
            </Button>
          </SheetFooter>
        </form>
      </SheetContent>
    </Sheet>
  );
}

/** One template, showing its structure and its process before the choice (R2). */
function TemplateCard({
  template,
  selected,
  isDefault,
  onSelect,
}: {
  template: SpecTemplate;
  selected: boolean;
  isDefault: boolean;
  onSelect: () => void;
}) {
  return (
    <label
      data-selected={selected}
      className="flex cursor-pointer gap-3 rounded-lg border p-3 transition-colors data-[selected=true]:border-primary data-[selected=true]:bg-accent/40"
    >
      <input
        type="radio"
        name="new-spec-template"
        className="mt-1 self-start"
        value={template.id}
        checked={selected}
        onChange={onSelect}
      />
      <div className="min-w-0 flex-1">
        <div className="flex items-center gap-2">
          <span className="font-medium">{template.name}</span>
          {isDefault && (
            <span className="rounded border px-1.5 py-0.5 text-xs text-muted-foreground">
              Default
            </span>
          )}
        </div>
        {template.description && (
          <p className="mt-0.5 text-sm text-muted-foreground">{template.description}</p>
        )}

        <dl className="mt-3 grid gap-2 text-sm">
          <div>
            <dt className="text-xs font-medium tracking-wide text-muted-foreground uppercase">
              Structure
            </dt>
            <dd className="mt-1 grid gap-1">
              {template.layers.map((layer) => (
                <div key={layer.key} className="flex flex-wrap items-baseline gap-x-2">
                  <span className="font-medium">{layer.title}</span>
                  <span className="text-muted-foreground">
                    {template.sections
                      .filter((section) => section.layerKey === layer.key)
                      .map((section) => section.title)
                      .join(" · ") || "No sections"}
                  </span>
                </div>
              ))}
            </dd>
          </div>
          <div>
            <dt className="text-xs font-medium tracking-wide text-muted-foreground uppercase">
              Process
            </dt>
            <dd className="mt-1 flex flex-wrap gap-x-4 gap-y-1">
              {STAGES.map((stage) => (
                <span key={stage.key} className="text-muted-foreground">
                  {stage.label}{" "}
                  <span className="font-medium text-foreground">
                    {STAGE_MODE_LABEL[template.stageFlags[stage.key]]}
                  </span>
                </span>
              ))}
            </dd>
          </div>
        </dl>
      </div>
    </label>
  );
}
