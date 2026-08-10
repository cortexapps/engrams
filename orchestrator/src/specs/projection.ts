import { createHash } from "node:crypto";
import { and, asc, desc, eq, gt, inArray, lt, lte, sql } from "drizzle-orm";
import * as Y from "yjs";

import { sessions } from "../control-plane/client.ts";
import { getDb } from "../db/client.ts";
import { spec, specProjection, specSnapshot, specUpdateLog } from "../db/schema.ts";
import { runExec, type DurableExecClient } from "../exec/durable-exec.ts";
import type { SessionFileClient } from "../routes/session-files.ts";
import { renderMarkdown } from "@engrams/spec-document";
import { proseMirrorDocument } from "./doc-service.ts";
import { PostgresSpecDigestSource, SpecDigestService } from "./digest.ts";

export const SPEC_PROJECTION_PATH = "/workspace/spec.md";
export const SPEC_DIGEST_PATH = "/workspace/.engrams/spec/digest.md";
const PENDING_STATES = ["rendering", "staged"] as const;
const PUBLISH_DEADLINE_MS = 30_000;

export interface SpecProjectionRequest {
  specId: string;
  sessionId: string;
  source: string;
}

export interface SpecProjection {
  request(input: SpecProjectionRequest): Promise<void>;
}

export interface ProjectionRecord {
  specId: string;
  rev: bigint;
  sessionId: string;
  docSeq: bigint;
  semanticDocSeq: bigint;
  sha256: string;
  rendered: Uint8Array;
  documentState: Uint8Array;
  digest: Uint8Array;
  digestSha256: string;
  stagingPath: string;
  state: "rendering" | "staged" | "published" | "superseded";
  requestedSource: string;
  discardNotice: boolean;
}

export interface SpecProjectionStore {
  reserve(input: SpecProjectionRequest, discardNotice?: boolean): Promise<ProjectionRecord>;
  pending(sessionId: string): Promise<ProjectionRecord[]>;
  latestPublished(specId: string): Promise<ProjectionRecord | null>;
  recordRender(
    specId: string,
    rev: bigint,
    value: {
      docSeq: bigint;
      semanticDocSeq: bigint;
      sha256: string;
      rendered: Uint8Array;
      documentState: Uint8Array;
      digest: Uint8Array;
      digestSha256: string;
    },
  ): Promise<void>;
  markStaged(specId: string, rev: bigint): Promise<void>;
  markPublished(specId: string, rev: bigint): Promise<void>;
  markSuperseded(specId: string, rev: bigint): Promise<void>;
  state(specId: string, rev: bigint): Promise<ProjectionRecord["state"] | null>;
  get(specId: string, rev: bigint): Promise<ProjectionRecord | null>;
  specForSession(sessionId: string): Promise<string | null>;
}

function projectionRecord(row: typeof specProjection.$inferSelect): ProjectionRecord {
  return {
    specId: row.specId,
    rev: row.rev,
    sessionId: row.sessionId,
    docSeq: row.docSeq,
    semanticDocSeq: row.semanticDocSeq,
    sha256: row.sha256,
    rendered: row.rendered,
    documentState: row.documentState,
    digest: row.digest,
    digestSha256: row.digestSha256,
    stagingPath: row.stagingPath,
    state: row.state as ProjectionRecord["state"],
    requestedSource: row.requestedSource,
    discardNotice: row.discardNotice,
  };
}

export class PostgresSpecProjectionStore implements SpecProjectionStore {
  constructor(private readonly now: () => Date) {}

  async reserve(input: SpecProjectionRequest, discardNotice = false): Promise<ProjectionRecord> {
    return getDb().transaction(async (tx) => {
      const specs = await tx
        .select({ docSeq: spec.currentDocSeq, semanticDocSeq: spec.currentSemanticDocSeq })
        .from(spec)
        .where(and(eq(spec.id, input.specId), eq(spec.sessionId, input.sessionId)))
        .for("update")
        .limit(1);
      const current = specs[0];
      if (!current)
        throw new Error(`Spec ${input.specId} is not attached to session ${input.sessionId}`);

      const latestRows = await tx
        .select()
        .from(specProjection)
        .where(eq(specProjection.specId, input.specId))
        .orderBy(desc(specProjection.rev))
        .limit(1);
      const latest = latestRows[0];
      if (
        latest &&
        PENDING_STATES.includes(latest.state as (typeof PENDING_STATES)[number]) &&
        latest.semanticDocSeq === current.semanticDocSeq
      ) {
        if (discardNotice && !latest.discardNotice && latest.rendered.byteLength === 0) {
          const updated = await tx
            .update(specProjection)
            .set({ discardNotice: true })
            .where(and(eq(specProjection.specId, input.specId), eq(specProjection.rev, latest.rev)))
            .returning();
          return projectionRecord(updated[0]!);
        }
        if (!discardNotice || latest.discardNotice || latest.rendered.byteLength === 0) {
          return projectionRecord(latest);
        }
      }
      if (
        latest?.state === "published" &&
        latest.semanticDocSeq === current.semanticDocSeq &&
        !discardNotice
      ) {
        return projectionRecord(latest);
      }

      const rev = (latest?.rev ?? 0n) + 1n;
      const stagingPath = `/workspace/.engrams/spec/incoming-${rev}.md`;
      const inserted = await tx
        .insert(specProjection)
        .values({
          specId: input.specId,
          rev,
          sessionId: input.sessionId,
          docSeq: current.docSeq,
          semanticDocSeq: current.semanticDocSeq,
          sha256: "",
          rendered: Buffer.alloc(0),
          documentState: Buffer.alloc(0),
          digest: Buffer.alloc(0),
          digestSha256: "",
          stagingPath,
          state: "rendering",
          requestedSource: input.source,
          discardNotice,
          createdAt: this.now(),
        })
        .returning();
      return projectionRecord(inserted[0]!);
    });
  }

  async pending(sessionId: string): Promise<ProjectionRecord[]> {
    const rows = await getDb()
      .select()
      .from(specProjection)
      .where(
        and(
          eq(specProjection.sessionId, sessionId),
          inArray(specProjection.state, [...PENDING_STATES]),
        ),
      )
      .orderBy(asc(specProjection.rev));
    return rows.map(projectionRecord);
  }

  async latestPublished(specId: string): Promise<ProjectionRecord | null> {
    const rows = await getDb()
      .select()
      .from(specProjection)
      .where(and(eq(specProjection.specId, specId), eq(specProjection.state, "published")))
      .orderBy(desc(specProjection.rev))
      .limit(1);
    return rows[0] ? projectionRecord(rows[0]) : null;
  }

  async recordRender(
    specId: string,
    rev: bigint,
    value: {
      docSeq: bigint;
      semanticDocSeq: bigint;
      sha256: string;
      rendered: Uint8Array;
      documentState: Uint8Array;
      digest: Uint8Array;
      digestSha256: string;
    },
  ): Promise<void> {
    const updated = await getDb().transaction(async (tx) => {
      await tx.execute(sql`SELECT set_config('engrams.semantic_revision_writer', '1', true)`);
      return tx
        .update(specProjection)
        .set({
          docSeq: value.docSeq,
          semanticDocSeq: value.semanticDocSeq,
          sha256: value.sha256,
          rendered: Buffer.from(value.rendered),
          documentState: Buffer.from(value.documentState),
          digest: Buffer.from(value.digest),
          digestSha256: value.digestSha256,
        })
        .where(
          and(
            eq(specProjection.specId, specId),
            eq(specProjection.rev, rev),
            sql`octet_length(${specProjection.rendered}) = 0`,
          ),
        )
        .returning({ rev: specProjection.rev });
    });
    if (updated.length === 0) {
      throw new Error(`Spec projection ${specId}:${rev} render is already pinned`);
    }
  }

  async markStaged(specId: string, rev: bigint): Promise<void> {
    await getDb()
      .update(specProjection)
      .set({ state: "staged" })
      .where(and(eq(specProjection.specId, specId), eq(specProjection.rev, rev)));
  }

  async markPublished(specId: string, rev: bigint): Promise<void> {
    await getDb().transaction(async (tx) => {
      await tx
        .update(specProjection)
        .set({ state: "superseded" })
        .where(
          and(
            eq(specProjection.specId, specId),
            eq(specProjection.state, "published"),
            lt(specProjection.rev, rev),
          ),
        );
      await tx
        .update(specProjection)
        .set({ state: "published", pushedAt: this.now() })
        .where(and(eq(specProjection.specId, specId), eq(specProjection.rev, rev)));
    });
  }

  async markSuperseded(specId: string, rev: bigint): Promise<void> {
    await getDb()
      .update(specProjection)
      .set({ state: "superseded" })
      .where(and(eq(specProjection.specId, specId), eq(specProjection.rev, rev)));
  }

  async state(specId: string, rev: bigint): Promise<ProjectionRecord["state"] | null> {
    const rows = await getDb()
      .select({ state: specProjection.state })
      .from(specProjection)
      .where(and(eq(specProjection.specId, specId), eq(specProjection.rev, rev)))
      .limit(1);
    return (rows[0]?.state as ProjectionRecord["state"] | undefined) ?? null;
  }

  async get(specId: string, rev: bigint): Promise<ProjectionRecord | null> {
    const rows = await getDb()
      .select()
      .from(specProjection)
      .where(and(eq(specProjection.specId, specId), eq(specProjection.rev, rev)))
      .limit(1);
    return rows[0] ? projectionRecord(rows[0]) : null;
  }

  async specForSession(sessionId: string): Promise<string | null> {
    const rows = await getDb()
      .select({ id: spec.id })
      .from(spec)
      .where(eq(spec.sessionId, sessionId))
      .limit(1);
    return rows[0]?.id ?? null;
  }
}

export interface CanonicalSpecRender {
  docSeq: bigint;
  semanticDocSeq: bigint;
  markdown: string;
  documentState: Uint8Array;
}

export interface CanonicalSpecRenderer {
  render(specId: string): Promise<CanonicalSpecRender>;
}

export class PostgresCanonicalSpecRenderer implements CanonicalSpecRenderer {
  async render(specId: string): Promise<CanonicalSpecRender> {
    return getDb().transaction(async (tx) => {
      // The shared row lock makes current_doc_seq and its update tail one
      // revision. A document writer must update this row before it appends.
      const docs = await tx
        .select({
          currentDocSeq: spec.currentDocSeq,
          currentSemanticDocSeq: spec.currentSemanticDocSeq,
        })
        .from(spec)
        .where(eq(spec.id, specId))
        .for("share")
        .limit(1);
      if (!docs[0]) throw new Error(`Unknown spec: ${specId}`);
      const currentDocSeq = docs[0].currentDocSeq;
      const currentSemanticDocSeq = docs[0].currentSemanticDocSeq;
      const snapshots = await tx
        .select({ state: specSnapshot.state, coveredSeq: specSnapshot.coveredSeq })
        .from(specSnapshot)
        .where(and(eq(specSnapshot.specId, specId), lte(specSnapshot.coveredSeq, currentDocSeq)))
        .limit(1);
      const snapshot = snapshots[0];
      const doc = new Y.Doc();
      let afterSeq = 0n;
      if (snapshot) {
        Y.applyUpdate(doc, snapshot.state);
        afterSeq = snapshot.coveredSeq;
      }
      const updates = await tx
        .select({ seq: specUpdateLog.seq, update: specUpdateLog.update })
        .from(specUpdateLog)
        .where(
          and(
            eq(specUpdateLog.specId, specId),
            gt(specUpdateLog.seq, afterSeq),
            lte(specUpdateLog.seq, currentDocSeq),
          ),
        )
        .orderBy(asc(specUpdateLog.seq));
      for (const update of updates) Y.applyUpdate(doc, update.update);
      return {
        docSeq: currentDocSeq,
        semanticDocSeq: currentSemanticDocSeq,
        markdown: renderMarkdown(proseMirrorDocument(doc)),
        documentState: Y.encodeStateAsUpdate(doc),
      };
    });
  }
}

export interface ProjectionGuestClient extends SessionFileClient, DurableExecClient {}

async function writeGuestFile(
  client: SessionFileClient,
  sessionId: string,
  path: string,
  bytes: Uint8Array,
  sha256: string,
): Promise<void> {
  async function* frames() {
    yield {
      frame: {
        case: "metadata" as const,
        value: {
          sessionId,
          path,
          sizeBytes: BigInt(bytes.byteLength),
          sha256,
          mode: 0o444,
        },
      },
    };
    yield { frame: { case: "chunk" as const, value: bytes } };
  }
  await client.writeFile(frames());
}

export interface SpecProjectionDriverOptions {
  sleep?: (ms: number) => Promise<void>;
  nowMs: () => number;
}

export class SpecProjectionDriver implements SpecProjection {
  private readonly encoder = new TextEncoder();
  private readonly sleep: (ms: number) => Promise<void>;
  private readonly nowMs: () => number;
  private readonly runs = new Map<string, Promise<number>>();

  constructor(
    private readonly store: SpecProjectionStore,
    private readonly renderer: CanonicalSpecRenderer,
    private readonly digest: SpecDigestService,
    private readonly guest: ProjectionGuestClient,
    options: SpecProjectionDriverOptions,
  ) {
    this.sleep = options.sleep ?? Bun.sleep;
    this.nowMs = options.nowMs;
  }

  async enqueue(input: SpecProjectionRequest): Promise<bigint> {
    return (await this.store.reserve(input)).rev;
  }

  async request(input: SpecProjectionRequest): Promise<void> {
    const rev = await this.enqueue(input);
    await this.waitUntilPublished(input.specId, rev);
  }

  async requestForSession(sessionId: string, source: string): Promise<bigint | null> {
    const specId = await this.store.specForSession(sessionId);
    return specId ? this.enqueue({ specId, sessionId, source }) : null;
  }

  async appliesTo(sessionId: string): Promise<boolean> {
    return (await this.store.specForSession(sessionId)) !== null;
  }

  async storeSpecForSession(sessionId: string): Promise<string | null> {
    return this.store.specForSession(sessionId);
  }

  async runOnce(sessionId: string): Promise<number> {
    const running = this.runs.get(sessionId);
    if (running) return running;
    const run = this.runPending(sessionId).finally(() => {
      if (this.runs.get(sessionId) === run) this.runs.delete(sessionId);
    });
    this.runs.set(sessionId, run);
    return run;
  }

  async waitUntilPublished(specId: string, rev: bigint, deadlineMs = 15_000): Promise<void> {
    const deadline = this.nowMs() + deadlineMs;
    for (;;) {
      const state = await this.store.state(specId, rev);
      if (state === "published") return;
      if (state === "superseded") {
        const requested = await this.store.get(specId, rev);
        const latest = await this.store.latestPublished(specId);
        if (
          requested &&
          latest &&
          latest.rev > requested.rev &&
          latest.docSeq >= requested.docSeq
        ) {
          return;
        }
      }
      if (this.nowMs() >= deadline) {
        throw new Error(
          `Spec projection ${specId}:${rev} was not published within ${deadlineMs}ms`,
        );
      }
      await this.sleep(25);
    }
  }

  async waitForIdle(sessionId: string): Promise<void> {
    await this.runs.get(sessionId);
  }

  async preparePrompt(sessionId: string, sessionStatus: string): Promise<void> {
    if (!new Set(["created", "idle", "parked"]).has(sessionStatus)) return;
    const specId = await this.store.specForSession(sessionId);
    if (!specId) return;
    const rev = await this.enqueue({ specId, sessionId, source: "prompt-delivery" });
    // The listener-lease scanner performs the publish. This request only waits
    // for its durable intent; it never runs the file pipeline itself.
    await this.waitUntilPublished(specId, rev);
  }

  async checkDrift(specId: string, sessionId: string): Promise<boolean> {
    const published = await this.store.latestPublished(specId);
    if (!published) return false;
    let observed = "";
    try {
      const iterator = this.guest
        .readFile({ sessionId, path: SPEC_PROJECTION_PATH })
        [Symbol.asyncIterator]();
      try {
        const first = await iterator.next();
        if (!first.done && first.value.frame.case === "metadata") {
          observed = first.value.frame.value.sha256;
        }
      } finally {
        await iterator.return?.();
      }
    } catch {
      // A missing projection is the same drift result as a digest mismatch.
    }
    if (observed === published.sha256) return false;
    await this.store.reserve({ specId, sessionId, source: "drift-repair" }, true);
    return true;
  }

  private async publish(record: ProjectionRecord): Promise<void> {
    let docSeq = record.docSeq;
    let semanticDocSeq = record.semanticDocSeq;
    let rendered = record.rendered;
    let documentState = record.documentState;
    let digest = record.digest;
    let sha256 = record.sha256;
    let digestSha256 = record.digestSha256;
    if (rendered.byteLength === 0) {
      const canonical = await this.renderer.render(record.specId);
      const previous = await this.store.latestPublished(record.specId);
      const body = [
        `<!-- Engrams spec projection rev ${record.rev}. Direct edits are discarded; use spec_* tools. -->`,
        "",
        canonical.markdown,
      ].join("\n");
      docSeq = canonical.docSeq;
      semanticDocSeq = canonical.semanticDocSeq;
      rendered = this.encoder.encode(body);
      documentState = canonical.documentState;
      sha256 = createHash("sha256").update(rendered).digest("hex");
      digest = this.encoder.encode(
        await this.digest.render(
          record.specId,
          previous?.docSeq ?? 0n,
          canonical.docSeq,
          record.discardNotice,
          previous?.documentState ?? null,
        ),
      );
      digestSha256 = createHash("sha256").update(digest).digest("hex");
      await this.store.recordRender(record.specId, record.rev, {
        docSeq,
        semanticDocSeq,
        sha256,
        rendered,
        documentState,
        digest,
        digestSha256,
      });
    }
    const digestStaging = `/workspace/.engrams/spec/incoming-digest-${record.rev}.md`;
    await writeGuestFile(this.guest, record.sessionId, record.stagingPath, rendered, sha256);
    await writeGuestFile(this.guest, record.sessionId, digestStaging, digest, digestSha256);
    await this.store.markStaged(record.specId, record.rev);

    const command = [
      "flock /workspace/.engrams/spec/lock sh -c '",
      `mv ${record.stagingPath} ${SPEC_PROJECTION_PATH} && `,
      `mv ${digestStaging} ${SPEC_DIGEST_PATH} && `,
      "rm -f /workspace/.engrams/spec/incoming-*.md'",
    ].join("");
    const result = await runExec(this.guest, record.sessionId, command, {
      execId: `spec-publish-${record.specId}-${record.rev}`,
      deadlineMs: PUBLISH_DEADLINE_MS,
    });
    if (result.exitStatus !== 0) {
      await this.store.markSuperseded(record.specId, record.rev);
      await this.store.reserve(
        {
          specId: record.specId,
          sessionId: record.sessionId,
          source: "publish-retry",
        },
        record.discardNotice,
      );
      throw new Error(
        `Spec projection publish failed with status ${String(result.exitStatus)}: ${result.stderr}`,
      );
    }
    await this.store.markPublished(record.specId, record.rev);
  }

  private async runPending(sessionId: string): Promise<number> {
    const pending = await this.store.pending(sessionId);
    for (const record of pending) await this.publish(record);
    return pending.length;
  }
}

// The generated SessionService client implements both narrow structural seams.
const productionGuest = sessions as ProjectionGuestClient;

export const productionSpecProjection = new SpecProjectionDriver(
  new PostgresSpecProjectionStore(() => new Date()),
  new PostgresCanonicalSpecRenderer(),
  new SpecDigestService(new PostgresSpecDigestSource()),
  productionGuest,
  { nowMs: Date.now },
);
