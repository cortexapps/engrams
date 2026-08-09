import { describe, expect, test } from "bun:test";

import {
  extractRequirementDefinitions,
  extractRequirementReferences,
  RequirementIntegrityError,
  RequirementLedger,
  validateRequirementEdit,
} from "./requirements.ts";
import { schema } from "./schema.ts";

describe("requirement extraction", () => {
  test("extracts functional and non-functional citations in document order", () => {
    expect(
      extractRequirementReferences({
        textContent: "R2 is implemented by the queue. N1 and R2 constrain its delivery.",
      }),
    ).toEqual([
      { id: "R2", offset: 0 },
      { id: "N1", offset: 32 },
      { id: "R2", offset: 39 },
    ]);
  });

  test("does not accept zero, leading-zero, or embedded IDs", () => {
    expect(extractRequirementReferences("R0 R01 XR1 R1_ok N2")).toEqual([{ id: "N2", offset: 17 }]);
  });

  test("extracts definitions and tombstones", () => {
    expect(
      extractRequirementDefinitions("- R1: Create a spec.\n- R2: [removed]\nN1 — Sync in 1 s."),
    ).toEqual([
      { id: "R1", kind: "functional", text: "Create a spec.", tombstone: false },
      { id: "R2", kind: "functional", text: null, tombstone: true },
      { id: "N1", kind: "non-functional", text: "Sync in 1 s.", tombstone: false },
    ]);
  });

  test("rejects duplicate definitions", () => {
    expect(() => extractRequirementDefinitions("R1: One.\nR1: Two.")).toThrow(
      "defined more than once",
    );
  });

  test("keeps ProseMirror block boundaries between definitions", () => {
    const doc = schema.nodes.doc!.create(null, [
      schema.nodes.section!.create({ id: "requirements", templateSectionKey: "requirements" }, [
        schema.nodes.sectionHeading!.create(null, schema.text("Requirements")),
        schema.nodes.paragraph!.create(null, schema.text("R1: First.")),
        schema.nodes.paragraph!.create(null, schema.text("R2: Second.")),
      ]),
    ]);

    expect(extractRequirementDefinitions(doc)).toEqual([
      { id: "R1", kind: "functional", text: "First.", tombstone: false },
      { id: "R2", kind: "functional", text: "Second.", tombstone: false },
    ]);
  });
});

describe("stable requirement ledger", () => {
  test("deleting an earlier requirement leaves a tombstone and does not renumber", () => {
    const ledger = RequirementLedger.fromText("R1: First.\nR2: Second.");

    ledger.remove("R1");

    expect(ledger.entries()).toEqual([
      { id: "R1", kind: "functional", text: null, tombstone: true },
      { id: "R2", kind: "functional", text: "Second.", tombstone: false },
    ]);
    expect(ledger.render()).toBe("- R1: [removed]\n- R2: Second.");
  });

  test("new IDs advance past tombstones", () => {
    const ledger = RequirementLedger.fromText("R1: [removed]\nR2: Existing.\nN1: Fast.");

    expect(ledger.add("functional", "New behavior.").id).toBe("R3");
    expect(ledger.add("non-functional", "Durable.").id).toBe("N2");
  });

  test("new IDs stay exact above the JavaScript safe-integer limit", () => {
    const ledger = RequirementLedger.fromText("R9007199254740993: Existing.");
    expect(ledger.add("functional", "New behavior.").id).toBe("R9007199254740994");
  });

  test("updates retain an existing ID", () => {
    const ledger = RequirementLedger.fromText("R7: Old wording.");
    expect(ledger.update("R7", "New wording.")).toEqual({
      id: "R7",
      kind: "functional",
      text: "New wording.",
      tombstone: false,
    });
  });

  test("does not revive a tombstoned ID", () => {
    const ledger = RequirementLedger.fromText("R1: [removed]");
    expect(() => ledger.update("R1", "Restored wording.")).toThrow("cannot be restored");
  });
});

describe("Requirements-section edit validation", () => {
  test("accepts permanent tombstones and the next monotonic IDs", () => {
    expect(
      validateRequirementEdit(
        "R1: First.\nR2: Second.\nN1: Fast.",
        "R1: [removed]\nR2: Updated.\nR3: Third.\nN1: Fast.\nN2: Durable.",
      ),
    ).toHaveLength(5);
  });

  test("rejects a missing prior ID", () => {
    expect(() => validateRequirementEdit("R1: First.\nR2: Second.", "R2: Second.")).toThrow(
      RequirementIntegrityError,
    );
  });

  test("rejects a revived tombstone", () => {
    expect(() => validateRequirementEdit("R1: [removed]", "R1: Restored.")).toThrow(
      "cannot be restored",
    );
  });

  test("rejects skipped or reused new IDs", () => {
    expect(() => validateRequirementEdit("R1: First.", "R1: First.\nR3: Third.")).toThrow(
      "must be R2",
    );
  });
});
