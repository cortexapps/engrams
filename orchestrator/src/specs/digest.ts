import { and, asc, eq, gt, lte } from "drizzle-orm";
import * as Y from "yjs";

import { getDb } from "../db/client.ts";
import { specParticipant, specSnapshot, specUpdateLog, user } from "../db/schema.ts";
import { proseMirrorDocument } from "./doc-service.ts";

export interface SpecHumanChange {
  sectionId: string;
  sectionTitle: string;
  author: string;
}

export interface SpecDigestSource {
  changes(
    specId: string,
    afterSeq: bigint,
    throughSeq: bigint,
    baseState: Uint8Array | null,
  ): Promise<SpecHumanChange[]>;
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
    result.set(id, { id, title: heading, value: JSON.stringify(section.toJSON()) });
  });
  return result;
}

/** Read Yjs deltas and name the human-authored sections they changed. */
export class PostgresSpecDigestSource implements SpecDigestSource {
  async changes(
    specId: string,
    afterSeq: bigint,
    throughSeq: bigint,
    baseState: Uint8Array | null,
  ): Promise<SpecHumanChange[]> {
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
    return changes;
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
  ): Promise<string> {
    const changes = await this.source.changes(specId, afterSeq, throughSeq, baseState);
    const sections = new Map<string, { title: string; authors: Set<string> }>();
    for (const change of changes) {
      const section = sections.get(change.sectionId) ?? {
        title: change.sectionTitle,
        authors: new Set<string>(),
      };
      section.authors.add(change.author);
      sections.set(change.sectionId, section);
    }

    const lines = ["# Spec changes since your last turn", ""];
    if (discardNotice) {
      lines.push(
        "Your direct edit to /workspace/spec.md was discarded. Use spec_update_section for document changes.",
        "",
      );
    }
    if (sections.size === 0) {
      lines.push("No human changes were recorded.");
    } else {
      for (const section of sections.values()) {
        lines.push(`## ${section.title}`);
        for (const author of [...section.authors].sort()) {
          lines.push(`- ${author} updated this section.`);
        }
        lines.push("");
      }
    }
    return `${lines.join("\n").trimEnd()}\n`;
  }
}
