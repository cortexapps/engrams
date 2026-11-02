import { createHash } from "node:crypto";
import type { IncomingMessage, ServerResponse } from "node:http";

interface CodeGrant {
  challenge: string;
  client: string;
  redirect: string;
  resource: string;
  scope: string;
}
interface RefreshGrant {
  client: string;
  scope: string;
}

// A deterministic authorization server for hostile-boundary tests, not a provider driver.
export function createOAuthFixture(
  origin: () => string,
  callbackIssuer?: string,
  advertised?: {
    issuer: string;
    authorization: string;
    token: string;
    redirectUri: string;
  },
) {
  const codes = new Map<string, CodeGrant>();
  const states = new Set<string>();
  const refreshes = new Map<string, RefreshGrant>();
  let codeSequence = 0;
  let tokenSequence = 0;
  let accessToken: string | undefined;
  const resource = () => `${origin()}/mcp`;
  const issuer = () => advertised?.issuer ?? origin();
  const redirectUri = () => advertised?.redirectUri ?? `${origin()}/callback`;
  let account = "account-one";
  let scopeOverride: string | undefined;
  let authorizations = 0;
  let exchanges = 0;
  let rotations = 0;
  let rejections = 0;
  const respond = (res: ServerResponse, value: unknown, status = 200) => {
    res.writeHead(status, {
      "content-type": "application/json",
      "cache-control": "no-store",
    });
    res.end(JSON.stringify(value));
  };
  const fail = (res: ServerResponse, error = "invalid_request") => {
    ++rejections;
    respond(res, { error }, 400);
  };
  return {
    observe() {
      return {
        account,
        scope: scopeOverride ?? null,
        authorizations,
        exchanges,
        rotations,
        rejections,
      };
    },
    configure(value: {
      account?: string;
      scope?: string;
      issuer?: "configured" | "mismatch";
    }) {
      if (value.account !== undefined) account = value.account;
      if (value.scope !== undefined) scopeOverride = value.scope;
      if (value.issuer !== undefined)
        callbackIssuer =
          value.issuer === "mismatch" ? "https://other.example" : undefined;
    },
    accepts(authorization: string | undefined) {
      return (
        accessToken !== undefined && authorization === `Bearer ${accessToken}`
      );
    },
    handle(
      req: IncomingMessage,
      res: ServerResponse,
      url: URL,
      body: string,
    ): boolean {
      if (
        url.pathname === "/.well-known/oauth-protected-resource" ||
        url.pathname === "/.well-known/oauth-protected-resource/mcp"
      ) {
        respond(res, {
          resource: resource(),
          authorization_servers: [issuer()],
          scopes_supported: ["tools:read", "tools:write"],
          bearer_methods_supported: ["header"],
        });
        return true;
      }
      if (url.pathname === "/.well-known/oauth-authorization-server") {
        respond(res, {
          issuer: issuer(),
          authorization_endpoint: `${advertised?.authorization ?? origin()}/authorize`,
          token_endpoint: `${advertised?.token ?? origin()}/token`,
          response_types_supported: ["code"],
          grant_types_supported: ["authorization_code", "refresh_token"],
          token_endpoint_auth_methods_supported: ["none", "client_secret_post"],
          code_challenge_methods_supported: ["S256"],
          authorization_response_iss_parameter_supported: true,
          scopes_supported: ["tools:read", "tools:write"],
        });
        return true;
      }
      if (url.pathname === "/authorize") {
        const p = url.searchParams;
        const client = p.get("client_id");
        const state = p.get("state");
        const challenge = p.get("code_challenge");
        const scope = p.get("scope") ?? "tools:read";
        if (
          req.method !== "GET" ||
          p.get("response_type") !== "code" ||
          !["fixture-public", "fixture-confidential"].includes(client ?? "") ||
          p.get("redirect_uri") !== redirectUri() ||
          p.get("resource") !== resource() ||
          p.get("code_challenge_method") !== "S256" ||
          !challenge ||
          !/^[A-Za-z0-9_-]{43}$/.test(challenge) ||
          !state ||
          states.has(state) ||
          !validScope(scope)
        ) {
          fail(res);
          return true;
        }
        if (codes.size >= 128 || states.size >= 1024) {
          fail(res);
          return true;
        }
        ++authorizations;
        const code = `fixture-code-${++codeSequence}`;
        states.add(state);
        codes.set(code, {
          client: client!,
          challenge,
          redirect: redirectUri(),
          resource: resource(),
          scope,
        });
        const callback = new URL(redirectUri());
        callback.search = new URLSearchParams({
          code,
          state,
          iss: callbackIssuer ?? issuer(),
        }).toString();
        res.writeHead(302, {
          location: callback.href,
          "cache-control": "no-store",
        });
        res.end();
        return true;
      }
      if (url.pathname !== "/token") return false;
      if (req.method !== "POST") {
        fail(res);
        return true;
      }
      const p = new URLSearchParams(body);
      const client = p.get("client_id");
      if (
        !["fixture-public", "fixture-confidential"].includes(client ?? "") ||
        (client === "fixture-confidential" &&
          p.get("client_secret") !== "fixture-client-secret") ||
        p.get("resource") !== resource()
      ) {
        fail(res);
        return true;
      }
      let scope: string;
      if (p.get("grant_type") === "authorization_code") {
        const code = p.get("code") ?? "";
        const grant = codes.get(code);
        const challenge = createHash("sha256")
          .update(p.get("code_verifier") ?? "")
          .digest("base64url");
        if (
          !grant ||
          grant.client !== client ||
          grant.redirect !== p.get("redirect_uri") ||
          grant.resource !== p.get("resource") ||
          grant.challenge !== challenge
        ) {
          fail(res, "invalid_grant");
          return true;
        }
        codes.delete(code);
        ++exchanges;
        scope = grant.scope;
      } else if (p.get("grant_type") === "refresh_token") {
        const refresh = p.get("refresh_token") ?? "";
        const grant = refreshes.get(refresh);
        if (
          !grant ||
          grant.client !== client ||
          (p.has("scope") && !isScopeSubset(p.get("scope")!, grant.scope))
        ) {
          fail(res, "invalid_grant");
          return true;
        }
        refreshes.delete(refresh);
        ++rotations;
        scope = p.get("scope") ?? grant.scope;
      } else {
        fail(res, "unsupported_grant_type");
        return true;
      }
      if (refreshes.size >= 128) {
        fail(res);
        return true;
      }
      scope = scopeOverride ?? scope;
      ++tokenSequence;
      accessToken = `fixture-access-${tokenSequence}`;
      const refreshToken = `fixture-refresh-${tokenSequence}`;
      refreshes.set(refreshToken, { client: client!, scope });
      respond(res, {
        access_token: accessToken,
        refresh_token: refreshToken,
        token_type: "Bearer",
        expires_in: 3600,
        scope,
        ...(advertised ? { fixture_account: account } : {}),
      });
      return true;
    },
  };
}
function validScope(scope: string) {
  return (
    scope.length > 0 &&
    scope
      .split(" ")
      .every((part) => ["tools:read", "tools:write"].includes(part))
  );
}
function isScopeSubset(scope: string, previous: string) {
  return (
    validScope(scope) &&
    scope.split(" ").every((part) => previous.split(" ").includes(part))
  );
}
