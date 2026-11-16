import { describe, expect, test } from "bun:test";

import {
  compileIntegrationPolicy,
  connectorRegistry,
  resourceScopeError,
} from "../connectors/registry.ts";

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
    expect(resourceScopeError("gitlab", "repository:read")).toBeNull();
    expect(resourceScopeError("gitlab", "projects:read")).toBeNull();
    expect(resourceScopeError("gitlab", "no_such:operation")).toContain("cannot be limited");
    // A connector with no resource paths: its own engine reads the resource.
    expect(resourceScopeError("github", "contents:read")).toBeNull();
  });

  test("reaches nothing when a resource is not a repository path", () => {
    // An empty list of globs means every path, so no inject is the refusal.
    expect(gitlabGlobs(["*"])).toEqual([]);
    expect(gitlabGlobs(["acme/api", "../other"])).toEqual([]);
  });
});
