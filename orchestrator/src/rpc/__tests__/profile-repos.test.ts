/**
 * Pure half of repo autodiscovery: remote-URL parsing, write validation, and
 * the discovery line-protocol parser (REPO/REMOTE, `git remote -v` shaped).
 */

import { describe, expect, test } from "bun:test";

import {
  MAX_PROFILE_REPOS,
  normalizeRepos,
  parseDiscoverOutput,
  parseGitRemote,
  stripRemoteCredentials,
} from "../profile-repos.ts";

describe("parseGitRemote", () => {
  test("https, ssh, and scp-style GitHub remotes parse to one identity", () => {
    const expected = { host: "github.com", owner: "cortexapps", name: "engrams" };
    expect(parseGitRemote("https://github.com/cortexapps/engrams.git")).toEqual(expected);
    expect(parseGitRemote("https://github.com/cortexapps/engrams")).toEqual(expected);
    expect(parseGitRemote("git@github.com:cortexapps/engrams.git")).toEqual(expected);
    expect(parseGitRemote("ssh://git@github.com/cortexapps/engrams.git")).toEqual(expected);
    expect(parseGitRemote("ssh://git@github.com:2222/cortexapps/engrams")).toEqual(expected);
  });

  test("GitLab subgroup paths keep every segment but the last as owner", () => {
    expect(parseGitRemote("https://gitlab.com/group/subgroup/tool.git")).toEqual({
      host: "gitlab.com",
      owner: "group/subgroup",
      name: "tool",
    });
  });

  test("hosts normalize to lowercase", () => {
    expect(parseGitRemote("https://GitHub.com/a/b")?.host).toBe("github.com");
  });

  test("non-forge and malformed URLs parse to null", () => {
    expect(parseGitRemote("")).toBeNull();
    expect(parseGitRemote("   ")).toBeNull();
    expect(parseGitRemote("/local/bare/repo.git")).toBeNull();
    expect(parseGitRemote("https://example.com/single")).toBeNull();
    expect(parseGitRemote("not a url")).toBeNull();
  });
});

describe("stripRemoteCredentials", () => {
  test("http(s) userinfo strips whether token-as-user or user:pass", () => {
    expect(stripRemoteCredentials("https://ghp_secret@github.com/o/r.git")).toBe(
      "https://github.com/o/r.git",
    );
    expect(
      stripRemoteCredentials("https://x-access-token:ghp_secret@github.com/o/r.git"),
    ).toBe("https://github.com/o/r.git");
  });

  test("structural ssh/scp identities survive; ssh passwords strip", () => {
    expect(stripRemoteCredentials("git@github.com:o/r.git")).toBe("git@github.com:o/r.git");
    expect(stripRemoteCredentials("ssh://git@github.com/o/r.git")).toBe(
      "ssh://git@github.com/o/r.git",
    );
    expect(stripRemoteCredentials("ssh://user:pass@github.com/o/r.git")).toBe(
      "ssh://github.com/o/r.git",
    );
  });
});

describe("normalizeRepos", () => {
  test("scrubs credential-bearing remote URLs before persisting", () => {
    const [repo] = normalizeRepos([
      { path: "/w/r", remoteUrl: "https://x-access-token:ghp_secret@github.com/o/r.git" },
    ]);
    expect(repo.remoteUrl).toBe("https://github.com/o/r.git");
    expect(JSON.stringify(repo)).not.toContain("ghp_secret");
    expect(repo.remote).toEqual({ host: "github.com", owner: "o", name: "r" });
  });

  test("trims, re-parses the remote server-side, and dedupes by path", () => {
    const out = normalizeRepos([
      { path: " /workspace/engrams ", remoteUrl: " git@github.com:cortexapps/engrams.git " },
      { path: "/workspace/engrams", remoteUrl: "https://elsewhere.example/x/y" }, // dup path dropped
      { path: "/workspace/notes", remoteUrl: "" },
    ]);
    expect(out).toEqual([
      {
        path: "/workspace/engrams",
        remoteUrl: "git@github.com:cortexapps/engrams.git",
        remote: { host: "github.com", owner: "cortexapps", name: "engrams" },
      },
      { path: "/workspace/notes", remoteUrl: "", remote: null },
    ]);
  });

  test("rejects an empty path and oversized inputs", () => {
    expect(() => normalizeRepos([{ path: "", remoteUrl: "" }])).toThrow("path is required");
    expect(() => normalizeRepos([{ path: "x".repeat(301), remoteUrl: "" }])).toThrow("path");
    expect(() =>
      normalizeRepos([{ path: "/a", remoteUrl: "h".repeat(501) }]),
    ).toThrow("remote URL");
    expect(() =>
      normalizeRepos(Array.from({ length: MAX_PROFILE_REPOS + 1 }, (_, i) => ({
        path: `/r${i}`,
        remoteUrl: "",
      }))),
    ).toThrow("at most");
  });
});

describe("parseDiscoverOutput", () => {
  test("parses checkouts with deduped remotes (fetch/push collapse)", () => {
    const stdout = [
      "REPO /workspace/engrams",
      "REMOTE origin\thttps://github.com/cortexapps/engrams.git (fetch)",
      "REMOTE origin\thttps://github.com/cortexapps/engrams.git (push)",
      "REMOTE upstream\tgit@github.com:upstream/engrams.git (fetch)",
      "REPO /workspace/bare-notes",
      "REPO /root/tool",
      "REMOTE origin\thttps://internal.example/x (fetch)",
    ].join("\n");
    expect(parseDiscoverOutput(stdout)).toEqual([
      {
        path: "/workspace/engrams",
        remotes: [
          {
            name: "origin",
            url: "https://github.com/cortexapps/engrams.git",
            parsed: { host: "github.com", owner: "cortexapps", name: "engrams" },
          },
          {
            name: "upstream",
            url: "git@github.com:upstream/engrams.git",
            parsed: { host: "github.com", owner: "upstream", name: "engrams" },
          },
        ],
      },
      { path: "/workspace/bare-notes", remotes: [] },
      {
        path: "/root/tool",
        remotes: [{ name: "origin", url: "https://internal.example/x", parsed: null }],
      },
    ]);
  });

  test("scrubs credential-bearing remotes from the discover response", () => {
    const out = parseDiscoverOutput(
      "REPO /w/r\nREMOTE origin\thttps://oauth2:glpat_secret@gitlab.com/g/p.git (fetch)",
    );
    expect(out[0].remotes[0].url).toBe("https://gitlab.com/g/p.git");
    expect(JSON.stringify(out)).not.toContain("glpat_secret");
  });

  test("is tolerant: junk lines, orphan remotes, and empty output", () => {
    expect(parseDiscoverOutput("")).toEqual([]);
    expect(
      parseDiscoverOutput(
        "REMOTE origin\thttps://x.example/a/b (fetch)\nnoise\nREPO \nfind: banner",
      ),
    ).toEqual([]);
  });
});
