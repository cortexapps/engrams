import { describe, expect, test } from "bun:test";

import {
  compileIntegrationPolicy,
  connectorRegistry,
  parseConnector,
  resourceScopeError,
} from "../connectors/registry.ts";

function scopeError(
  provider: string,
  operation: string,
  opts: { resources?: string[]; userScoped?: boolean } = {},
): string | null {
  return resourceScopeError(
    {
      provider,
      operation,
      resourceConstraints: opts.resources ?? ["acme/api"],
      ...(opts.userScoped !== undefined ? { userScoped: opts.userScoped } : {}),
    },
    connectorRegistry(),
  );
}

function gitlabGlobs(resourceConstraints: string[]): string[] {
  const policy = compileIntegrationPolicy(
    [{ connectionId: "c-1", provider: "gitlab", operation: "repository:read", resourceConstraints }],
    connectorRegistry(),
  );
  return policy.injects.flatMap((inject) => inject.path_globs);
}

describe("a grant that names resources on a resource-aware connector", () => {
  test("reaches only the paths of its resources", () => {
    expect(gitlabGlobs(["acme/platform/api"]).sort()).toEqual([
      "/api/v4/projects/acme%2Fplatform%2Fapi/repository/*",
      // The token's own user: the same for every resource.
      "/api/v4/user",
      "path-query:/acme/platform/api.git/git-upload-pack",
      "path-query:/acme/platform/api.git/info/refs?service=git-upload-pack",
    ]);
  });

  test("with no resources reaches every repository", () => {
    expect(gitlabGlobs([])).toContain("path-query:/*.git/git-upload-pack");
  });

  test("an operation with no path for a resource cannot take one", () => {
    expect(scopeError("gitlab", "repository:read")).toBeNull();
    expect(scopeError("gitlab", "projects:read")).toBeNull();
    expect(scopeError("gitlab", "no_such:operation")).toContain("cannot be limited");
    // A minted credential carries the limit.
    expect(scopeError("github", "contents:read")).toBeNull();
    // A static key with no resource path: nothing would hold the session
    // to the resource.
    expect(scopeError("azure_devops", "code:write")).toContain("cannot be limited");
    expect(scopeError("jira", "issues:read")).toContain("cannot be limited");
    // A connector that the registry does not have.
    expect(scopeError("acme_custom", "things:read")).toContain("cannot be limited");
    expect(scopeError("gitlab", "repository:read", { resources: [] })).toBeNull();
  });

  test("a person's own token is not held to a resource by a mint connector", () => {
    expect(scopeError("github", "contents:read", { userScoped: true })).toContain("own credential");
    const policy = compileIntegrationPolicy(
      [
        {
          connectionId: "c-1",
          provider: "github",
          operation: "contents:read",
          resourceConstraints: ["acme/api"],
          userScoped: true,
        },
      ],
      connectorRegistry(),
      { userSubjectId: "u-1" },
    );
    expect(policy.injects).toEqual([]);
  });

  test("the save check and the compile refuse the same resources", () => {
    // A project id and a group are not a project path; the rest are not names.
    for (const resource of ["12345", "acme", "*", "../other", "acme/-x", "acme/a b"]) {
      expect(scopeError("gitlab", "repository:read", { resources: [resource] })).toContain(
        "is not a resource",
      );
      // An empty list of globs means every path, so no inject is the refusal.
      expect(gitlabGlobs(["acme/api", resource])).toEqual([]);
    }
  });

  test("a custom connector with a static key takes no resource", () => {
    const custom = parseConnector(
      {
        provider: "acme_custom",
        protocol: "http",
        credential: { source: "inject", injects: [{ header: "Authorization", secretRef: "ACME_KEY" }] },
        hosts: ["api.acme.example"],
        operations: [{ grants: ["things:read"], match: { method: "GET", path: "/things/*" } }],
      },
      "acme_custom",
    );
    const registry = new Map([...connectorRegistry(), [custom.provider, custom]]);
    const grant = {
      connectionId: "c-1",
      provider: "acme_custom",
      operation: "things:read",
      resourceConstraints: ["only-this"],
    };
    expect(resourceScopeError(grant, registry)).toContain("cannot be limited");
    expect(compileIntegrationPolicy([grant], registry).injects).toEqual([]);
  });
});
