import { and, asc, eq, gt, lte, sql } from "drizzle-orm";
import { semanticSpecNodeJson, type SectionState } from "@engrams/spec-document";
import * as Y from "yjs";

import { getDb } from "../db/client.ts";
import {
  spec,
  specOpenQuestion,
  specParticipant,
  specSectionState,
  specSnapshot,
  specUpdateLog,
  user,
} from "../db/schema.ts";
import { proseMirrorDocument } from "./doc-service.ts";

/** Leave more than one KiB between the digest and the SDK's 8 KiB cap. */
export const SPEC_DIGEST_MAX_BYTES = 7_000;

export interface SpecHumanChange {
  sectionId: string;
  sectionTitle: string;
  author: string;
}

export interface SpecDigestSection {
  sectionId: string;
  sectionTitle: string;
  state: SectionState;
  stateChangedAt: Date | null;
  settledBy: string | null;
  openQuestionCount: number;
}

export interface SpecDigestSnapshot {
  phase: "ideation" | "drafting" | "published";
  sections: SpecDigestSection[];
  changes: SpecHumanChange[];
}

export interface SpecDigestSource {
  read(
    specId: string,
    afterSeq: bigint,
    throughSeq: bigint,
    baseState: Uint8Array | null,
  ): Promise<SpecDigestSnapshot>;
}

interface SectionFingerprint {
  id: string;
  title: string;
  value: string;
}

function sectionFingerprints(doc: Y.Doc): Map<string, SectionFingerprint> {
  const result = new Map<string, SectionFingerprint>();
  const prosemirror = proseMirrorDocument(doc);
  prosemirror.forEach((section) => {
    const id = String(section.attrs.id);
    const heading = section.firstChild?.textContent.trim() || id;
    result.set(id, { id, title: heading, value: JSON.stringify(semanticSpecNodeJson(section)) });
  });
  return result;
}

/** Read the current digest context and the Yjs deltas since the last projection. */
export class PostgresSpecDigestSource implements SpecDigestSource {
  async read(
    specId: string,
    afterSeq: bigint,
    throughSeq: bigint,
    baseState: Uint8Array | null,
  ): Promise<SpecDigestSnapshot> {
    const db = getDb();
    const doc = new Y.Doc();
    let replayFrom = afterSeq;
    if (baseState) {
      Y.applyUpdate(doc, baseState);
    } else {
      const snapshots = await db
        .select({ state: specSnapshot.state, coveredSeq: specSnapshot.coveredSeq })
        .from(specSnapshot)
        .where(
          and(
            eq(specSnapshot.specId, specId),
            lte(specSnapshot.coveredSeq, afterSeq),
          ),
        )
        .limit(1);
      if (snapshots[0]) {
        Y.applyUpdate(doc, snapshots[0].state);
        replayFrom = snapshots[0].coveredSeq;
      } else {
        replayFrom = 0n;
      }
    }

    const rows = await db
      .select({
        seq: specUpdateLog.seq,
        update: specUpdateLog.update,
        author: user.name,
      })
      .from(specUpdateLog)
      .leftJoin(
        specParticipant,
        and(
          eq(specParticipant.specId, specUpdateLog.specId),
          eq(specParticipant.clientId, specUpdateLog.clientId),
        ),
      )
      .leftJoin(user, eq(user.id, specParticipant.userId))
      .where(
        and(
          eq(specUpdateLog.specId, specId),
          gt(specUpdateLog.seq, replayFrom),
          lte(specUpdateLog.seq, throughSeq),
        ),
      )
      .orderBy(asc(specUpdateLog.seq));

    const changes: SpecHumanChange[] = [];
    for (const row of rows) {
      const before = doc.getXmlFragment("prosemirror").length > 0
        ? sectionFingerprints(doc)
        : new Map<string, SectionFingerprint>();
      Y.applyUpdate(doc, row.update);
      if (row.seq <= afterSeq || row.author == null) continue;
      const after = sectionFingerprints(doc);
      for (const section of after.values()) {
        if (before.get(section.id)?.value !== section.value) {
          changes.push({
            sectionId: section.id,
            sectionTitle: section.title,
            author: row.author,
          });
        }
      }
    }

    const [specRows, stateRows, questionRows] = await Promise.all([
      db.select({ phase: spec.phase }).from(spec).where(eq(spec.id, specId)).limit(1),
      db
        .select({
          sectionId: specSectionState.sectionId,
          state: specSectionState.state,
          stateChangedAt: specSectionState.updatedAt,
          settledBy: user.name,
        })
        .from(specSectionState)
        .leftJoin(user, eq(user.id, specSectionState.settledBy))
        .where(eq(specSectionState.specId, specId)),
      db
        .select({
          sectionId: specOpenQuestion.sectionId,
          count: sql<number>`count(*)::int`,
        })
        .from(specOpenQuestion)
        .where(and(eq(specOpenQuestion.specId, specId), eq(specOpenQuestion.state, "open")))
        .groupBy(specOpenQuestion.sectionId),
    ]);
    const phase = specRows[0]?.phase;
    if (phase !== "ideation" && phase !== "drafting" && phase !== "published") {
      throw new Error(`Spec ${specId} has an invalid phase: ${String(phase)}`);
    }
    const states = new Map(
      stateRows.map((row) => [
        row.sectionId,
        {
          state: sectionState(specId, row.sectionId, row.state),
          stateChangedAt: row.stateChangedAt,
          settledBy: row.settledBy,
        },
      ]),
    );
    const questionCounts = new Map(
      questionRows.map((row) => [row.sectionId, Number(row.count)]),
    );
    const sections = [...sectionFingerprints(doc).values()].map((section) => {
      const status = states.get(section.id);
      return {
        sectionId: section.id,
        sectionTitle: section.title,
        state: status?.state ?? "open",
        stateChangedAt: status?.stateChangedAt ?? null,
        settledBy: status?.state === "settled" ? (status.settledBy ?? "Unknown member") : null,
        openQuestionCount: questionCounts.get(section.id) ?? 0,
      };
    });
    return { phase, sections, changes };
  }
}

export class SpecDigestService {
  constructor(private readonly source: SpecDigestSource) {}

  async render(
    specId: string,
    afterSeq: bigint,
    throughSeq: bigint,
    discardNotice: boolean,
    baseState: Uint8Array | null = null,
    peopleHere: readonly string[] = [],
  ): Promise<string> {
    const snapshot = await this.source.read(specId, afterSeq, throughSeq, baseState);
    const changes = new Map<string, { title: string; authors: Set<string> }>();
    for (const change of snapshot.changes) {
      const section = changes.get(change.sectionId) ?? {
        title: change.sectionTitle,
        authors: new Set<string>(),
      };
      section.authors.add(change.author);
      changes.set(change.sectionId, section);
    }

    const digest = new DigestBudget(SPEC_DIGEST_MAX_BYTES);
    digest.appendRequired("# Current spec context\n\n");
    digest.appendRequired(`Phase: ${snapshot.phase}.\n\n`);
    digest.appendRequired("## Section status\n");
    const prioritizedSections = snapshot.sections
      .map((section, index) => ({ section, index }))
      .sort((left, right) => {
        const priority = statusPriority(left.section.state) - statusPriority(right.section.state);
        return priority === 0 ? left.index - right.index : priority;
      });
    appendList(
      digest,
      prioritizedSections.map(({ section }) => sectionStatusLine(section)),
      "- Additional section statuses were omitted to fit the digest budget.\n",
    );
    digest.append("\n");

    if (discardNotice) {
      digest.append(
        "Your direct edit to /workspace/spec.md was discarded. Use spec_update_section for document changes.\n\n",
      );
    }

    digest.append("## Changes since your last turn\n");
    if (changes.size === 0) {
      digest.append("No human changes were recorded.\n");
    } else {
      const blocks = [...changes.values()].map((section) => {
        const lines = [`### ${label(section.title, 160)}\n`];
        for (const author of [...section.authors].sort()) {
          lines.push(`- ${label(author, 100)} updated this section.\n`);
        }
        return lines.join("");
      });
      appendList(
        digest,
        blocks,
        "Changes in additional sections were omitted to fit the digest budget.\n",
      );
    }

    const present = [...new Set(peopleHere.map((name) => label(name, 100)))].sort();
    if (present.length > 0) {
      digest.append(`\n## People here now\n${present.join(", ")}.\n`);
    }

    const questionLines = snapshot.sections.map(
      (section) =>
        `- ${label(section.sectionTitle, 160)}: ${section.openQuestionCount} open question${section.openQuestionCount === 1 ? "" : "s"}.\n`,
    );
    if (questionLines.length > 0 && digest.append("\n## Open questions by section\n")) {
      appendList(
        digest,
        questionLines,
        "- Additional question counts were omitted to fit the digest budget.\n",
      );
    }
    return digest.render();
  }
}

function sectionState(specId: string, sectionId: string, value: string): SectionState {
  if (value === "open" || value === "proposed" || value === "settled" || value === "n/a") {
    return value;
  }
  throw new Error(`Spec ${specId} section ${sectionId} has an invalid state: ${value}`);
}

function statusPriority(state: SectionState): number {
  if (state === "settled") return 0;
  if (state === "proposed") return 1;
  if (state === "n/a") return 2;
  return 3;
}

function sectionStatusLine(section: SpecDigestSection): string {
  const title = label(section.sectionTitle, 160);
  const changed = section.stateChangedAt?.toISOString();
  if (section.state === "settled") {
    const person = label(section.settledBy ?? "Unknown member", 100);
    return `- ${title}: settled by ${person}${changed ? ` at ${changed}` : " at an unknown time"}.\n`;
  }
  return `- ${title}: ${section.state}${changed ? ` since ${changed}` : " (state-change time not recorded)"}.\n`;
}

function appendList(digest: DigestBudget, values: readonly string[], omission: string): void {
  for (const value of values) {
    if (digest.append(value)) continue;
    digest.append(omission);
    return;
  }
}

function label(value: string, maxBytes: number): string {
  return truncateUtf8(value.replace(/\s+/g, " ").trim(), maxBytes);
}

function truncateUtf8(value: string, maxBytes: number): string {
  const encoder = new TextEncoder();
  if (encoder.encode(value).byteLength <= maxBytes) return value;
  const suffix = "…";
  const suffixBytes = encoder.encode(suffix).byteLength;
  let result = "";
  for (const character of value) {
    if (encoder.encode(result + character).byteLength > maxBytes - suffixBytes) break;
    result += character;
  }
  return result + suffix;
}

class DigestBudget {
  private readonly encoder = new TextEncoder();
  private readonly chunks: string[] = [];
  private bytes = 0;

  constructor(private readonly maxBytes: number) {}

  append(value: string): boolean {
    const bytes = this.encoder.encode(value).byteLength;
    if (this.bytes + bytes + 1 > this.maxBytes) return false;
    this.chunks.push(value);
    this.bytes += bytes;
    return true;
  }

  appendRequired(value: string): void {
    if (!this.append(value)) throw new Error("The required spec digest prefix exceeds its budget");
  }

  render(): string {
    return `${this.chunks.join("").trimEnd()}\n`;
  }
}
