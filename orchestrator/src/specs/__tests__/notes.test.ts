import { describe, expect, test } from "bun:test";
import {
  buildWorkingNotesDocument,
  createTemplateDocument,
  findSection,
  renderMarkdown,
  replaceSection,
  schema,
  SPEC_NOTES_FRAGMENT_NAME,
  type SpecTemplate,
  type SpecWorkingNotes,
  type SpecWorkingNotesInput,
} from "@engrams/spec-document";
import { prosemirrorToYXmlFragment } from "y-prosemirror";
import * as Y from "yjs";

import { MemoryDocumentStore } from "./memory-document-store.ts";
import {
  encodeProseMirrorDocument,
  proseMirrorDocument,
  readSpecWorkingNotes,
  specNotesArchivedAt,
  SpecDocumentService,
  SpecNotesArchivedError,
  SpecNotesEditError,
} from "../doc-service.ts";
import { SpecWorkingNotesService } from "../notes.ts";

const SPEC_ID = "00000000-0000-4000-8000-0000000002a0";
const NOW = () => new Date("2026-08-12T09:30:00.000Z");
const BROWSER_CLIENT_ID = "4242";
const AGENT_CLIENT_ID = "agent:session-1:call-1";

const TEMPLATE: SpecTemplate = {
  sections: [
    { id: "behavior", key: "behavior", title: "Behavior" },
    { id: "api", key: "api", title: "API surface" },
    { id: "data", key: "data", title: "Data model" },
  ],
};

function notesInput(): SpecWorkingNotesInput {
  return {
    clusters: [
      {
        id: "burst",
        theme: "burst semantics",
        sectionIds: ["behavior"],
        bullets: [
          {
            id: "b1",
            mark: "verified",
            kind: "observation",
            text: "orgs get a burst credit pool, refills daily",
            provenance: "your call, firm",
          },
          {
            id: "b2",
            mark: "contradicted",
            kind: "observation",
            text: "the limiter already handles bursts",
            provenance: "limits.rs @ 8f2c1a4: a per-user bucket, no credit concept",
          },
        ],
      },
      {
        id: "refusal",
        theme: "refusal path",
        sectionIds: ["behavior", "api"],
        bullets: [
          {
            id: "b3",
            mark: "verified",
            kind: "requirement",
            text: "a refusal names the org and the reset time, never a bare 429",
            provenance: "agreed with the author",
          },
        ],
      },
      {
        id: "pile",
        theme: "untagged",
        bullets: [
          {
            id: "b4",
            mark: "unchecked",
            kind: "question",
            text: "does billing want org-hour granularity or finer?",
          },
        ],
      },
    ],
  };
}

async function setup() {
  const store = new MemoryDocumentStore();
  const documents = new SpecDocumentService(store, { now: NOW });
  await documents.applyUpdate(
    SPEC_ID,
    encodeProseMirrorDocument(createTemplateDocument(TEMPLATE)),
    "seed",
  );
  const notes = new SpecWorkingNotesService({ documents, now: NOW });
  return { documents, notes, store };
}

async function readStage(documents: SpecDocumentService) {
  const loaded = await documents.syncFromLog(SPEC_ID);
  return {
    notes: readSpecWorkingNotes(loaded.doc),
    archivedAt: specNotesArchivedAt(loaded.doc),
    markdown: renderMarkdown(proseMirrorDocument(loaded.doc)),
    rev: loaded.semanticDocSeq,
  };
}

/** A person's keystroke: the sync route only ever passes a numeric client id. */
async function personWrites(
  documents: SpecDocumentService,
  replacement: SpecWorkingNotes,
  clientId = BROWSER_CLIENT_ID,
): Promise<void> {
  const loaded = await documents.syncFromLog(SPEC_ID);
  const fork = new Y.Doc();
  try {
    Y.applyUpdate(fork, Y.encodeStateAsUpdate(loaded.doc));
    const before = Y.encodeStateVector(fork);
    prosemirrorToYXmlFragment(
      buildWorkingNotesDocument(replacement),
      fork.getXmlFragment(SPEC_NOTES_FRAGMENT_NAME),
    );
    await documents.applyUpdate(SPEC_ID, Y.encodeStateAsUpdate(fork, before), clientId);
  } finally {
    fork.destroy();
  }
}

describe("working notes on the canvas", () => {
  test("a notes write is durable but does not move the spec revision", async () => {
    const { documents, notes, store } = await setup();
    const seeded = await readStage(documents);

    const result = await notes.update({
      specId: SPEC_ID,
      notes: notesInput(),
      clientId: AGENT_CLIENT_ID,
    });

    const after = await readStage(documents);
    expect(result.newRev).toBe(seeded.rev);
    expect(after.rev).toBe(seeded.rev);
    expect(store.lastEffects?.semanticChanged).toBe(false);
    expect(after.notes?.clusters.map((cluster) => cluster.id)).toEqual([
      "burst",
      "refusal",
      "pile",
    ]);
    expect(result.stage.untaggedBullets).toBe(1);
  });

  test("the notes never reach the spec render", async () => {
    const { documents, notes } = await setup();
    await notes.update({ specId: SPEC_ID, notes: notesInput(), clientId: AGENT_CLIENT_ID });

    const after = await readStage(documents);
    expect(after.markdown).not.toContain("burst credit pool");
    expect(after.markdown).toBe("## Behavior\n\n\n\n## API surface\n\n\n\n## Data model\n");
  });

  test("a destination tag must name a section this template has", async () => {
    const { notes } = await setup();
    const input = notesInput();
    input.clusters[0]!.sectionIds = ["failure-modes"];

    await expect(
      notes.update({ specId: SPEC_ID, notes: input, clientId: AGENT_CLIENT_ID }),
    ).rejects.toThrow("unknown section: failure-modes");
  });

  test("a rewritten notes set that changes nothing writes no update", async () => {
    const { notes, store } = await setup();
    await notes.update({ specId: SPEC_ID, notes: notesInput(), clientId: AGENT_CLIENT_ID });
    const updateCount = store.updates.length;

    await notes.update({ specId: SPEC_ID, notes: notesInput(), clientId: AGENT_CLIENT_ID });

    expect(store.updates).toHaveLength(updateCount);
  });
});

describe("a person corrects the agent without a chat round-trip", () => {
  test("their words win, and the next agent write is told", async () => {
    const { documents, notes } = await setup();
    await notes.update({ specId: SPEC_ID, notes: notesInput(), clientId: AGENT_CLIENT_ID });
    const current = (await readStage(documents)).notes!;
    current.clusters[0]!.bullets[0]!.text = "orgs get a burst credit pool, refills weekly";
    await personWrites(documents, current);

    const result = await notes.update({
      specId: SPEC_ID,
      notes: notesInput(),
      clientId: AGENT_CLIENT_ID,
    });

    expect(result.corrections).toEqual([
      {
        bulletId: "b1",
        agentText: "orgs get a burst credit pool, refills daily",
        personText: "orgs get a burst credit pool, refills weekly",
        keptAgainstDrop: false,
      },
    ]);
    const after = await readStage(documents);
    expect(after.notes?.clusters[0]!.bullets[0]!.text).toBe(
      "orgs get a burst credit pool, refills weekly",
    );
  });

  test("the agent cannot drop a corrected bullet", async () => {
    const { documents, notes } = await setup();
    await notes.update({ specId: SPEC_ID, notes: notesInput(), clientId: AGENT_CLIENT_ID });
    const current = (await readStage(documents)).notes!;
    current.clusters[2]!.bullets[0]!.text = "billing wants org-hour granularity";
    await personWrites(documents, current);
    const input = notesInput();
    input.clusters.splice(2, 1);

    const result = await notes.update({
      specId: SPEC_ID,
      notes: input,
      clientId: AGENT_CLIENT_ID,
    });

    expect(result.corrections[0]?.keptAgainstDrop).toBe(true);
    const after = await readStage(documents);
    expect(JSON.stringify(after.notes)).toContain("billing wants org-hour granularity");
  });

  test("a person cannot change a mark, a tag or the bullet set", async () => {
    const { documents, notes } = await setup();
    await notes.update({ specId: SPEC_ID, notes: notesInput(), clientId: AGENT_CLIENT_ID });
    const current = (await readStage(documents)).notes!;
    current.clusters[0]!.bullets[0]!.mark = "contradicted";

    await expect(personWrites(documents, current)).rejects.toThrow(SpecNotesEditError);
  });

  test("only the agent opens the notes", async () => {
    const { documents } = await setup();

    await expect(
      personWrites(documents, {
        clusters: [
          {
            id: "c1",
            theme: "mine",
            sectionIds: [],
            bullets: [
              {
                id: "x1",
                mark: "unchecked",
                kind: "observation",
                text: "typed by a person",
                provenance: null,
                agentText: "typed by a person",
              },
            ],
          },
        ],
      }),
    ).rejects.toThrow("Only the agent opens the working notes");
  });
});

describe("distillation closes the stage", () => {
  test("every tagged cluster becomes section material, grouped by theme", async () => {
    const { documents, notes } = await setup();
    await notes.update({ specId: SPEC_ID, notes: notesInput(), clientId: AGENT_CLIENT_ID });

    const result = await notes.distill({ specId: SPEC_ID, clientId: AGENT_CLIENT_ID });

    expect(result.applied).toBe(true);
    const after = await readStage(documents);
    expect(after.markdown).toContain("### burst semantics");
    expect(after.markdown).toContain(
      "Verified — orgs get a burst credit pool, refills daily (your call, firm)",
    );
    expect(after.markdown).toContain("Requirement candidate — a refusal names the org");
    expect(result.distillation.sections.map((section) => section.sectionId)).toEqual([
      "behavior",
      "api",
    ]);
  });

  test("a section with no tagged material stays empty", async () => {
    const { documents, notes } = await setup();
    await notes.update({ specId: SPEC_ID, notes: notesInput(), clientId: AGENT_CLIENT_ID });

    await notes.distill({ specId: SPEC_ID, clientId: AGENT_CLIENT_ID });

    const after = await readStage(documents);
    expect(after.markdown).toContain("## Data model\n");
    expect(after.markdown.split("## Data model")[1]).toBe("\n");
  });

  test("a refuted claim and an untagged bullet never enter the spec", async () => {
    const { documents, notes } = await setup();
    await notes.update({ specId: SPEC_ID, notes: notesInput(), clientId: AGENT_CLIENT_ID });

    const result = await notes.distill({ specId: SPEC_ID, clientId: AGENT_CLIENT_ID });

    const after = await readStage(documents);
    expect(result.distillation.refutedBullets).toBe(1);
    expect(result.distillation.untaggedBullets).toBe(1);
    expect(after.markdown).not.toContain("the limiter already handles bursts");
    expect(after.markdown).not.toContain("org-hour granularity");
  });

  test("the archive stays browsable, and the second call changes nothing", async () => {
    const { documents, notes, store } = await setup();
    await notes.update({ specId: SPEC_ID, notes: notesInput(), clientId: AGENT_CLIENT_ID });
    const first = await notes.distill({ specId: SPEC_ID, clientId: AGENT_CLIENT_ID });
    const updateCount = store.updates.length;

    const second = await notes.distill({ specId: SPEC_ID, clientId: AGENT_CLIENT_ID });

    expect(second.applied).toBe(false);
    expect(second.distillation).toEqual(first.distillation);
    expect(store.updates).toHaveLength(updateCount);
    const after = await readStage(documents);
    expect(after.archivedAt).toBe("2026-08-12T09:30:00.000Z");
    expect(after.notes?.clusters).toHaveLength(3);
    expect(after.markdown.match(/### burst semantics/g)).toHaveLength(1);
  });

  test("the archive is read-only for the agent and for a person", async () => {
    const { documents, notes } = await setup();
    await notes.update({ specId: SPEC_ID, notes: notesInput(), clientId: AGENT_CLIENT_ID });
    const current = (await readStage(documents)).notes!;
    await notes.distill({ specId: SPEC_ID, clientId: AGENT_CLIENT_ID });

    await expect(
      notes.update({ specId: SPEC_ID, notes: notesInput(), clientId: AGENT_CLIENT_ID }),
    ).rejects.toThrow(SpecNotesArchivedError);
    current.clusters[0]!.bullets[0]!.text = "a late correction";
    await expect(personWrites(documents, current)).rejects.toThrow(SpecNotesArchivedError);
  });

  test("archiving with nothing tagged is the skip, and it writes no section", async () => {
    const { documents, notes, store } = await setup();
    const input = notesInput();
    for (const cluster of input.clusters) cluster.sectionIds = [];
    await notes.update({ specId: SPEC_ID, notes: input, clientId: AGENT_CLIENT_ID });
    const before = await readStage(documents);

    const result = await notes.distill({ specId: SPEC_ID, clientId: AGENT_CLIENT_ID });

    const after = await readStage(documents);
    expect(result.distillation.sections).toEqual([]);
    expect(after.markdown).toBe(before.markdown);
    expect(after.archivedAt).toBe("2026-08-12T09:30:00.000Z");
    expect(store.lastEffects?.semanticChanged).toBe(false);
  });

  test("distillation keeps prose an author already wrote", async () => {
    const { documents, notes } = await setup();
    await notes.update({ specId: SPEC_ID, notes: notesInput(), clientId: AGENT_CLIENT_ID });
    // A body written before the stage closed. Distillation must not eat it.
    await documents.mutateDocument(SPEC_ID, "author", (document) => {
      const section = findSection(document, "behavior");
      if (!section) throw new Error("the test template lost its section");
      return replaceSection(
        document,
        "behavior",
        section.node.type.create(section.node.attrs, [
          section.node.child(0),
          schema.nodes.paragraph!.create(null, schema.text("The author's own words.")),
        ]),
      );
    });

    await notes.distill({ specId: SPEC_ID, clientId: AGENT_CLIENT_ID });

    const after = await readStage(documents);
    expect(after.markdown).toContain("The author's own words.");
    expect(after.markdown).toContain("### burst semantics");
  });

  test("distilling before the stage opened is refused", async () => {
    const { notes } = await setup();

    await expect(notes.distill({ specId: SPEC_ID, clientId: AGENT_CLIENT_ID })).rejects.toThrow(
      "no working notes to distil",
    );
  });
});
