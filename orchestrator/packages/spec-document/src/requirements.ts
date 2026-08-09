export type RequirementKind = "functional" | "non-functional";
export type RequirementPrefix = "R" | "N";
export type RequirementId = `${RequirementPrefix}${number}`;

export interface RequirementDefinition {
  id: RequirementId;
  kind: RequirementKind;
  text: string | null;
  tombstone: boolean;
}

export interface RequirementReference {
  id: RequirementId;
  offset: number;
}

export interface RequirementDocumentLike {
  readonly textContent: string;
  readonly content?: { readonly size: number };
  textBetween?(from: number, to: number, blockSeparator?: string): string;
}

const REFERENCE_PATTERN = /\b([RN])([1-9]\d*)\b/g;
const DEFINITION_PATTERN = /^\s*(?:[-*+]\s+)?([RN])([1-9]\d*)\s*(?:[.:\-—]\s*|\s+)(.*?)\s*$/;
const TOMBSTONE_TEXT = "[removed]";

function prefixFor(kind: RequirementKind): RequirementPrefix {
  return kind === "functional" ? "R" : "N";
}

function kindFor(prefix: RequirementPrefix): RequirementKind {
  return prefix === "R" ? "functional" : "non-functional";
}

function requirementId(prefix: string, numeric: string): RequirementId {
  return `${prefix}${numeric}` as RequirementId;
}

function documentText(input: string | RequirementDocumentLike): string {
  if (typeof input === "string") return input;
  if (input.textBetween && input.content) {
    return input.textBetween(0, input.content.size, "\n");
  }
  return input.textContent;
}

/** Return every requirement citation in document order, including repeated citations. */
export function extractRequirementReferences(
  input: string | RequirementDocumentLike,
): RequirementReference[] {
  const text = documentText(input);
  const references: RequirementReference[] = [];
  for (const match of text.matchAll(REFERENCE_PATTERN)) {
    references.push({
      id: requirementId(match[1]!, match[2]!),
      offset: match.index,
    });
  }
  return references;
}

/** Read requirement definitions from line-oriented Requirements section text. */
export function extractRequirementDefinitions(
  input: string | RequirementDocumentLike,
): RequirementDefinition[] {
  const text = documentText(input);
  const definitions: RequirementDefinition[] = [];
  const seen = new Set<RequirementId>();
  for (const line of text.split(/\r?\n/)) {
    const match = DEFINITION_PATTERN.exec(line);
    if (!match) continue;
    const prefix = match[1]! as RequirementPrefix;
    const id = requirementId(prefix, match[2]!);
    if (seen.has(id)) throw new Error(`Requirement ${id} is defined more than once.`);
    seen.add(id);
    const text = match[3]!.trim();
    const tombstone = text.toLowerCase() === TOMBSTONE_TEXT;
    if (text.length === 0) throw new Error(`Requirement ${id} has no text.`);
    definitions.push({
      id,
      kind: kindFor(prefix),
      text: tombstone ? null : text,
      tombstone,
    });
  }
  return definitions;
}

function validateLedger(entries: readonly RequirementDefinition[]): void {
  const seen = new Set<RequirementId>();
  for (const entry of entries) {
    if (!/^[RN][1-9]\d*$/.test(entry.id)) {
      throw new Error(`Requirement ${entry.id} has an invalid identifier.`);
    }
    if (seen.has(entry.id)) throw new Error(`Requirement ${entry.id} is defined more than once.`);
    seen.add(entry.id);
    if (entry.id[0] !== prefixFor(entry.kind)) {
      throw new Error(`Requirement ${entry.id} has the wrong kind.`);
    }
    if (entry.tombstone !== (entry.text == null)) {
      throw new Error(`Requirement ${entry.id} has an invalid tombstone.`);
    }
    if (entry.text != null && entry.text.trim().length === 0) {
      throw new Error(`Requirement ${entry.id} has no text.`);
    }
  }
}

/**
 * A ledger for stable requirement identifiers. Callers persist the
 * returned entries in the Requirements section. The ledger never deletes an ID.
 */
export class RequirementLedger {
  readonly #entries: RequirementDefinition[];

  constructor(entries: readonly RequirementDefinition[] = []) {
    validateLedger(entries);
    this.#entries = entries.map((entry) => ({ ...entry, text: entry.text?.trim() ?? null }));
  }

  static fromText(input: string | RequirementDocumentLike): RequirementLedger {
    return new RequirementLedger(extractRequirementDefinitions(input));
  }

  entries(): readonly RequirementDefinition[] {
    return this.#entries.map((entry) => ({ ...entry }));
  }

  add(kind: RequirementKind, text: string): RequirementDefinition {
    const normalized = text.trim();
    if (normalized.length === 0) throw new Error("A requirement must have text.");
    const prefix = prefixFor(kind);
    const highest = this.#entries.reduce((max, entry) => {
      if (!entry.id.startsWith(prefix)) return max;
      const value = BigInt(entry.id.slice(1));
      return value > max ? value : max;
    }, 0n);
    const entry: RequirementDefinition = {
      id: requirementId(prefix, String(highest + 1n)),
      kind,
      text: normalized,
      tombstone: false,
    };
    this.#entries.push(entry);
    return { ...entry };
  }

  update(id: RequirementId, text: string): RequirementDefinition {
    const normalized = text.trim();
    if (normalized.length === 0) throw new Error("A requirement must have text.");
    const entry = this.#requiredEntry(id);
    if (entry.tombstone) throw new Error(`Requirement ${id} was removed and cannot be restored.`);
    entry.text = normalized;
    entry.tombstone = false;
    return { ...entry };
  }

  remove(id: RequirementId): RequirementDefinition {
    const entry = this.#requiredEntry(id);
    entry.text = null;
    entry.tombstone = true;
    return { ...entry };
  }

  render(): string {
    return this.#entries
      .map((entry) => `- ${entry.id}: ${entry.tombstone ? TOMBSTONE_TEXT : entry.text}`)
      .join("\n");
  }

  #requiredEntry(id: RequirementId): RequirementDefinition {
    const entry = this.#entries.find((candidate) => candidate.id === id);
    if (!entry) throw new Error(`Requirement ${id} does not exist.`);
    return entry;
  }
}

export class RequirementIntegrityError extends Error {
  constructor(
    readonly code: "missing_id" | "revived_tombstone" | "non_monotonic_id",
    message: string,
  ) {
    super(message);
    this.name = "RequirementIntegrityError";
  }
}

/** Validate a complete Requirements-section edit before it reaches Yjs. */
export function validateRequirementEdit(
  before: string | RequirementDocumentLike,
  after: string | RequirementDocumentLike,
): readonly RequirementDefinition[] {
  const prior = extractRequirementDefinitions(before);
  const next = extractRequirementDefinitions(after);
  const nextById = new Map(next.map((entry) => [entry.id, entry]));
  for (const entry of prior) {
    const replacement = nextById.get(entry.id);
    if (!replacement) {
      throw new RequirementIntegrityError(
        "missing_id",
        `Requirement ${entry.id} must remain as content or a tombstone.`,
      );
    }
    if (entry.tombstone && !replacement.tombstone) {
      throw new RequirementIntegrityError(
        "revived_tombstone",
        `Requirement ${entry.id} was removed and cannot be restored.`,
      );
    }
  }

  const priorIds = new Set(prior.map((entry) => entry.id));
  for (const prefix of ["R", "N"] as const) {
    let expected =
      prior.reduce((highest, entry) => {
        if (!entry.id.startsWith(prefix)) return highest;
        const value = BigInt(entry.id.slice(1));
        return value > highest ? value : highest;
      }, 0n) + 1n;
    for (const entry of next) {
      if (!entry.id.startsWith(prefix) || priorIds.has(entry.id)) continue;
      if (BigInt(entry.id.slice(1)) !== expected) {
        throw new RequirementIntegrityError(
          "non_monotonic_id",
          `The next ${prefix} requirement must be ${prefix}${expected}.`,
        );
      }
      expected += 1n;
    }
  }
  return next;
}
