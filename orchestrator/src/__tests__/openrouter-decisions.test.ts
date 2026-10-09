import { describe, expect, test } from "bun:test";

import {
  DECISIONS_URL,
  DecisionsApiError,
  makeDecisionsClient,
  type DecisionsRequest,
} from "../integrations/openrouter-decisions.ts";

const REQUEST: DecisionsRequest = {
  model: "typesafe/jev-1.13",
  state: "the build is red on main",
  questions: { urgent: { type: "noul", instructions: "Is this urgent?" } },
};

function clientReturning(status: number, body: string, seen?: Request[]) {
  return makeDecisionsClient({
    apiKey: "k-test",
    fetch: async (url, init) => {
      seen?.push(new Request(url, init));
      return new Response(body, { status });
    },
  });
}

async function failure(promise: Promise<unknown>): Promise<DecisionsApiError> {
  try {
    await promise;
  } catch (error) {
    expect(error).toBeInstanceOf(DecisionsApiError);
    return error as DecisionsApiError;
  }
  throw new Error("expected a failure");
}

describe("decisions client", () => {
  test("posts the request with the bearer key and parses the answers", async () => {
    const seen: Request[] = [];
    const client = clientReturning(
      200,
      JSON.stringify({
        id: "gen-dec-1",
        model: "typesafe/jev-1.13-20260917",
        answers: { urgent: { type: "noul", noul: 0.91 } },
        usage: { input_tokens: 40, output_tokens: 0, cost: 0.0000017 },
      }),
      seen,
    );
    const res = await client.decide(REQUEST);
    expect(res.answers["urgent"]).toEqual({ type: "noul", noul: 0.91 });
    expect(res.usage.cost).toBe(0.0000017);
    expect(seen[0]!.url).toBe(DECISIONS_URL);
    expect(seen[0]!.headers.get("authorization")).toBe("Bearer k-test");
    expect(await seen[0]!.json()).toEqual(REQUEST);
  });

  test("rate limits and provider failures are retryable", async () => {
    for (const status of [429, 502, 503, 524, 529]) {
      const error = await failure(clientReturning(status, "{}").decide(REQUEST));
      expect(error.status).toBe(status);
      expect(error.retryable).toBe(true);
    }
  });

  test("a bad request, a bad key or no credits are not retryable", async () => {
    for (const status of [400, 401, 402, 413]) {
      const body = JSON.stringify({ error: { code: status, message: `nope ${status}` } });
      const error = await failure(clientReturning(status, body).decide(REQUEST));
      expect(error.retryable).toBe(false);
      expect(error.message).toContain(`nope ${status}`);
    }
  });

  test("a 200 with an HTML body is a retryable upstream fault", async () => {
    const error = await failure(clientReturning(200, "<html>fallback</html>").decide(REQUEST));
    expect(error.retryable).toBe(true);
  });

  test("a response that breaks the schema is not retried", async () => {
    const error = await failure(
      clientReturning(200, JSON.stringify({ model: "m", answers: {} })).decide(REQUEST),
    );
    expect(error.retryable).toBe(false);
    expect(error.message).toContain("usage");
  });

  test("a network failure is retryable", async () => {
    const client = makeDecisionsClient({
      apiKey: "k",
      fetch: async () => {
        throw new TypeError("connection reset");
      },
    });
    const error = await failure(client.decide(REQUEST));
    expect(error.status).toBeNull();
    expect(error.retryable).toBe(true);
  });
});
