/** Organization catalog for spec templates (ADR 0114 D3). */

import { and, asc, eq, isNull, or } from "drizzle-orm";
import type { NodePgDatabase } from "drizzle-orm/node-postgres";

import { getDb } from "../db/client.ts";
import { specTemplate, type SpecTemplateLayer, type SpecTemplateSection } from "../db/schema.ts";
import * as schema from "../db/schema.ts";

export const ENGINEERING_DESIGN_TEMPLATE_ID = "00000000-0000-4000-8000-000000000115";

export interface SpecTemplateDefinition {
  name: string;
  description: string;
  layers: SpecTemplateLayer[];
  sections: SpecTemplateSection[];
}

export interface SpecTemplateCatalogItem extends SpecTemplateDefinition {
  id: string;
  builtIn: boolean;
  modifiedFromDefault: boolean;
  createdAt: string;
  updatedAt: string;
}

export interface SpecTemplateSnapshot {
  templateId: string;
  layers: SpecTemplateLayer[];
  sections: SpecTemplateSection[];
}

export const ENGINEERING_DESIGN_TEMPLATE: SpecTemplateDefinition = {
  name: "Engineering design doc",
  description: "A three-layer design document that moves from intent to system detail.",
  layers: [
    {
      key: "intent",
      title: "Intent",
      description: "Define the problem, the desired outcome, and the constraints.",
    },
    {
      key: "contract",
      title: "Contract",
      description: "Define observable behavior and the public contract.",
    },
    {
      key: "system",
      title: "System",
      description: "Define the implementation, its boundaries, and its release plan.",
    },
  ],
  sections: [
    section(
      "problem",
      "Problem",
      "intent",
      "State the user or system problem and explain why it is important now.",
      ["The affected user or system is clear.", "The current failure or limitation is measurable."],
      true,
    ),
    section(
      "goals",
      "Goals",
      "intent",
      "State the outcomes that this design must produce and the outcomes that it does not cover.",
      ["Goals describe outcomes.", "Non-goals make the scope boundary clear."],
      true,
    ),
    section(
      "requirements",
      "Requirements",
      "intent",
      "List the functional and quality requirements that constrain the design.",
      ["Each requirement is testable.", "Quality requirements have explicit limits."],
      true,
    ),
    section(
      "behavior",
      "Behavior",
      "contract",
      "Describe the user-visible and system-visible behavior, including important state changes.",
      ["The main flow is complete.", "Important error and recovery flows are present."],
      true,
    ),
    section(
      "api",
      "API",
      "contract",
      "Define the interfaces that callers use, including request, response, and compatibility rules.",
      ["Inputs and outputs are explicit.", "Errors and compatibility rules are explicit."],
      false,
      true,
    ),
    section(
      "alternatives",
      "Alternatives",
      "system",
      "Compare the serious alternatives and state why the selected direction is better.",
      ["At least one credible alternative is assessed.", "Trade-offs are explicit."],
      true,
    ),
    section(
      "design",
      "Design",
      "system",
      "Explain the selected design, its main components, and the important control flow.",
      ["Responsibilities and control flow are clear.", "The design satisfies the requirements."],
      true,
    ),
    section(
      "data-model",
      "Data model",
      "system",
      "Define stored entities, ownership, lifecycle, and consistency rules.",
      ["Stored fields and relationships are clear.", "Lifecycle and consistency rules are clear."],
      false,
      true,
    ),
    section(
      "interfaces",
      "Interfaces",
      "system",
      "Define internal boundaries, dependencies, and the data that crosses each boundary.",
      ["Each boundary has an owner.", "Data and failure behavior are defined."],
      false,
      true,
    ),
    section(
      "failure-modes",
      "Failure modes",
      "system",
      "Describe expected failures, detection, recovery, and data safety.",
      ["Important failures have detection and recovery paths.", "Durability risks are explicit."],
      true,
    ),
    section(
      "rollout",
      "Rollout",
      "system",
      "Describe deployment, migration, observation, and rollback.",
      ["The rollout has verification points.", "Rollback or forward recovery is defined."],
      true,
    ),
  ],
};

function section(
  key: string,
  title: string,
  layerKey: string,
  guidance: string,
  doneCriteria: string[],
  required: boolean,
  allowNa = false,
): SpecTemplateSection {
  return { key, title, layerKey, guidance, doneCriteria, required, allowNa };
}

export interface SpecTemplateRow extends SpecTemplateDefinition {
  id: string;
  orgId: string | null;
  createdAt: Date;
  updatedAt: Date;
}

export interface SpecTemplateStore {
  listVisible(orgId: string): Promise<SpecTemplateRow[]>;
  getVisible(id: string, orgId: string): Promise<SpecTemplateRow | null>;
  insert(row: SpecTemplateRow): Promise<SpecTemplateRow>;
  replace(
    id: string,
    orgId: string,
    definition: SpecTemplateDefinition,
    updatedAt: Date,
  ): Promise<SpecTemplateRow | null>;
}

export class SpecTemplateValidationError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "SpecTemplateValidationError";
  }
}

export class SpecTemplateCatalog {
  constructor(
    private readonly store: SpecTemplateStore,
    private readonly newId: () => string = () => crypto.randomUUID(),
    private readonly now: () => Date = () => new Date(),
  ) {}

  async list(orgId: string): Promise<SpecTemplateCatalogItem[]> {
    return (await this.store.listVisible(orgId)).map(catalogItem);
  }

  async create(orgId: string, input: unknown): Promise<SpecTemplateCatalogItem> {
    const definition = validateSpecTemplate(input);
    const createdAt = this.now();
    return catalogItem(
      await this.store.insert({
        id: this.newId(),
        orgId,
        ...definition,
        createdAt,
        updatedAt: createdAt,
      }),
    );
  }

  async clone(orgId: string, id: string): Promise<SpecTemplateCatalogItem | null> {
    const source = await this.store.getVisible(id, orgId);
    if (!source) return null;
    return this.create(orgId, {
      ...definitionOf(source),
      name: `${source.name} copy`,
    });
  }

  async update(orgId: string, id: string, input: unknown): Promise<SpecTemplateCatalogItem | null> {
    const definition = validateSpecTemplate(input);
    const updated = await this.store.replace(id, orgId, definition, this.now());
    return updated ? catalogItem(updated) : null;
  }

  async restoreDefault(orgId: string, id: string): Promise<SpecTemplateCatalogItem | null> {
    const current = await this.store.getVisible(id, orgId);
    if (!current || current.id !== ENGINEERING_DESIGN_TEMPLATE_ID || current.orgId !== null) {
      return null;
    }
    const restored = await this.store.replace(
      id,
      orgId,
      cloneDefinition(ENGINEERING_DESIGN_TEMPLATE),
      this.now(),
    );
    return restored ? catalogItem(restored) : null;
  }

  /** Capture the definition that a new spec owns for its complete lifetime. */
  async snapshotForNewSpec(orgId: string, id: string): Promise<SpecTemplateSnapshot | null> {
    const template = await this.store.getVisible(id, orgId);
    if (!template) return null;
    return {
      templateId: template.id,
      layers: structuredClone(template.layers),
      sections: structuredClone(template.sections),
    };
  }
}

export function makeSpecTemplateCatalog(
  db: NodePgDatabase<typeof schema> = getDb(),
): SpecTemplateCatalog {
  return new SpecTemplateCatalog(makePostgresSpecTemplateStore(db));
}

export function makePostgresSpecTemplateStore(
  db: NodePgDatabase<typeof schema>,
): SpecTemplateStore {
  return {
    async listVisible(orgId) {
      const rows = await db
        .select()
        .from(specTemplate)
        .where(or(isNull(specTemplate.orgId), eq(specTemplate.orgId, orgId)))
        .orderBy(asc(specTemplate.orgId), asc(specTemplate.name), asc(specTemplate.id));
      return rows.map(toRow);
    },
    async getVisible(id, orgId) {
      const rows = await db
        .select()
        .from(specTemplate)
        .where(
          and(
            eq(specTemplate.id, id),
            or(isNull(specTemplate.orgId), eq(specTemplate.orgId, orgId)),
          ),
        )
        .limit(1);
      return rows[0] ? toRow(rows[0]) : null;
    },
    async insert(row) {
      const rows = await db.insert(specTemplate).values(row).returning();
      return toRow(rows[0]!);
    },
    async replace(id, orgId, definition, updatedAt) {
      const rows = await db
        .update(specTemplate)
        .set({ ...definition, updatedAt })
        .where(
          and(
            eq(specTemplate.id, id),
            or(isNull(specTemplate.orgId), eq(specTemplate.orgId, orgId)),
          ),
        )
        .returning();
      return rows[0] ? toRow(rows[0]) : null;
    },
  };
}

export function validateSpecTemplate(input: unknown): SpecTemplateDefinition {
  if (!isObject(input)) throw new SpecTemplateValidationError("template must be an object");
  const name = requiredString(input["name"], "name");
  const description = optionalString(input["description"], "description");
  const rawLayers = input["layers"];
  const rawSections = input["sections"];
  if (!Array.isArray(rawLayers) || rawLayers.length === 0) {
    throw new SpecTemplateValidationError("template must contain at least one layer");
  }
  if (!Array.isArray(rawSections)) {
    throw new SpecTemplateValidationError("sections must be an array");
  }

  const layers = rawLayers.map((value, index) => validateLayer(value, index));
  const layerKeys = new Set(layers.map((layer) => layer.key));
  if (layerKeys.size !== layers.length) {
    throw new SpecTemplateValidationError("layer keys must be unique");
  }

  const sections = rawSections.map((value, index) => validateSection(value, index));
  const sectionKeys = new Set(sections.map((entry) => entry.key));
  if (sectionKeys.size !== sections.length) {
    throw new SpecTemplateValidationError("section keys must be unique");
  }
  const sectionTitles = new Set(sections.map((entry) => entry.title.toLocaleLowerCase()));
  if (sectionTitles.size !== sections.length) {
    throw new SpecTemplateValidationError("section titles must be unique");
  }
  if (sections.some((entry) => !layerKeys.has(entry.layerKey))) {
    throw new SpecTemplateValidationError("each section must reference an existing layer");
  }
  if (!sections.some((entry) => entry.required)) {
    throw new SpecTemplateValidationError("template must contain at least one required section");
  }

  return {
    name,
    description,
    layers,
    sections,
  };
}

function validateLayer(value: unknown, index: number): SpecTemplateLayer {
  if (!isObject(value)) {
    throw new SpecTemplateValidationError(`layers[${index}] must be an object`);
  }
  const description = optionalString(value["description"], `layers[${index}].description`);
  return {
    key: requiredString(value["key"], `layers[${index}].key`),
    title: requiredString(value["title"], `layers[${index}].title`),
    ...(description ? { description } : {}),
  };
}

function validateSection(value: unknown, index: number): SpecTemplateSection {
  if (!isObject(value)) {
    throw new SpecTemplateValidationError(`sections[${index}] must be an object`);
  }
  const criteria = value["doneCriteria"];
  if (!Array.isArray(criteria) || criteria.some((entry) => typeof entry !== "string")) {
    throw new SpecTemplateValidationError(`sections[${index}].doneCriteria must be text items`);
  }
  if (typeof value["required"] !== "boolean" || typeof value["allowNa"] !== "boolean") {
    throw new SpecTemplateValidationError(
      `sections[${index}].required and allowNa must be boolean values`,
    );
  }
  return {
    key: requiredString(value["key"], `sections[${index}].key`),
    title: requiredString(value["title"], `sections[${index}].title`),
    layerKey: requiredString(value["layerKey"], `sections[${index}].layerKey`),
    guidance: text(value["guidance"], `sections[${index}].guidance`),
    doneCriteria: criteria.map((entry) => entry.trim()),
    required: value["required"],
    allowNa: value["allowNa"],
  };
}

function requiredString(value: unknown, field: string): string {
  const result = text(value, field).trim();
  if (!result) throw new SpecTemplateValidationError(`${field} must not be empty`);
  return result;
}

function optionalString(value: unknown, field: string): string {
  if (value === undefined || value === null) return "";
  return text(value, field).trim();
}

function text(value: unknown, field: string): string {
  if (typeof value !== "string") throw new SpecTemplateValidationError(`${field} must be text`);
  return value;
}

function isObject(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function toRow(row: typeof specTemplate.$inferSelect): SpecTemplateRow {
  return {
    id: row.id,
    orgId: row.orgId,
    name: row.name,
    description: row.description ?? "",
    layers: row.layers,
    sections: row.sections,
    createdAt: row.createdAt,
    updatedAt: row.updatedAt,
  };
}

function definitionOf(row: SpecTemplateRow): SpecTemplateDefinition {
  return {
    name: row.name,
    description: row.description,
    layers: row.layers,
    sections: row.sections,
  };
}

function cloneDefinition(definition: SpecTemplateDefinition): SpecTemplateDefinition {
  return structuredClone(definition);
}

function catalogItem(row: SpecTemplateRow): SpecTemplateCatalogItem {
  const definition = definitionOf(row);
  return {
    id: row.id,
    ...cloneDefinition(definition),
    builtIn: row.orgId === null,
    modifiedFromDefault:
      row.id === ENGINEERING_DESIGN_TEMPLATE_ID &&
      !definitionsEqual(definition, ENGINEERING_DESIGN_TEMPLATE),
    createdAt: row.createdAt.toISOString(),
    updatedAt: row.updatedAt.toISOString(),
  };
}

function definitionsEqual(left: SpecTemplateDefinition, right: SpecTemplateDefinition): boolean {
  return canonicalJson(left) === canonicalJson(right);
}

/** Compare JSON values after Postgres jsonb has normalized object key order. */
function canonicalJson(value: unknown): string {
  if (Array.isArray(value)) return `[${value.map(canonicalJson).join(",")}]`;
  if (isObject(value)) {
    return `{${Object.keys(value)
      .sort()
      .map((key) => `${JSON.stringify(key)}:${canonicalJson(value[key])}`)
      .join(",")}}`;
  }
  return JSON.stringify(value);
}
