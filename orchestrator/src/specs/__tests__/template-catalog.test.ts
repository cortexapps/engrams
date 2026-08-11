import { describe, expect, test } from "bun:test";

import {
  ENGINEERING_DESIGN_TEMPLATE,
  ENGINEERING_DESIGN_TEMPLATE_ID,
  SpecTemplateCatalog,
  SpecTemplateValidationError,
  type SpecTemplateDefinition,
  type SpecTemplateRow,
  type SpecTemplateStore,
} from "../template-catalog.ts";

class MemoryTemplateStore implements SpecTemplateStore {
  readonly rows = new Map<string, SpecTemplateRow>();

  constructor() {
    const createdAt = new Date("2026-08-11T00:00:00.000Z");
    this.rows.set(ENGINEERING_DESIGN_TEMPLATE_ID, {
      id: ENGINEERING_DESIGN_TEMPLATE_ID,
      orgId: null,
      ...structuredClone(ENGINEERING_DESIGN_TEMPLATE),
      createdAt,
      updatedAt: createdAt,
    });
  }

  async listVisible(orgId: string) {
    return [...this.rows.values()].filter((row) => row.orgId === null || row.orgId === orgId);
  }

  async getVisible(id: string, orgId: string) {
    const row = this.rows.get(id);
    return row && (row.orgId === null || row.orgId === orgId) ? structuredClone(row) : null;
  }

  async insert(row: SpecTemplateRow) {
    this.rows.set(row.id, structuredClone(row));
    return structuredClone(row);
  }

  async replace(id: string, orgId: string, definition: SpecTemplateDefinition, updatedAt: Date) {
    const row = this.rows.get(id);
    if (!row || (row.orgId !== null && row.orgId !== orgId)) return null;
    const next = { ...row, ...structuredClone(definition), updatedAt };
    this.rows.set(id, next);
    return structuredClone(next);
  }
}

function catalog() {
  return new SpecTemplateCatalog(
    new MemoryTemplateStore(),
    () => "00000000-0000-4000-8000-000000000116",
    () => new Date("2026-08-11T01:00:00.000Z"),
  );
}

describe("spec template catalog", () => {
  test("rejects a template with no required section", async () => {
    const input = structuredClone(ENGINEERING_DESIGN_TEMPLATE);
    input.sections = input.sections.map((section) => ({ ...section, required: false }));

    await expect(catalog().create("org-1", input)).rejects.toThrow(
      new SpecTemplateValidationError("template must contain at least one required section"),
    );
  });

  test("rejects duplicate section titles without regard to case", async () => {
    const input = structuredClone(ENGINEERING_DESIGN_TEMPLATE);
    input.sections[1] = { ...input.sections[1]!, title: "problem" };

    await expect(catalog().create("org-1", input)).rejects.toThrow("section titles must be unique");
  });

  test("restores the built-in template exactly", async () => {
    const service = catalog();
    const changed = structuredClone(ENGINEERING_DESIGN_TEMPLATE);
    changed.sections[0] = { ...changed.sections[0]!, guidance: "Changed guidance" };
    const saved = await service.update("org-1", ENGINEERING_DESIGN_TEMPLATE_ID, changed);
    expect(saved?.modifiedFromDefault).toBe(true);

    const restored = await service.restoreDefault("org-1", ENGINEERING_DESIGN_TEMPLATE_ID);

    expect(restored).toMatchObject({
      ...ENGINEERING_DESIGN_TEMPLATE,
      builtIn: true,
      modifiedFromDefault: false,
    });
  });

  test("editing a template does not change a spec snapshot created from it", async () => {
    const service = catalog();
    const snapshot = await service.snapshotForNewSpec("org-1", ENGINEERING_DESIGN_TEMPLATE_ID);
    const changed = structuredClone(ENGINEERING_DESIGN_TEMPLATE);
    changed.sections[0] = { ...changed.sections[0]!, title: "Problem statement" };

    await service.update("org-1", ENGINEERING_DESIGN_TEMPLATE_ID, changed);

    expect(snapshot?.sections[0]?.title).toBe("Problem");
    expect(
      (await service.snapshotForNewSpec("org-1", ENGINEERING_DESIGN_TEMPLATE_ID))?.sections[0]
        ?.title,
    ).toBe("Problem statement");
  });
});
