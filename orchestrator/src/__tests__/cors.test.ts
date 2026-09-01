/**
 * The split-host CORS gate (src/auth/cors.ts).
 *
 * The contract under test:
 *  - a trusted Origin gets the credentialed approval headers, echoed exactly;
 *  - an untrusted Origin gets NO CORS headers (fail closed by omission);
 *  - a preflight (OPTIONS + Access-Control-Request-Method) is answered 204
 *    without reaching the rest of the dispatch — trusted or not — because
 *    preflights carry no cookie and no IAP assertion, and the fail-closed
 *    bridge would 401 them;
 *  - a same-origin request (no Origin header) is untouched;
 *  - the WS origin policy admits no-Origin (CLI), same-host, and trusted
 *    origins, and refuses everything else.
 */

import { describe, expect, test } from "bun:test";
import type { IncomingMessage, ServerResponse } from "node:http";
import { applyCors, wsOriginAllowed } from "../auth/cors.ts";

const TRUSTED = ["https://app.example.com"];

function fakeReq(headers: Record<string, string>, method = "GET"): IncomingMessage {
  return { method, headers } as unknown as IncomingMessage;
}

function fakeRes(): ServerResponse & {
  headers: Record<string, string>;
  ended: boolean;
} {
  const headers: Record<string, string> = {};
  const res = {
    headers,
    ended: false,
    statusCode: 200,
    setHeader(name: string, value: string) {
      headers[name.toLowerCase()] = value;
    },
    end() {
      (res as { ended: boolean }).ended = true;
    },
  };
  return res as unknown as ServerResponse & { headers: Record<string, string>; ended: boolean };
}

describe("applyCors", () => {
  test("no Origin header: untouched, dispatch continues", () => {
    const res = fakeRes();
    expect(applyCors(fakeReq({}), res, TRUSTED)).toBe(false);
    expect(Object.keys(res.headers)).toHaveLength(0);
  });

  test("trusted Origin on a real request: approval headers, dispatch continues", () => {
    const res = fakeRes();
    const handled = applyCors(fakeReq({ origin: "https://app.example.com" }), res, TRUSTED);
    expect(handled).toBe(false);
    expect(res.headers["access-control-allow-origin"]).toBe("https://app.example.com");
    expect(res.headers["access-control-allow-credentials"]).toBe("true");
    expect(res.headers["vary"]).toBe("Origin");
  });

  test("untrusted Origin on a real request: no CORS headers, dispatch continues", () => {
    const res = fakeRes();
    const handled = applyCors(fakeReq({ origin: "https://evil.example.net" }), res, TRUSTED);
    expect(handled).toBe(false);
    expect(res.headers["access-control-allow-origin"]).toBeUndefined();
    expect(res.headers["access-control-allow-credentials"]).toBeUndefined();
  });

  test("trusted preflight: 204 with methods + echoed request headers, fully handled", () => {
    const res = fakeRes();
    const handled = applyCors(
      fakeReq(
        {
          origin: "https://app.example.com",
          "access-control-request-method": "POST",
          "access-control-request-headers": "content-type,connect-protocol-version",
        },
        "OPTIONS",
      ),
      res,
      TRUSTED,
    );
    expect(handled).toBe(true);
    expect(res.ended).toBe(true);
    expect(res.statusCode).toBe(204);
    expect(res.headers["access-control-allow-origin"]).toBe("https://app.example.com");
    expect(res.headers["access-control-allow-headers"]).toBe(
      "content-type,connect-protocol-version",
    );
    expect(res.headers["access-control-allow-methods"]).toContain("POST");
  });

  test("untrusted preflight: 204 with NO approval headers, fully handled", () => {
    const res = fakeRes();
    const handled = applyCors(
      fakeReq(
        { origin: "https://evil.example.net", "access-control-request-method": "POST" },
        "OPTIONS",
      ),
      res,
      TRUSTED,
    );
    expect(handled).toBe(true);
    expect(res.ended).toBe(true);
    expect(res.headers["access-control-allow-origin"]).toBeUndefined();
  });

  test("plain OPTIONS without Access-Control-Request-Method is NOT a preflight", () => {
    const res = fakeRes();
    const handled = applyCors(fakeReq({ origin: "https://app.example.com" }, "OPTIONS"), res, TRUSTED);
    expect(handled).toBe(false);
    expect(res.ended).toBe(false);
  });
});

describe("wsOriginAllowed", () => {
  test("no Origin (CLI / non-browser client): allowed", () => {
    expect(wsOriginAllowed(fakeReq({ host: "api.example.com" }), TRUSTED)).toBe(true);
  });

  test("trusted origin: allowed", () => {
    expect(
      wsOriginAllowed(
        fakeReq({ host: "api.example.com", origin: "https://app.example.com" }),
        TRUSTED,
      ),
    ).toBe(true);
  });

  test("same-host origin: allowed", () => {
    expect(
      wsOriginAllowed(
        fakeReq({ host: "api.example.com", origin: "https://api.example.com" }),
        TRUSTED,
      ),
    ).toBe(true);
  });

  test("cross-site origin: refused (CSWSH — the socket would carry the cookie)", () => {
    expect(
      wsOriginAllowed(
        fakeReq({ host: "api.example.com", origin: "https://evil.example.net" }),
        TRUSTED,
      ),
    ).toBe(false);
  });

  test("a sibling preview host is NOT same-host and NOT trusted: refused", () => {
    expect(
      wsOriginAllowed(
        fakeReq({ host: "api.example.com", origin: "https://x.preview.example.com" }),
        TRUSTED,
      ),
    ).toBe(false);
  });

  test("unparseable origin: refused", () => {
    expect(wsOriginAllowed(fakeReq({ host: "api.example.com", origin: "::::" }), TRUSTED)).toBe(
      false,
    );
  });
});
