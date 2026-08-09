import { describe, expect, test } from "bun:test";
import { createHash } from "node:crypto";

import type { ExecAttachRequest, ExecOutputFrame } from "../../exec/durable-exec.ts";
import {
  SPEC_DIGEST_PATH,
  SPEC_PROJECTION_PATH,
  SpecProjectionDriver,
  type CanonicalSpecRenderer,
  type ProjectionGuestClient,
  type ProjectionRecord,
  type SpecProjectionRequest,
  type SpecProjectionStore,
} from "../projection.ts";
import { SpecDigestService, type SpecDigestSource } from "../digest.ts";
import { makeSpecProjectionConsumer } from "../../listeners/spec-projection-consumer.ts";

const SPEC_ID = "00000000-0000-4000-8000-000000001109";
const SESSION_ID = "00000000-0000-4000-8000-000000001102";

class MemoryProjectionStore implements SpecProjectionStore {
  readonly rows: ProjectionRecord[] = [];
  readonly sessionSpecs = new Map([[SESSION_ID, SPEC_ID]]);
  currentDocSeq = 7n;

  async reserve(input: SpecProjectionRequest, discardNotice = false): Promise<ProjectionRecord> {
    const latest = this.rows.filter((row) => row.specId === input.specId).at(-1);
    if (
      latest &&
      (latest.state === "rendering" || latest.state === "staged") &&
      latest.docSeq === this.currentDocSeq
    ) {
      if (!discardNotice || latest.discardNotice || latest.rendered.byteLength === 0) {
        latest.discardNotice ||= discardNotice;
        return latest;
      }
    }
    const rev = (latest?.rev ?? 0n) + 1n;
    const row: ProjectionRecord = {
      ...input,
      rev,
      docSeq: this.currentDocSeq,
      sha256: "",
      rendered: new Uint8Array(),
      documentState: new Uint8Array(),
      digest: new Uint8Array(),
      digestSha256: "",
      stagingPath: `/workspace/.engrams/spec/incoming-${rev}.md`,
      state: "rendering",
      requestedSource: input.source,
      discardNotice,
    };
    this.rows.push(row);
    return row;
  }

  async pending(sessionId: string): Promise<ProjectionRecord[]> {
    return this.rows.filter(
      (row) => row.sessionId === sessionId && (row.state === "rendering" || row.state === "staged"),
    );
  }

  async latestPublished(specId: string): Promise<ProjectionRecord | null> {
    return this.rows.filter((row) => row.specId === specId && row.state === "published").at(-1) ?? null;
  }

  async recordRender(
    specId: string,
    rev: bigint,
    value: {
      docSeq: bigint;
      sha256: string;
      rendered: Uint8Array;
      documentState: Uint8Array;
      digest: Uint8Array;
      digestSha256: string;
    },
  ): Promise<void> {
    Object.assign(this.row(specId, rev), value);
  }

  async markStaged(specId: string, rev: bigint): Promise<void> {
    this.row(specId, rev).state = "staged";
  }

  async markPublished(specId: string, rev: bigint): Promise<void> {
    for (const row of this.rows) {
      if (row.specId === specId && row.state === "published") row.state = "superseded";
    }
    this.row(specId, rev).state = "published";
  }

  async markSuperseded(specId: string, rev: bigint): Promise<void> {
    this.row(specId, rev).state = "superseded";
  }

  async state(specId: string, rev: bigint): Promise<ProjectionRecord["state"] | null> {
    return this.rows.find((row) => row.specId === specId && row.rev === rev)?.state ?? null;
  }

  async get(specId: string, rev: bigint): Promise<ProjectionRecord | null> {
    return this.rows.find((row) => row.specId === specId && row.rev === rev) ?? null;
  }

  async specForSession(sessionId: string): Promise<string | null> {
    return this.sessionSpecs.get(sessionId) ?? null;
  }

  private row(specId: string, rev: bigint): ProjectionRecord {
    const row = this.rows.find((candidate) => candidate.specId === specId && candidate.rev === rev);
    if (!row) throw new Error("missing projection row");
    return row;
  }
}

class FakeGuest implements ProjectionGuestClient {
  readonly files = new Map<string, Uint8Array>();
  readonly journals = new Map<string, { command: string; exitStatus: number }>();
  readonly attempts = new Map<string, number>();
  spawned = 0;
  bodyBytesRead = 0;
  severFirstAttach = false;
  failNextPublish = false;
  failWriteOnce: string | null = null;
  onWrite: ((path: string) => void | Promise<void>) | null = null;

  async writeFile(input: AsyncIterable<Parameters<ProjectionGuestClient["writeFile"]>[0] extends AsyncIterable<infer F> ? F : never>) {
    let path = "";
    let expectedSha = "";
    const chunks: Uint8Array[] = [];
    for await (const frame of input) {
      if (frame.frame.case === "metadata") {
        path = frame.frame.value.path;
        expectedSha = frame.frame.value.sha256;
      } else {
        chunks.push(frame.frame.value);
      }
    }
    const bytes = concat(chunks);
    expect(createHash("sha256").update(bytes).digest("hex")).toBe(expectedSha);
    const existing = this.files.get(path);
    if (existing && !Buffer.from(existing).equals(Buffer.from(bytes))) {
      throw new Error("create-only path has different content");
    }
    if (this.failWriteOnce === path) {
      this.failWriteOnce = null;
      throw new Error("injected staging failure");
    }
    this.files.set(path, bytes);
    await this.onWrite?.(path);
    return { path, sizeBytes: BigInt(bytes.byteLength), sha256: expectedSha };
  }

  async *readFile(input: { sessionId: string; path: string }) {
    const bytes = this.files.get(input.path);
    if (!bytes) throw new Error("not found");
    yield {
      frame: {
        case: "metadata" as const,
        value: {
          path: input.path,
          sizeBytes: BigInt(bytes.byteLength),
          sha256: createHash("sha256").update(bytes).digest("hex"),
          fileName: input.path.split("/").at(-1) ?? "file",
        },
      },
    };
    this.bodyBytesRead += bytes.byteLength;
    yield { frame: { case: "chunk" as const, value: bytes } };
  }

  exec(req: ExecAttachRequest): AsyncIterable<ExecOutputFrame> {
    const self = this;
    return {
      async *[Symbol.asyncIterator]() {
        const execId = req.execId!;
        const attempt = (self.attempts.get(execId) ?? 0) + 1;
        self.attempts.set(execId, attempt);
        let journal = self.journals.get(execId);
        if (!journal) {
          self.spawned += 1;
          journal = {
            command: req.command,
            exitStatus: self.failNextPublish ? 1 : 0,
          };
          self.failNextPublish = false;
          self.journals.set(execId, journal);
        }
        yield { event: { case: "started" as const, value: { execId } } };
        if (self.severFirstAttach && attempt === 1) return;
        if (journal.exitStatus === 0) self.applyPublish(journal.command);
        yield { event: { case: "exit" as const, value: { exitStatus: journal.exitStatus } } };
      },
    };
  }

  async cancelExec(): Promise<void> {}

  private applyPublish(command: string): void {
    const moves = [...command.matchAll(/mv (\/workspace\/[^ ]+) (\/workspace\/[^ &']+)/g)];
    for (const move of moves) {
      const bytes = this.files.get(move[1]!);
      if (!bytes) throw new Error(`missing staged file ${move[1]}`);
      this.files.set(move[2]!, bytes);
      this.files.delete(move[1]!);
    }
    for (const path of [...this.files.keys()]) {
      if (path.startsWith("/workspace/.engrams/spec/incoming-")) this.files.delete(path);
    }
  }
}

function concat(chunks: Uint8Array[]): Uint8Array {
  const result = new Uint8Array(chunks.reduce((sum, chunk) => sum + chunk.byteLength, 0));
  let offset = 0;
  for (const chunk of chunks) {
    result.set(chunk, offset);
    offset += chunk.byteLength;
  }
  return result;
}

function fixture() {
  const store = new MemoryProjectionStore();
  const guest = new FakeGuest();
  let rendererCalls = 0;
  const renderer: CanonicalSpecRenderer = {
    render: async () => {
      rendererCalls += 1;
      return {
        docSeq: store.currentDocSeq,
        markdown: `## Context\n\nCanonical ${store.currentDocSeq}\n`,
        documentState: new Uint8Array([1, 2, Number(store.currentDocSeq)]),
      };
    },
  };
  const source: SpecDigestSource = {
    changes: async () => [{ sectionId: "context", sectionTitle: "Context", author: "Ada" }],
  };
  const driver = new SpecProjectionDriver(
    store,
    renderer,
    new SpecDigestService(source),
    guest,
    { sleep: () => Bun.sleep(1), nowMs: Date.now },
  );
  return { store, guest, driver, rendererCalls: () => rendererCalls };
}

describe("SpecProjectionDriver", () => {
  test("durable exec replay attaches to one stable publish", async () => {
    const { driver, guest } = fixture();
    guest.severFirstAttach = true;
    await driver.enqueue({ specId: SPEC_ID, sessionId: SESSION_ID, source: "test" });
    await driver.runOnce(SESSION_ID);

    expect(guest.spawned).toBe(1);
    expect(guest.attempts.get(`spec-publish-${SPEC_ID}-1`)).toBe(2);
    expect(new TextDecoder().decode(guest.files.get(SPEC_PROJECTION_PATH))).toContain("Canonical");
  });

  test("a staging retry reuses the pinned projection and digest bytes", async () => {
    const { driver, guest, store, rendererCalls } = fixture();
    guest.failWriteOnce = "/workspace/.engrams/spec/incoming-digest-1.md";
    await driver.enqueue({ specId: SPEC_ID, sessionId: SESSION_ID, source: "test" });
    await expect(driver.runOnce(SESSION_ID)).rejects.toThrow("injected staging failure");
    const pinned = store.rows[0]!;
    const rendered = pinned.rendered.slice();
    const digest = pinned.digest.slice();
    store.currentDocSeq = 8n;

    await driver.runOnce(SESSION_ID);
    expect(rendererCalls()).toBe(1);
    expect(pinned.rendered).toEqual(rendered);
    expect(pinned.digest).toEqual(digest);
    expect(new TextDecoder().decode(guest.files.get(SPEC_PROJECTION_PATH))).toContain("Canonical 7");
  });

  test("a mutation during staging reserves and publishes a dirty successor", async () => {
    const { driver, guest, store } = fixture();
    let reserved = false;
    guest.onWrite = async (path) => {
      if (reserved || path !== "/workspace/.engrams/spec/incoming-1.md") return;
      reserved = true;
      store.currentDocSeq = 8n;
      await driver.enqueue({
        specId: SPEC_ID,
        sessionId: SESSION_ID,
        source: "agent-tool-mutation",
      });
    };
    await driver.enqueue({ specId: SPEC_ID, sessionId: SESSION_ID, source: "initial" });
    await driver.runOnce(SESSION_ID);
    expect(store.rows.map((row) => row.docSeq)).toEqual([7n, 8n]);
    expect((await store.pending(SESSION_ID)).map((row) => row.rev)).toEqual([2n]);

    guest.onWrite = null;
    await driver.runOnce(SESSION_ID);
    expect(store.rows.map((row) => row.state)).toEqual(["superseded", "published"]);
    expect(new TextDecoder().decode(guest.files.get(SPEC_PROJECTION_PATH))).toContain("Canonical 8");
  });

  test("the next publish sweeps a staged file after a failed publish", async () => {
    const { driver, guest } = fixture();
    guest.failNextPublish = true;
    await driver.enqueue({ specId: SPEC_ID, sessionId: SESSION_ID, source: "first" });
    await expect(driver.runOnce(SESSION_ID)).rejects.toThrow("status 1");
    expect(guest.files.has("/workspace/.engrams/spec/incoming-1.md")).toBe(true);

    await driver.enqueue({ specId: SPEC_ID, sessionId: SESSION_ID, source: "retry" });
    await driver.runOnce(SESSION_ID);
    expect([...guest.files.keys()].filter((path) => path.includes("incoming-"))).toEqual([]);
    expect(guest.files.has(SPEC_PROJECTION_PATH)).toBe(true);
  });

  test("metadata drift coalesces to one repair and transfers no file body", async () => {
    const { driver, guest, store } = fixture();
    await driver.enqueue({ specId: SPEC_ID, sessionId: SESSION_ID, source: "initial" });
    await driver.runOnce(SESSION_ID);
    guest.files.set(SPEC_PROJECTION_PATH, new TextEncoder().encode("direct edit"));

    expect(await driver.checkDrift(SPEC_ID, SESSION_ID)).toBe(true);
    expect(await driver.checkDrift(SPEC_ID, SESSION_ID)).toBe(true);
    expect(guest.bodyBytesRead).toBe(0);
    expect(store.rows.filter((row) => row.state === "rendering")).toHaveLength(1);

    await driver.runOnce(SESSION_ID);
    expect(store.rows).toHaveLength(2);
    expect(new TextDecoder().decode(guest.files.get(SPEC_DIGEST_PATH))).toContain(
      "direct edit to /workspace/spec.md was discarded",
    );
  });

  test("drift against a pinned pending render reserves one notice successor", async () => {
    const { driver, guest, store } = fixture();
    await driver.enqueue({ specId: SPEC_ID, sessionId: SESSION_ID, source: "initial" });
    await driver.runOnce(SESSION_ID);

    guest.failWriteOnce = "/workspace/.engrams/spec/incoming-digest-2.md";
    await driver.enqueue({ specId: SPEC_ID, sessionId: SESSION_ID, source: "boundary" });
    await expect(driver.runOnce(SESSION_ID)).rejects.toThrow("injected staging failure");
    guest.files.set(SPEC_PROJECTION_PATH, new TextEncoder().encode("direct edit"));
    await driver.checkDrift(SPEC_ID, SESSION_ID);
    await driver.checkDrift(SPEC_ID, SESSION_ID);

    expect(store.rows).toHaveLength(3);
    expect(store.rows[1]!.discardNotice).toBe(false);
    expect(store.rows[2]!.discardNotice).toBe(true);
  });

  test("a prompt waits across a failed exec until its durable successor publishes", async () => {
    const { driver, guest, store } = fixture();
    guest.failNextPublish = true;
    let ready = false;
    const prompt = driver.preparePrompt(SESSION_ID, "parked").then(() => {
      ready = true;
    });
    while ((await store.pending(SESSION_ID)).length === 0) await Promise.resolve();
    await expect(driver.runOnce(SESSION_ID)).rejects.toThrow("status 1");
    await Promise.resolve();
    expect(ready).toBe(false);
    expect((await store.pending(SESSION_ID)).map((row) => row.rev)).toEqual([2n]);

    await driver.runOnce(SESSION_ID);
    await prompt;
    expect(ready).toBe(true);
    expect(store.rows.map((row) => row.state)).toEqual(["superseded", "published"]);
  });

  test("waitForIdle holds lease release until an in-flight publish completes", async () => {
    const { driver, guest } = fixture();
    let release!: () => void;
    const gate = new Promise<void>((resolve) => {
      release = resolve;
    });
    guest.onWrite = (path) =>
      path === "/workspace/.engrams/spec/incoming-1.md" ? gate : undefined;
    await driver.enqueue({ specId: SPEC_ID, sessionId: SESSION_ID, source: "resume" });
    const run = driver.runOnce(SESSION_ID);
    while (!guest.files.has("/workspace/.engrams/spec/incoming-1.md")) await Promise.resolve();
    let idle = false;
    const wait = driver.waitForIdle(SESSION_ID).then(() => {
      idle = true;
    });
    await Promise.resolve();
    expect(idle).toBe(false);
    release();
    await run;
    await wait;
    expect(idle).toBe(true);
  });

  test("a parked prompt waits until the scanner publishes its resume view", async () => {
    const { driver, guest, store } = fixture();
    const ready = driver.preparePrompt(SESSION_ID, "parked");
    while ((await store.pending(SESSION_ID)).length === 0) await Promise.resolve();
    expect(guest.files.has(SPEC_PROJECTION_PATH)).toBe(false);
    await driver.runOnce(SESSION_ID);
    await ready;
    expect(guest.files.has(SPEC_PROJECTION_PATH)).toBe(true);
  });

  test("an agent mutation request returns only after the lease scanner publishes", async () => {
    const { driver, guest, store } = fixture();
    let returned = false;
    const request = driver.request({
      specId: SPEC_ID,
      sessionId: SESSION_ID,
      source: "agent-tool-mutation",
    }).then(() => {
      returned = true;
    });
    while ((await store.pending(SESSION_ID)).length === 0) await Promise.resolve();
    expect(returned).toBe(false);
    expect(guest.files.has(SPEC_PROJECTION_PATH)).toBe(false);
    await driver.runOnce(SESSION_ID);
    await request;
    expect(returned).toBe(true);
    expect(guest.files.has(SPEC_PROJECTION_PATH)).toBe(true);
  });

  test("the listener lease refreshes a harness-parked boundary", async () => {
    const { driver, store } = fixture();
    const consumer = makeSpecProjectionConsumer(driver);
    expect(await consumer.appliesTo(SESSION_ID)).toBe(true);
    await consumer.handle(
      { idx: 9n, kind: "harness_parked", payloadJson: "{}" },
      { sessionId: SESSION_ID },
    );
    expect(store.rows).toHaveLength(1);
    expect(store.rows[0]!.requestedSource).toBe("harness_parked");
    expect(store.rows[0]!.state).toBe("published");
  });
});
