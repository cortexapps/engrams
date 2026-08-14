import { useEffect, useMemo, useState } from "react";
import { useNavigate } from "@tanstack/react-router";
import { Check, CheckCheck, Loader2 } from "lucide-react";

import { Button } from "@/components/ui/button";
import { Text } from "@/components/ui/text";
import { Textarea } from "@/components/ui/textarea";
import type { Profile } from "@/gen/engram/app/v1/profile_pb";
import { useProfiles } from "@/hooks/useProfiles";
import { useCreateSpec } from "@/hooks/useSpecCreate";
import { useSpecTemplates, type SpecTemplate } from "@/hooks/useSpecTemplates";
import { TEMPLATE_LOCK_REASON } from "@/pages/specs/template-lock";

function defaultTemplateId(templates: readonly SpecTemplate[]): string {
  return (templates.find((template) => template.builtIn) ?? templates[0])?.id ?? "";
}

export function NewSpecPage() {
  const navigate = useNavigate();
  const templates = useSpecTemplates();
  const profiles = useProfiles();
  const create = useCreateSpec();

  const [prompt, setPrompt] = useState("");
  const [templateId, setTemplateId] = useState("");
  const [profileId, setProfileId] = useState("");

  const templateList = useMemo(() => templates.data ?? [], [templates.data]);
  const profileList = useMemo(() => profiles.data?.profiles ?? [], [profiles.data]);
  const profile = profileList.find((candidate) => candidate.id === profileId);

  useEffect(() => {
    setTemplateId((current) => current || defaultTemplateId(templateList));
  }, [templateList]);
  useEffect(() => {
    setProfileId((current) => current || profileList[0]?.id || "");
  }, [profileList]);

  const signature = JSON.stringify([prompt, templateId, profileId]);
  const [idempotencyKey, setIdempotencyKey] = useState(() => crypto.randomUUID());
  useEffect(() => setIdempotencyKey(crypto.randomUUID()), [signature]);

  const ready = prompt.trim().length > 0 && templateId !== "" && profileId !== "";

  const submit = (event: React.FormEvent) => {
    event.preventDefault();
    if (!ready || create.isPending) return;

    create.mutate(
      {
        templateId,
        profileId,
        problemStatement: prompt.trim(),
        idempotencyKey,
      },
      {
        onSuccess: (spec) => {
          void navigate({ to: "/specs/$specId", params: { specId: spec.id } });
        },
      },
    );
  };

  return (
    <main
      className="fixed inset-0 overflow-y-auto bg-background px-5 py-12 sm:px-10 sm:py-16"
      aria-labelledby="new-spec-title"
      data-testid="new-spec"
    >
      <form className="mx-auto flex w-full max-w-[680px] flex-col gap-7" onSubmit={submit}>
        <header className="grid gap-3">
          <Text as="p" variant="label" tone="muted">
            New spec
          </Text>
          <Text as="h1" id="new-spec-title" variant="display" className="text-[2rem]">
            What are we designing?
          </Text>
          <Text tone="muted" className="max-w-[60ch] text-[0.92rem]">
            Rough is fine. I read the repository before I ask you anything, so the first thing you
            see is what I found — not a blank page.
          </Text>
        </header>

        <section className="rounded-lg border bg-card p-[18px] shadow-xs">
          <label className="sr-only" htmlFor="new-spec-prompt">
            What is the spec about?
          </label>
          <Textarea
            id="new-spec-prompt"
            autoFocus
            rows={3}
            value={prompt}
            onChange={(event) => setPrompt(event.target.value)}
            placeholder="Per-org caps on sandbox creation, plus a meter we can bill against."
            className="min-h-24 resize-none border-0 bg-transparent p-0 text-base shadow-none focus-visible:ring-0 dark:bg-transparent"
          />
          <div className="mt-4 flex min-h-8 flex-wrap items-center gap-2 border-t pt-4">
            <span className="inline-flex min-w-0 items-center gap-1.5 rounded-md border bg-background px-2 py-1">
              <CheckCheck aria-hidden="true" className="size-3.5 shrink-0" />
              <Text as="span" variant="code" tone="muted" className="truncate text-xs">
                {profileContext(profile)}
              </Text>
            </span>
            {profile && profile.repos.length > 1 ? (
              <Text as="span" variant="code" tone="muted" className="text-xs">
                +{profile.repos.length - 1} more
              </Text>
            ) : null}
          </div>
        </section>

        <fieldset className="grid gap-3">
          <div className="flex items-baseline justify-between gap-4">
            <Text as="legend" variant="label" tone="muted">
              Shape
            </Text>
            <Text variant="code" tone="muted" className="text-right text-xs">
              {TEMPLATE_LOCK_REASON}
            </Text>
          </div>

          {templates.isPending ? (
            <Text tone="muted">Loading shapes…</Text>
          ) : templates.error ? (
            <Text role="alert" tone="destructive">
              The shapes did not load. {templates.error.message}
            </Text>
          ) : (
            <div
              className="grid grid-cols-1 gap-3 sm:grid-cols-3"
              role="radiogroup"
              aria-label="Shape"
            >
              {templateList.map((template) => {
                const selected = template.id === templateId;
                return (
                  <button
                    key={template.id}
                    type="button"
                    role="radio"
                    aria-checked={selected}
                    data-selected={selected || undefined}
                    className="relative min-h-32 rounded-lg border bg-card p-4 text-left shadow-xs transition-[border-color,background-color,box-shadow] outline-none hover:bg-accent/40 focus-visible:border-ring focus-visible:ring-2 focus-visible:ring-ring/60 data-[selected=true]:border-ring data-[selected=true]:bg-accent/40"
                    onClick={() => setTemplateId(template.id)}
                  >
                    {selected ? (
                      <Check
                        aria-hidden="true"
                        className="absolute top-3 right-3 size-4 text-ring"
                      />
                    ) : null}
                    <Text as="span" variant="heading" className="block pr-5 text-sm">
                      {template.name}
                    </Text>
                    <Text as="span" tone="muted" className="mt-2 block text-[0.79rem]">
                      {template.description}
                    </Text>
                    <Text as="span" variant="code" tone="muted" className="mt-3 block text-xs">
                      {template.sections.length}{" "}
                      {template.sections.length === 1 ? "section" : "sections"}
                    </Text>
                  </button>
                );
              })}
            </div>
          )}
        </fieldset>

        {profiles.error ? (
          <Text role="alert" tone="destructive">
            The profiles did not load. {profiles.error.message}
          </Text>
        ) : !profiles.isPending && profileList.length === 0 ? (
          <Text role="alert" tone="destructive">
            No active profile is available. Add a profile before you start a spec.
          </Text>
        ) : null}

        {create.error ? (
          <Text role="alert" tone="destructive">
            The spec did not start. {create.error.message}
          </Text>
        ) : null}

        <footer className="flex flex-wrap items-center gap-4">
          <Button type="submit" disabled={!ready || create.isPending}>
            {create.isPending ? <Loader2 aria-hidden="true" className="animate-spin" /> : null}
            Start
          </Button>
          <Text variant="code" tone="muted" className="text-xs">
            Recon takes about 40 seconds
          </Text>
        </footer>
      </form>
    </main>
  );
}

function profileContext(profile: Pick<Profile, "name" | "repos"> | undefined): string {
  const repo = profile?.repos[0];
  if (repo?.remote?.owner && repo.remote.name) {
    return `${repo.remote.owner}/${repo.remote.name}`;
  }
  if (repo?.path) {
    const name = repo.path.replace(/[/]$/, "").split("/").at(-1);
    if (name) return name;
  }
  return profile?.name ?? "Profile";
}
