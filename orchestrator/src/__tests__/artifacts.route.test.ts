/**
 * Artifact byte route + hardened headers (ADR 0026).
 *
 * The serve headers — not the upload gate — make attacker-controlled bytes
 * safe to serve. Asserts:
 *   - the header matrix per media type (sandbox CSP, nosniff, no-store,
 *     inline vs attachment)
 *   - the route applies the matrix, streams chunks, and keeps the
 *     401/404 guard semantics
 */

import { expect, test, describe } from "bun:test";

import { artifactResponseHeaders } from "../routes/artifact-headers.ts";
import { makeArtifactsRoute } from "../routes/artifacts.ts";
import type { GetArtifactResponse, SessionsClient } from "../routes/artifacts.ts";
import { ConnectError, Code } from "@connectrpc/connect";

// ---------------------------------------------------------------------------
// artifactResponseHeaders matrix
// ---------------------------------------------------------------------------

const ACTIVE_SANDBOX =
  "sandbox allow-scripts allow-forms allow-modals allow-popups allow-downloads";

describe("artifactResponseHeaders", () => {
  test("html renders inline with the scripted sandbox", () => {
    const h = artifactResponseHeaders("text/html", "report.html");
    expect(h["Content-Type"]).toBe("text/html");
    expect(h["Content-Security-Policy"]).toBe(ACTIVE_SANDBOX);
    expect(h["Content-Disposition"]).toBe('inline; filename="report.html"');
    expect(h["X-Content-Type-Options"]).toBe("nosniff");
    expect(h["Cache-Control"]).toBe("private, no-store");
  });

  test("svg renders inline with the scripted sandbox", () => {
    const h = artifactResponseHeaders("image/svg+xml");
    expect(h["Content-Security-Policy"]).toBe(ACTIVE_SANDBOX);
    expect(h["Content-Disposition"]).toBe("inline");
  });

  for (const mt of [
    "image/png",
    "video/mp4",
    "audio/mpeg",
    "text/markdown",
    "text/plain",
    "text/csv",
    "application/json",
    "application/xml",
  ]) {
    test(`${mt} renders inline with the inert sandbox`, () => {
      const h = artifactResponseHeaders(mt);
      expect(h["Content-Security-Policy"]).toBe("sandbox");
      expect(h["Content-Disposition"]).toBe("inline");
    });
  }

  for (const mt of ["application/pdf", "application/octet-stream", "font/woff2"]) {
    test(`${mt} is served as attachment`, () => {
      const h = artifactResponseHeaders(mt, "blob.bin");
      expect(h["Content-Security-Policy"]).toBe("sandbox");
      expect(h["Content-Disposition"]).toBe('attachment; filename="blob.bin"');
    });
  }

  test("empty media type falls back to octet-stream attachment", () => {
    const h = artifactResponseHeaders("");
    expect(h["Content-Type"]).toBe("application/octet-stream");
    expect(h["Content-Disposition"]).toBe("attachment");
  });

  test("filename is sanitised to printable ASCII without quotes", () => {
    const h = artifactResponseHeaders("text/html", 'ев"il\n.html');
    expect(h["Content-Disposition"]).toBe('inline; filename="___il_.html"');
  });
});

// ---------------------------------------------------------------------------
// Route behavior
// ---------------------------------------------------------------------------

function fakeSessions(
  mediaType: string,
  fileName: string,
  chunks: Uint8Array[],
): SessionsClient {
  return {
    async *getArtifact(): AsyncIterable<GetArtifactResponse> {
      yield {
        msg: {
          case: "metadata",
          value: {
            mediaType,
            sizeBytes: BigInt(chunks.reduce((n, c) => n + c.length, 0)),
            fileName,
          },
        },
      };
      for (const c of chunks) {
        yield { msg: { case: "chunk", value: c } };
      }
    },
  };
}

const notFoundSessions: SessionsClient = {
  // eslint-disable-next-line require-yield
  async *getArtifact(): AsyncIterable<GetArtifactResponse> {
    throw new ConnectError("artifact not found", Code.NotFound);
  },
};

const owner = async () => ({ user: { id: "u1" } });
const anon = async () => null;
const ownedBy = (id: string) => async () => id;

const PATH = "/api/v1/sessions/s1/artifacts/a1";

describe("artifact byte route", () => {
  test("401 when unauthenticated", async () => {
    const app = makeArtifactsRoute({
      sessions: fakeSessions("text/html", "x.html", []),
      getSession: anon,
      resolveOwner: ownedBy("u1"),
    });
    const res = await app.request(PATH);
    expect(res.status).toBe(401);
  });

  test("404 when the session belongs to another user", async () => {
    const app = makeArtifactsRoute({
      sessions: fakeSessions("text/html", "x.html", []),
      getSession: owner,
      resolveOwner: ownedBy("someone-else"),
    });
    const res = await app.request(PATH);
    expect(res.status).toBe(404);
  });

  test("404 when the artifact is unknown upstream", async () => {
    const app = makeArtifactsRoute({
      sessions: notFoundSessions,
      getSession: owner,
      resolveOwner: ownedBy("u1"),
    });
    const res = await app.request(PATH);
    expect(res.status).toBe(404);
  });

  test("streams bytes with the hardened header set", async () => {
    const chunks = [new Uint8Array([60, 104, 49, 62]), new Uint8Array([104, 105])];
    const app = makeArtifactsRoute({
      sessions: fakeSessions("text/html", "page.html", chunks),
      getSession: owner,
      resolveOwner: ownedBy("u1"),
    });
    const res = await app.request(PATH);
    expect(res.status).toBe(200);
    expect(res.headers.get("content-type")).toBe("text/html");
    expect(res.headers.get("x-content-type-options")).toBe("nosniff");
    expect(res.headers.get("content-security-policy")).toBe(ACTIVE_SANDBOX);
    expect(res.headers.get("cache-control")).toBe("private, no-store");
    expect(res.headers.get("content-disposition")).toBe(
      'inline; filename="page.html"',
    );
    expect(res.headers.get("content-length")).toBe("6");
    expect(await res.text()).toBe("<h1>hi");
  });

  test("unknown binary is served as attachment", async () => {
    const app = makeArtifactsRoute({
      sessions: fakeSessions("application/octet-stream", "tool.bin", [
        new Uint8Array([1, 2, 3]),
      ]),
      getSession: owner,
      resolveOwner: ownedBy("u1"),
    });
    const res = await app.request(PATH);
    expect(res.status).toBe(200);
    expect(res.headers.get("content-disposition")).toBe(
      'attachment; filename="tool.bin"',
    );
    expect(res.headers.get("content-security-policy")).toBe("sandbox");
  });
});
