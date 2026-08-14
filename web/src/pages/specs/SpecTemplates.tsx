import { ArrowDown, ArrowUp, Copy, Layers3, Plus, RotateCcw, Save, Trash2 } from "lucide-react";
import { useEffect, useMemo, useState, type ReactNode } from "react";

import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Skeleton } from "@/components/ui/skeleton";
import { Switch } from "@/components/ui/switch";
import { Textarea } from "@/components/ui/textarea";
import {
  useCloneSpecTemplate,
  useRestoreSpecTemplate,
  useSaveSpecTemplate,
  useSpecTemplates,
  type SpecTemplate,
  type SpecTemplateDefinition,
  type SpecTemplateSection,
} from "@/hooks/useSpecTemplates";
import { errorMessage } from "@/lib/errors";
import "./spec-templates.css";

const EMPTY_TEMPLATE: SpecTemplateDefinition = {
  name: "Untitled template",
  description: "",
  layers: [{ key: "layer-1", title: "First layer", description: "" }],
  sections: [
    {
      key: "section-1",
      title: "First section",
      layerKey: "layer-1",
      guidance: "",
      doneCriteria: [],
      required: true,
      allowNa: false,
    },
  ],
};

export function SpecTemplates() {
  const templates = useSpecTemplates();
  const save = useSaveSpecTemplate();
  const clone = useCloneSpecTemplate();
  const restore = useRestoreSpecTemplate();
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [draft, setDraft] = useState<SpecTemplateDefinition | null>(null);
  const [creating, setCreating] = useState(false);

  const selected = useMemo(
    () => templates.data?.find((template) => template.id === selectedId) ?? null,
    [selectedId, templates.data],
  );

  useEffect(() => {
    if (creating || selectedId || !templates.data?.[0]) return;
    setSelectedId(templates.data[0].id);
  }, [creating, selectedId, templates.data]);

  useEffect(() => {
    if (!selected || creating) return;
    setDraft(definitionOf(selected));
  }, [creating, selected]);

  const dirty = draft !== null && selected !== null && !sameDefinition(draft, selected);
  const pending = save.isPending || clone.isPending || restore.isPending;
  const mutationError = save.error ?? clone.error ?? restore.error;

  function choose(template: SpecTemplate) {
    setCreating(false);
    setSelectedId(template.id);
    setDraft(definitionOf(template));
  }

  async function saveDraft() {
    if (!draft) return;
    const saved = await save.mutateAsync({
      id: creating ? null : selectedId,
      definition: normalizeDefinitionForSave(draft),
    });
    setCreating(false);
    setSelectedId(saved.id);
    setDraft(definitionOf(saved));
  }

  async function cloneSelected() {
    if (!selectedId) return;
    choose(await clone.mutateAsync(selectedId));
  }

  async function restoreSelected() {
    if (!selectedId) return;
    choose(await restore.mutateAsync(selectedId));
  }

  return (
    <div className="spec-template-page">
      <aside className="spec-template-catalog" aria-label="Spec templates">
        <div className="spec-template-catalog-heading">
          <div>
            <h2>Templates</h2>
            <p>Reusable document structure.</p>
          </div>
          <Button
            size="sm"
            onClick={() => {
              setSelectedId(null);
              setCreating(true);
              setDraft(structuredClone(EMPTY_TEMPLATE));
            }}
          >
            <Plus aria-hidden />
            New
          </Button>
        </div>

        {templates.isPending ? (
          <div className="space-y-2 p-3" aria-label="Loading templates">
            <Skeleton className="h-20 w-full" />
            <Skeleton className="h-20 w-full" />
          </div>
        ) : templates.error ? (
          <p className="spec-template-catalog-error" role="alert">
            Could not load templates. {errorMessage(templates.error)}
          </p>
        ) : (
          <div className="spec-template-list">
            {templates.data.map((template) => (
              <button
                type="button"
                key={template.id}
                className={selectedId === template.id && !creating ? "is-selected" : ""}
                onClick={() => choose(template)}
              >
                <span className="spec-template-list-title">{template.name}</span>
                <span className="spec-template-list-meta">
                  {template.sections.length} sections · {template.layers.length} layers
                </span>
                <span className="spec-template-list-badges">
                  {template.builtIn && <Badge variant="secondary">Built-in</Badge>}
                  {template.modifiedFromDefault && <Badge variant="outline">Modified</Badge>}
                </span>
              </button>
            ))}
          </div>
        )}
      </aside>

      <main className="spec-template-editor">
        {!draft ? (
          <div className="spec-template-empty">
            <Layers3 aria-hidden />
            <p>Select a template to view its structure.</p>
          </div>
        ) : (
          <>
            <header className="spec-template-editor-heading">
              <div>
                <div className="spec-template-title-line">
                  <h2>{creating ? "New template" : draft.name}</h2>
                  {selected?.builtIn && <Badge variant="secondary">Built-in</Badge>}
                  {selected?.modifiedFromDefault && (
                    <Badge variant="outline">Modified from default</Badge>
                  )}
                  {dirty && <Badge>Unsaved</Badge>}
                </div>
                <p>
                  Template edits apply to future spec sessions only. Existing specs keep their
                  current structure.
                </p>
              </div>
              <div className="spec-template-heading-actions">
                {!creating && (
                  <Button variant="outline" onClick={() => void cloneSelected()} disabled={pending}>
                    <Copy aria-hidden />
                    Clone
                  </Button>
                )}
                {selected?.builtIn && selected.modifiedFromDefault && (
                  <Button
                    variant="outline"
                    onClick={() => void restoreSelected()}
                    disabled={pending}
                  >
                    <RotateCcw aria-hidden />
                    Restore default
                  </Button>
                )}
                <Button
                  onClick={() => void saveDraft()}
                  disabled={pending || (!creating && !dirty)}
                >
                  <Save aria-hidden />
                  {pending ? "Saving…" : "Save template"}
                </Button>
              </div>
            </header>

            {mutationError && (
              <p className="spec-template-save-error" role="alert">
                Could not save the template. {errorMessage(mutationError)}
              </p>
            )}

            <div className="spec-template-editor-body">
              <TemplateIdentity draft={draft} disabled={false} onChange={setDraft} />
              <StructureEditor draft={draft} disabled={false} onChange={setDraft} />
            </div>
          </>
        )}
      </main>
    </div>
  );
}

function TemplateIdentity({
  draft,
  disabled,
  onChange,
}: {
  draft: SpecTemplateDefinition;
  disabled: boolean;
  onChange: (draft: SpecTemplateDefinition) => void;
}) {
  return (
    <section className="spec-template-panel" aria-labelledby="template-details-title">
      <div className="spec-template-section-heading">
        <div>
          <h3 id="template-details-title">Details</h3>
          <p>Name this template and explain when a team should use it.</p>
        </div>
      </div>
      <div className="spec-template-details-grid">
        <div className="space-y-2">
          <Label htmlFor="template-name">Name</Label>
          <Input
            id="template-name"
            value={draft.name}
            disabled={disabled}
            onChange={(event) => onChange({ ...draft, name: event.target.value })}
          />
        </div>
        <div className="space-y-2">
          <Label htmlFor="template-description">Description</Label>
          <Textarea
            id="template-description"
            value={draft.description}
            disabled={disabled}
            rows={2}
            onChange={(event) => onChange({ ...draft, description: event.target.value })}
          />
        </div>
      </div>
    </section>
  );
}

function StructureEditor({
  draft,
  disabled,
  onChange,
}: {
  draft: SpecTemplateDefinition;
  disabled: boolean;
  onChange: (draft: SpecTemplateDefinition) => void;
}) {
  function updateLayer(index: number, field: "title" | "description", value: string) {
    const layers = draft.layers.map((layer, candidate) =>
      candidate === index ? { ...layer, [field]: value } : layer,
    );
    onChange({ ...draft, layers });
  }

  function moveLayer(index: number, direction: -1 | 1) {
    const target = index + direction;
    if (target < 0 || target >= draft.layers.length) return;
    const layers = [...draft.layers];
    [layers[index], layers[target]] = [layers[target]!, layers[index]!];
    onChange({ ...draft, layers });
  }

  function removeLayer(index: number) {
    const key = draft.layers[index]?.key;
    if (!key || draft.layers.length === 1) return;
    onChange({
      ...draft,
      layers: draft.layers.filter((_, candidate) => candidate !== index),
      sections: draft.sections.filter((section) => section.layerKey !== key),
    });
  }

  function addLayer() {
    const key = nextKey(
      "layer",
      draft.layers.map((layer) => layer.key),
    );
    onChange({
      ...draft,
      layers: [...draft.layers, { key, title: "New layer", description: "" }],
    });
  }

  function updateSection(key: string, patch: Partial<SpecTemplateSection>) {
    onChange({
      ...draft,
      sections: draft.sections.map((section) =>
        section.key === key ? { ...section, ...patch } : section,
      ),
    });
  }

  function addSection(layerKey: string) {
    const key = nextKey(
      "section",
      draft.sections.map((section) => section.key),
    );
    onChange({
      ...draft,
      sections: [
        ...draft.sections,
        {
          key,
          title: "New section",
          layerKey,
          guidance: "",
          doneCriteria: [],
          required: false,
          allowNa: true,
        },
      ],
    });
  }

  function moveSection(key: string, direction: -1 | 1) {
    const index = draft.sections.findIndex((section) => section.key === key);
    const section = draft.sections[index];
    if (!section) return;
    const peers = draft.sections.filter((candidate) => candidate.layerKey === section.layerKey);
    const peerIndex = peers.findIndex((candidate) => candidate.key === key);
    const targetPeer = peers[peerIndex + direction];
    if (!targetPeer) return;
    const target = draft.sections.findIndex((candidate) => candidate.key === targetPeer.key);
    const sections = [...draft.sections];
    [sections[index], sections[target]] = [sections[target]!, sections[index]!];
    onChange({ ...draft, sections });
  }

  return (
    <section className="spec-template-panel" aria-labelledby="template-structure-title">
      <div className="spec-template-section-heading">
        <div>
          <h3 id="template-structure-title">Structure</h3>
          <p>Layers and sections appear in this order in each new spec.</p>
        </div>
        {!disabled && (
          <Button variant="outline" size="sm" onClick={addLayer}>
            <Plus aria-hidden />
            Add layer
          </Button>
        )}
      </div>

      <div className="spec-template-layers">
        {draft.layers.map((layer, layerIndex) => {
          const sections = draft.sections.filter((section) => section.layerKey === layer.key);
          return (
            <article className="spec-template-layer" key={layer.key}>
              <div className="spec-template-layer-heading">
                <span className="spec-template-layer-number">{layerIndex + 1}</span>
                <div className="spec-template-layer-fields">
                  <Input
                    aria-label={`Layer ${layerIndex + 1} title`}
                    value={layer.title}
                    disabled={disabled}
                    onChange={(event) => updateLayer(layerIndex, "title", event.target.value)}
                  />
                  <Input
                    aria-label={`Layer ${layerIndex + 1} description`}
                    value={layer.description ?? ""}
                    disabled={disabled}
                    placeholder="Layer purpose"
                    onChange={(event) => updateLayer(layerIndex, "description", event.target.value)}
                  />
                </div>
                {!disabled && (
                  <div className="spec-template-order-actions">
                    <IconButton
                      label={`Move ${layer.title} up`}
                      disabled={layerIndex === 0}
                      onClick={() => moveLayer(layerIndex, -1)}
                    >
                      <ArrowUp />
                    </IconButton>
                    <IconButton
                      label={`Move ${layer.title} down`}
                      disabled={layerIndex === draft.layers.length - 1}
                      onClick={() => moveLayer(layerIndex, 1)}
                    >
                      <ArrowDown />
                    </IconButton>
                    <IconButton
                      label={`Remove ${layer.title}`}
                      disabled={draft.layers.length === 1}
                      onClick={() => removeLayer(layerIndex)}
                    >
                      <Trash2 />
                    </IconButton>
                  </div>
                )}
              </div>

              <div className="spec-template-sections">
                {sections.map((section, sectionIndex) => (
                  <SectionEditor
                    key={section.key}
                    section={section}
                    index={sectionIndex}
                    count={sections.length}
                    disabled={disabled}
                    onChange={(patch) => updateSection(section.key, patch)}
                    onMove={(direction) => moveSection(section.key, direction)}
                    onRemove={() =>
                      onChange({
                        ...draft,
                        sections: draft.sections.filter(
                          (candidate) => candidate.key !== section.key,
                        ),
                      })
                    }
                  />
                ))}
                {!disabled && (
                  <Button variant="ghost" size="sm" onClick={() => addSection(layer.key)}>
                    <Plus aria-hidden />
                    Add section
                  </Button>
                )}
              </div>
            </article>
          );
        })}
      </div>
    </section>
  );
}

function SectionEditor({
  section,
  index,
  count,
  disabled,
  onChange,
  onMove,
  onRemove,
}: {
  section: SpecTemplateSection;
  index: number;
  count: number;
  disabled: boolean;
  onChange: (patch: Partial<SpecTemplateSection>) => void;
  onMove: (direction: -1 | 1) => void;
  onRemove: () => void;
}) {
  return (
    <div className="spec-template-section-card">
      <div className="spec-template-section-card-heading">
        <Input
          aria-label={`Section ${index + 1} title`}
          value={section.title}
          disabled={disabled}
          onChange={(event) => onChange({ title: event.target.value })}
        />
        {!disabled && (
          <div className="spec-template-order-actions">
            <IconButton
              label={`Move ${section.title} up`}
              disabled={index === 0}
              onClick={() => onMove(-1)}
            >
              <ArrowUp />
            </IconButton>
            <IconButton
              label={`Move ${section.title} down`}
              disabled={index === count - 1}
              onClick={() => onMove(1)}
            >
              <ArrowDown />
            </IconButton>
            <IconButton label={`Remove ${section.title}`} onClick={onRemove}>
              <Trash2 />
            </IconButton>
          </div>
        )}
      </div>
      <div className="spec-template-section-grid">
        <div className="space-y-2">
          <Label htmlFor={`guidance-${section.key}`}>Guidance prompt</Label>
          <Textarea
            id={`guidance-${section.key}`}
            value={section.guidance}
            disabled={disabled}
            rows={3}
            onChange={(event) => onChange({ guidance: event.target.value })}
          />
        </div>
        <div className="space-y-2">
          <Label htmlFor={`criteria-${section.key}`}>Done criteria</Label>
          <Textarea
            id={`criteria-${section.key}`}
            value={section.doneCriteria.join("\n")}
            disabled={disabled}
            rows={3}
            placeholder="One criterion per line"
            onChange={(event) =>
              onChange({
                doneCriteria: event.target.value.split("\n"),
              })
            }
          />
        </div>
      </div>
      <div className="spec-template-section-toggles">
        <ToggleField
          label="Required"
          checked={section.required}
          disabled={disabled}
          onCheckedChange={(checked) => onChange({ required: checked })}
        />
        <ToggleField
          label="Allow n/a"
          checked={section.allowNa}
          disabled={disabled}
          onCheckedChange={(checked) => onChange({ allowNa: checked })}
        />
      </div>
    </div>
  );
}

function ToggleField({
  label,
  checked,
  disabled,
  onCheckedChange,
}: {
  label: string;
  checked: boolean;
  disabled: boolean;
  onCheckedChange: (checked: boolean) => void;
}) {
  return (
    <label className="spec-template-toggle">
      <Switch checked={checked} disabled={disabled} onCheckedChange={onCheckedChange} />
      <span>{label}</span>
    </label>
  );
}

function IconButton({
  label,
  disabled,
  onClick,
  children,
}: {
  label: string;
  disabled?: boolean;
  onClick: () => void;
  children: ReactNode;
}) {
  return (
    <Button
      type="button"
      variant="ghost"
      size="icon-sm"
      aria-label={label}
      disabled={disabled}
      onClick={onClick}
    >
      {children}
    </Button>
  );
}

function definitionOf(template: SpecTemplateDefinition): SpecTemplateDefinition {
  return {
    name: template.name,
    description: template.description,
    layers: structuredClone(template.layers),
    sections: structuredClone(template.sections),
  };
}

function normalizeDefinitionForSave(definition: SpecTemplateDefinition): SpecTemplateDefinition {
  return {
    ...definition,
    sections: definition.sections.map((section) => ({
      ...section,
      doneCriteria: section.doneCriteria.map((line) => line.trim()).filter(Boolean),
    })),
  };
}

function sameDefinition(left: SpecTemplateDefinition, right: SpecTemplateDefinition): boolean {
  return JSON.stringify(left) === JSON.stringify(definitionOf(right));
}

function nextKey(prefix: string, keys: string[]): string {
  const used = new Set(keys);
  let index = used.size + 1;
  while (used.has(`${prefix}-${index}`)) index += 1;
  return `${prefix}-${index}`;
}
