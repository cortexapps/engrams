import { describe, expect, test } from "bun:test";

import {
  OpenQuestionError,
  OpenQuestionService,
  type OpenQuestionRecord,
  type OpenQuestionStore,
  type QuestionDocument,
} from "../open-questions.ts";

const QUESTION_FINGERPRINT = JSON.stringify([
  "00000000-0000-4000-8000-000000000001",
  "failure-modes",
  "How does retry stop?",
  "user-1",
  "yjs-section://failure-modes/AAEC",
  null,
]);

const question: OpenQuestionRecord = {
  id: "00000000-0000-4000-8000-000000000011",
  specId: "00000000-0000-4000-8000-000000000001",
  sectionId: "failure-modes",
  text: "How does retry stop?",
  openedBy: "user-1",
  requestFingerprint: QUESTION_FINGERPRINT,
  state: "open",
  resolutionLink: null,
  resolvedAt: null,
};

function setup(
  result: Awaited<ReturnType<QuestionDocument["absorbAnswer"]>> = {
    changed: true,
    replayed: false,
    resolutionLink: "yjs-section://failure-modes/AQID",
  },
) {
  const resolveInputs: unknown[] = [];
  const store: OpenQuestionStore = {
    find: async () => question,
    countOpenBySection: async () => ({ [question.sectionId]: 1 }),
    create: async () => question,
    resolve: async (input) => {
      resolveInputs.push(input);
      return true;
    },
  };
  const document: QuestionDocument = {
    addQuestionMarker: async () => false,
    removeQuestionMarker: async () => {},
    absorbAnswer: async () => result,
  };
  const service = new OpenQuestionService({
    store,
    document,
    now: () => new Date("2026-08-09T12:00:00.000Z"),
  });
  return { service, resolveInputs };
}

describe("open questions", () => {
  test("rejects a non-UUID question key before it changes the document", async () => {
    const { service } = setup();

    await expect(
      service.open({
        questionId: "question-11",
        specId: question.specId,
        sectionId: question.sectionId,
        text: question.text,
        openedBy: question.openedBy,
        anchor: "yjs-section://failure-modes/AAEC",
      }),
    ).rejects.toMatchObject({ code: "question_id_invalid" });
  });

  test("opens a first-class question at its document anchor", async () => {
    const markerInputs: unknown[] = [];
    const createInputs: unknown[] = [];
    const store: OpenQuestionStore = {
      find: async () => null,
      countOpenBySection: async () => ({}),
      create: async (input) => {
        createInputs.push(input);
        return question;
      },
      resolve: async () => false,
    };
    const service = new OpenQuestionService({
      store,
      document: {
        addQuestionMarker: async (input) => {
          markerInputs.push(input);
          return true;
        },
        removeQuestionMarker: async () => {},
        absorbAnswer: async () => ({ changed: false, replayed: false, resolutionLink: null }),
      },
      now: () => new Date("2026-08-09T12:00:00.000Z"),
    });

    const created = await service.open({
      questionId: question.id,
      specId: question.specId,
      sectionId: question.sectionId,
      text: `  ${question.text}  `,
      openedBy: question.openedBy,
      anchor: "yjs-section://failure-modes/AAEC",
    });

    expect(created).toEqual(question);
    expect(markerInputs).toHaveLength(1);
    expect(createInputs).toEqual([
      {
        id: question.id,
        specId: question.specId,
        sectionId: question.sectionId,
        text: question.text,
        openedBy: question.openedBy,
        requestFingerprint: QUESTION_FINGERPRINT,
      },
    ]);
  });

  test("passes the expected document revision into the anchored edit", async () => {
    let expectedDocSeq: bigint | undefined;
    const store: OpenQuestionStore = {
      find: async () => null,
      countOpenBySection: async () => ({}),
      create: async (input) => ({
        ...question,
        requestFingerprint: input.requestFingerprint,
      }),
      resolve: async () => false,
    };
    const service = new OpenQuestionService({
      store,
      document: {
        addQuestionMarker: async (input) => {
          expectedDocSeq = input.expectedDocSeq;
          return true;
        },
        removeQuestionMarker: async () => {},
        absorbAnswer: async () => ({ changed: false, replayed: false, resolutionLink: null }),
      },
      now: () => new Date("2026-08-09T12:00:00.000Z"),
    });

    await service.open({
      questionId: question.id,
      specId: question.specId,
      sectionId: question.sectionId,
      text: question.text,
      openedBy: question.openedBy,
      anchor: "yjs-section://failure-modes/AAEC",
      expectedDocSeq: 7n,
    });

    expect(expectedDocSeq).toBe(7n);
  });

  test("rejects resolution without a document change", async () => {
    const { service, resolveInputs } = setup({
      changed: false,
      replayed: false,
      resolutionLink: null,
    });

    await expect(
      service.resolve({ questionId: question.id, answerMarkdown: "Use three tries." }),
    ).rejects.toBeInstanceOf(OpenQuestionError);
    expect(resolveInputs).toHaveLength(0);
  });

  test("records the relative-position link only after the answer lands", async () => {
    const { service, resolveInputs } = setup();

    const resolved = await service.resolve({
      questionId: question.id,
      answerMarkdown: "  Use three tries.  ",
    });

    expect(resolved).toMatchObject({
      state: "resolved",
      resolutionLink: "yjs-section://failure-modes/AQID",
    });
    expect(resolveInputs).toEqual([
      {
        id: question.id,
        expectedState: "open",
        resolutionLink: "yjs-section://failure-modes/AQID",
        resolvedAt: new Date("2026-08-09T12:00:00.000Z"),
      },
    ]);
  });

  test("finishes the row after a crash without a second document edit", async () => {
    let documentCalls = 0;
    const resolveInputs: unknown[] = [];
    const service = new OpenQuestionService({
      store: {
        find: async () => question,
        countOpenBySection: async () => ({ [question.sectionId]: 1 }),
        create: async () => question,
        resolve: async (input) => {
          resolveInputs.push(input);
          return true;
        },
      },
      document: {
        addQuestionMarker: async () => false,
        removeQuestionMarker: async () => {},
        absorbAnswer: async () => {
          documentCalls += 1;
          return {
            changed: false,
            replayed: true,
            resolutionLink: "yjs-section://failure-modes/retry",
          };
        },
      },
      now: () => new Date("2026-08-09T12:00:00.000Z"),
    });

    const resolved = await service.resolve({
      questionId: question.id,
      answerMarkdown: "Use three tries.",
    });

    expect(resolved.state).toBe("resolved");
    expect(documentCalls).toBe(1);
    expect(resolveInputs).toHaveLength(1);
  });

  test("replays a stable question after a marker-only crash", async () => {
    const markerFingerprints = new Map([[question.id, QUESTION_FINGERPRINT]]);
    let createCalls = 0;
    const service = new OpenQuestionService({
      store: {
        find: async () => null,
        countOpenBySection: async () => ({}),
        create: async () => {
          createCalls += 1;
          return question;
        },
        resolve: async () => false,
      },
      document: {
        addQuestionMarker: async ({ questionId, requestFingerprint }) => {
          const existing = markerFingerprints.get(questionId);
          if (existing && existing !== requestFingerprint) throw new Error("different request");
          if (existing) return false;
          markerFingerprints.set(questionId, requestFingerprint);
          return true;
        },
        removeQuestionMarker: async () => {},
        absorbAnswer: async () => ({ changed: false, replayed: false, resolutionLink: null }),
      },
      now: () => new Date("2026-08-09T12:00:00.000Z"),
    });

    const replay = await service.open({
      questionId: question.id,
      specId: question.specId,
      sectionId: question.sectionId,
      text: question.text,
      openedBy: question.openedBy,
      anchor: "yjs-section://failure-modes/AAEC",
    });

    expect(replay).toEqual(question);
    expect(createCalls).toBe(1);
    expect(markerFingerprints.size).toBe(1);
  });

  test("keeps the marker when create commits and then reports an error", async () => {
    let stored: OpenQuestionRecord | null = null;
    let removeCalls = 0;
    const service = new OpenQuestionService({
      store: {
        find: async () => stored,
        countOpenBySection: async () => ({}),
        create: async () => {
          stored = question;
          throw new Error("The response was lost after commit.");
        },
        resolve: async () => false,
      },
      document: {
        addQuestionMarker: async () => true,
        removeQuestionMarker: async () => {
          removeCalls += 1;
        },
        absorbAnswer: async () => ({ changed: false, replayed: false, resolutionLink: null }),
      },
      now: () => new Date("2026-08-09T12:00:00.000Z"),
    });

    const replay = await service.open({
      questionId: question.id,
      specId: question.specId,
      sectionId: question.sectionId,
      text: question.text,
      openedBy: question.openedBy,
      anchor: "yjs-section://failure-modes/AAEC",
    });

    expect(replay).toEqual(question);
    expect(removeCalls).toBe(0);
  });

  test("rejects one question ID for a different anchor", async () => {
    let removeCalls = 0;
    const service = new OpenQuestionService({
      store: {
        find: async () => question,
        countOpenBySection: async () => ({ [question.sectionId]: 1 }),
        create: async () => question,
        resolve: async () => false,
      },
      document: {
        addQuestionMarker: async () => true,
        removeQuestionMarker: async () => {
          removeCalls += 1;
        },
        absorbAnswer: async () => ({ changed: false, replayed: false, resolutionLink: null }),
      },
      now: () => new Date("2026-08-09T12:00:00.000Z"),
    });

    await expect(
      service.open({
        questionId: question.id,
        specId: question.specId,
        sectionId: question.sectionId,
        text: question.text,
        openedBy: question.openedBy,
        anchor: "yjs-section://failure-modes/different",
      }),
    ).rejects.toMatchObject({ code: "question_key_conflict" });
    expect(removeCalls).toBe(1);
  });

  test("rejects one question ID for different content", async () => {
    const service = new OpenQuestionService({
      store: {
        find: async () => question,
        countOpenBySection: async () => ({ [question.sectionId]: 1 }),
        create: async () => question,
        resolve: async () => false,
      },
      document: {
        addQuestionMarker: async () => false,
        removeQuestionMarker: async () => {
          throw new Error("A replay marker must not be removed.");
        },
        absorbAnswer: async () => ({ changed: false, replayed: false, resolutionLink: null }),
      },
      now: () => new Date("2026-08-09T12:00:00.000Z"),
    });

    await expect(
      service.open({
        questionId: question.id,
        specId: question.specId,
        sectionId: question.sectionId,
        text: "A different question?",
        openedBy: question.openedBy,
        anchor: "yjs-section://failure-modes/AAEC",
      }),
    ).rejects.toMatchObject({ code: "question_key_conflict" });
  });
});
