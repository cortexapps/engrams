import { Hono, type Context } from "hono";
import { HTTPException } from "hono/http-exception";

import { SpecDocumentReadOnlyError } from "../specs/doc-service.ts";
import { OpenQuestionError, type OpenQuestionService } from "../specs/open-questions.ts";
import type { GetSession, ResolveSpecMembership } from "./guard.ts";
import { makeSpecMemberHeaderGuard } from "./guard.ts";

const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[1-5][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/i;
const MAX_ANSWER_CHARS = 20_000;

export interface SpecQuestionsRouteDeps {
  questions: Pick<OpenQuestionService, "resolve" | "dismiss">;
  resolveMembership: ResolveSpecMembership;
  getSession?: GetSession;
}

/**
 * The human exits for an open question (the agent has spec_* tools; a person
 * had NOTHING — questions could only pile up until publish acknowledged them).
 *
 * Resolve absorbs the answer into the document at the question's anchor, the
 * same invariant the agent's resolve tool holds. Dismiss closes a question
 * that no longer applies, which is also the only way out for a row whose
 * marker predates rewrite-preservation.
 */
export function makeSpecQuestionsRoute(deps: SpecQuestionsRouteDeps): Hono {
  const app = new Hono();
  const authorize = makeSpecMemberHeaderGuard(deps.resolveMembership, deps.getSession);

  async function requireMember(
    c: Context,
  ): Promise<{ specId: string; questionId: string; userId: string }> {
    const specId = c.req.param("id");
    const questionId = c.req.param("questionId");
    if (
      typeof specId !== "string" ||
      !UUID.test(specId) ||
      typeof questionId !== "string" ||
      !UUID.test(questionId)
    ) {
      throw new HTTPException(404, { message: "not found" });
    }
    const result = await authorize(c.req.raw.headers, specId);
    if (!result.ok) {
      throw new HTTPException(result.status, {
        message: result.status === 401 ? "unauthenticated" : "not found",
      });
    }
    return { specId, questionId, userId: result.user.id };
  }

  app.post("/api/v1/specs/:id/questions/:questionId/resolve", async (c) => {
    const { specId, questionId, userId } = await requireMember(c);
    const body = await readJsonObject(c);
    const answer = body["answer"];
    if (typeof answer !== "string" || answer.trim().length === 0) {
      throw new HTTPException(400, { message: "answer must not be empty" });
    }
    if (answer.length > MAX_ANSWER_CHARS) {
      throw new HTTPException(413, { message: `answer exceeds ${MAX_ANSWER_CHARS} characters` });
    }
    const question = await run(specId, () =>
      deps.questions.resolve({ questionId, answerMarkdown: answer, resolvedBy: userId }),
    );
    return c.json({ question: questionJson(question) });
  });

  app.post("/api/v1/specs/:id/questions/:questionId/dismiss", async (c) => {
    const { specId, questionId, userId } = await requireMember(c);
    const question = await run(specId, () =>
      deps.questions.dismiss({ questionId, resolvedBy: userId }),
    );
    return c.json({ question: questionJson(question) });
  });

  return app;
}

async function run<T extends { specId: string }>(specId: string, action: () => Promise<T>) {
  let result: T;
  try {
    result = await action();
  } catch (error) {
    throw questionHttpError(error);
  }
  // The membership guard authorizes the spec in the URL; the question row must
  // belong to that same spec, or the id grants reach into another one.
  if (result.specId !== specId) throw new HTTPException(404, { message: "not found" });
  return result;
}

async function readJsonObject(c: Context): Promise<Record<string, unknown>> {
  let value: unknown;
  try {
    value = await c.req.json();
  } catch {
    throw new HTTPException(400, { message: "invalid JSON body" });
  }
  if (value === null || typeof value !== "object" || Array.isArray(value)) {
    throw new HTTPException(400, { message: "body must be an object" });
  }
  return value as Record<string, unknown>;
}

function questionHttpError(error: unknown): unknown {
  if (error instanceof SpecDocumentReadOnlyError) {
    return new HTTPException(409, { message: "published specs are read-only" });
  }
  if (!(error instanceof OpenQuestionError)) return error;
  switch (error.code) {
    case "question_not_found":
      return new HTTPException(404, { message: "not found" });
    case "question_not_open":
    case "stale_question":
    case "document_change_required":
      return new HTTPException(409, { message: error.message });
    default:
      return new HTTPException(400, { message: error.message });
  }
}

function questionJson(question: {
  id: string;
  sectionId: string;
  text: string;
  state: string;
  resolvedBy: string | null;
}) {
  return {
    id: question.id,
    sectionId: question.sectionId,
    text: question.text,
    state: question.state,
    resolvedBy: question.resolvedBy,
  };
}
