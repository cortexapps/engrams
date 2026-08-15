import { useEffect, useMemo, useState } from "react";
import { useNavigate } from "@tanstack/react-router";
import { Check } from "lucide-react";
import { RadioGroup as RadioGroupPrimitive } from "radix-ui";

import { TaskComposer, type TaskComposerState } from "@/components/composer/TaskComposer";
import { RadioGroup } from "@/components/ui/radio-group";
import { Text } from "@/components/ui/text";
import { useCreateSpec } from "@/hooks/useSpecCreate";
import { useSpecTemplates, type SpecTemplate } from "@/hooks/useSpecTemplates";
import { TEMPLATE_LOCK_REASON } from "@/pages/specs/template-lock";

function defaultTemplateId(templates: readonly SpecTemplate[]): string {
  return (templates.find((template) => template.builtIn) ?? templates[0])?.id ?? "";
}

export function NewSpecPage() {
  const navigate = useNavigate();
  const templates = useSpecTemplates();
  const create = useCreateSpec();

  const [templateId, setTemplateId] = useState("");
  const [composerState, setComposerState] = useState<TaskComposerState | null>(null);
  const templateList = useMemo(() => templates.data ?? [], [templates.data]);

  useEffect(() => {
    setTemplateId((current) => current || defaultTemplateId(templateList));
  }, [templateList]);

  const signature = JSON.stringify([
    composerState?.prompt ?? "",
    templateId,
    composerState?.profileId ?? "",
    composerState?.harnessOverride.harness ?? null,
    composerState?.harnessOverride.model ?? null,
    composerState?.harnessOverride.modelRouter ?? null,
    composerState?.harnessOverride.effort ?? null,
    composerState?.harnessOverride.mode ?? null,
  ]);
  const [idempotencyKey, setIdempotencyKey] = useState(() => crypto.randomUUID());
  useEffect(() => setIdempotencyKey(crypto.randomUUID()), [signature]);

  const submit = (state: TaskComposerState) => {
    if (!state.valid || !state.profileId || !templateId || create.isPending) return;
    const { harnessOverride } = state;
    create.mutate(
      {
        templateId,
        profileId: state.profileId,
        problemStatement: state.prompt.trim(),
        idempotencyKey,
        ...(harnessOverride.harness ? { harness: harnessOverride.harness } : {}),
        ...(harnessOverride.model ? { model: harnessOverride.model } : {}),
        ...(harnessOverride.modelRouter !== null
          ? { modelRouter: harnessOverride.modelRouter }
          : {}),
        ...(harnessOverride.effort ? { effort: harnessOverride.effort } : {}),
        ...(harnessOverride.mode ? { harnessMode: harnessOverride.mode } : {}),
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
      className="section-sheet flex-1 overflow-y-auto px-5 py-12 sm:px-10 sm:py-16"
      aria-labelledby="new-spec-title"
      data-testid="new-spec"
    >
      <div className="mx-auto flex w-full max-w-[680px] flex-col gap-7">
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
            <RadioGroup
              className="grid-cols-1 sm:grid-cols-3"
              value={templateId}
              onValueChange={setTemplateId}
              aria-label="Shape"
            >
              {templateList.map((template) => {
                const selected = template.id === templateId;
                return (
                  // The stock RadioGroupItem hard-codes its dot as JSX
                  // children, which would discard this card. The primitive
                  // gives the same roving focus and arrow keys and takes
                  // children.
                  <RadioGroupPrimitive.Item
                    key={template.id}
                    value={template.id}
                    data-selected={selected || undefined}
                    className="relative block aspect-auto size-auto min-h-32 rounded-lg border bg-card p-4 text-left shadow-xs transition-[border-color,background-color,box-shadow] outline-none hover:bg-accent/40 focus-visible:border-ring focus-visible:ring-2 focus-visible:ring-ring/60 data-[selected=true]:border-ring data-[selected=true]:bg-accent/40"
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
                  </RadioGroupPrimitive.Item>
                );
              })}
            </RadioGroup>
          )}
        </fieldset>

        <TaskComposer
          submitLabel="Start"
          pendingLabel="Starting…"
          pending={create.isPending}
          disabled={!templateId}
          quiet
          ariaLabel="What is the spec about?"
          placeholder="Per-org caps on sandbox creation, plus a meter we can bill against."
          onStateChange={setComposerState}
          onSubmit={submit}
        />

        {create.error ? (
          <Text role="alert" tone="destructive">
            The spec did not start. {create.error.message}
          </Text>
        ) : null}

        <Text variant="code" tone="muted" className="text-xs">
          Recon takes about 40 seconds
        </Text>
      </div>
    </main>
  );
}
