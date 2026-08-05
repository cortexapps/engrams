/**
 * Profile repo config + autodiscovery — the pure half.
 *
 * `profile.repos` is explicit, user-managed config: which git checkouts the
 * profile's image contains. This module owns everything deterministic about
 * it: remote-URL parsing (shared by the write path and discovery), the write
 * validation, the bounded in-guest discovery command, and the parser for its
 * line protocol. The side-effecting discovery flow (boot a session, exec,
 * tear down) lives in profile-discover.ts.
 */

import type { ProfileRepo, ProfileRepoRemote } from "../db/schema.ts";

/** Bounds on stored repo config (write-path validation). */
export const MAX_PROFILE_REPOS = 50;
const MAX_PATH_CHARS = 300;
const MAX_URL_CHARS = 500;

/** Bounds on discovery: checkout count and scan wall-time (in-guest). */
export const DISCOVER_MAX_REPOS = 40;
const DISCOVER_FIND_ROOTS = "/workspace /root /home /srv /opt";
const DISCOVER_MAX_DEPTH = 6;

/**
 * The ONE bounded command discovery runs in the guest. `-name .git` matches
 * both `.git` directories and the `.git` FILES a linked worktree carries.
 * Output line protocol, parsed by `parseDiscoverOutput`:
 *   REPO <checkout path>
 *   REMOTE <name>\t<url> (fetch|push)     — `git remote -v`, prefixed
 */
export const DISCOVER_COMMAND =
  `find ${DISCOVER_FIND_ROOTS} -maxdepth ${DISCOVER_MAX_DEPTH} -name .git ` +
  `\\( -type d -o -type f \\) -print 2>/dev/null | head -n ${DISCOVER_MAX_REPOS} | ` +
  `while read -r g; do d=$(dirname "$g"); echo "REPO $d"; ` +
  `git -C "$d" remote -v 2>/dev/null | sed "s/^/REMOTE /"; done`;

/** One discovered checkout: its path and every distinct remote. */
export interface DiscoveredRepo {
  path: string;
  remotes: { name: string; url: string; parsed: ProfileRepoRemote | null }[];
}

/**
 * Remove the userinfo component from a remote URL — `remoteUrl` is persisted
 * and member-visible, so it must never carry a credential. In http(s) URLs
 * ANY userinfo is a credential (tokens ride as the username:
 * `https://ghp_xxx@github.com/...`). In ssh:// URLs the conventional `git@`
 * is structural — only a password-bearing userinfo (`user:pass@`) strips.
 * scp-style forms (`git@host:path`) cannot carry a password; unchanged.
 */
export function stripRemoteCredentials(url: string): string {
  const web = /^((?:https?|git):\/\/)([^@/]+)@(.+)$/.exec(url);
  if (web) return web[1] + web[3];
  const ssh = /^(ssh:\/\/)([^@/]+)@(.+)$/.exec(url);
  if (ssh && ssh[2].includes(":")) return ssh[1] + ssh[3];
  return url;
}

/**
 * Parse a git remote URL to its forge identity, or null when it doesn't
 * look like a forge remote. Handles:
 *   https://host/owner/name(.git)   (also http, and deeper group paths)
 *   ssh://git@host[:port]/owner/name(.git)
 *   git@host:owner/name(.git)
 * `owner` keeps every path segment but the last (GitLab subgroups).
 */
export function parseGitRemote(url: string): ProfileRepoRemote | null {
  const trimmed = url.trim();
  if (trimmed === "") return null;

  let host: string;
  let path: string;
  const scp = /^[A-Za-z0-9_.-]+@([^:/]+):(.+)$/.exec(trimmed);
  const web = /^(?:https?|ssh|git):\/\/(?:[^@/]+@)?([^:/]+)(?::\d+)?\/(.+)$/.exec(trimmed);
  if (web) {
    host = web[1];
    path = web[2];
  } else if (scp) {
    host = scp[1];
    path = scp[2];
  } else {
    return null;
  }

  const segments = path
    .replace(/\.git$/, "")
    .split("/")
    .filter(Boolean);
  if (segments.length < 2) return null;
  const name = segments[segments.length - 1];
  const owner = segments.slice(0, -1).join("/");
  return { host: host.toLowerCase(), owner, name };
}

/**
 * Normalize + validate repos for the profile write path. The server re-parses
 * `remoteUrl` itself — a client-sent `remote` is ignored, so the stored parse
 * can never drift from the URL. Throws on invalid input (the ProfileService
 * handler maps it to InvalidArgument).
 */
export function normalizeRepos(
  repos: ReadonlyArray<{ path?: string; remoteUrl?: string }>,
): ProfileRepo[] {
  if (repos.length > MAX_PROFILE_REPOS) {
    throw new Error(`at most ${MAX_PROFILE_REPOS} repos per profile`);
  }
  const seen = new Set<string>();
  const out: ProfileRepo[] = [];
  for (const r of repos) {
    const path = (r.path ?? "").trim();
    const remoteUrl = stripRemoteCredentials((r.remoteUrl ?? "").trim());
    if (!path) throw new Error("repo path is required");
    if (path.length > MAX_PATH_CHARS) throw new Error(`repo path exceeds ${MAX_PATH_CHARS} chars`);
    if (remoteUrl.length > MAX_URL_CHARS) {
      throw new Error(`repo remote URL exceeds ${MAX_URL_CHARS} chars`);
    }
    if (seen.has(path)) continue; // dedupe by path, first entry wins
    seen.add(path);
    out.push({ path, remoteUrl, remote: parseGitRemote(remoteUrl) });
  }
  return out;
}

/**
 * Parse the discovery command's stdout. Tolerant: unknown lines are skipped,
 * fetch/push duplicates collapse, remotes seen before any REPO line are
 * dropped. Pure; never throws.
 */
export function parseDiscoverOutput(stdout: string): DiscoveredRepo[] {
  const repos: DiscoveredRepo[] = [];
  let current: DiscoveredRepo | null = null;
  const seenRemotes = new Set<string>();
  for (const line of stdout.split("\n")) {
    if (line.startsWith("REPO ")) {
      const path = line.slice("REPO ".length).trim();
      if (!path) continue;
      current = { path, remotes: [] };
      seenRemotes.clear();
      repos.push(current);
      continue;
    }
    if (line.startsWith("REMOTE ") && current) {
      // `git remote -v`: `<name>\t<url> (fetch)` — tab-separated, with an
      // optional trailing role marker.
      const body = line.slice("REMOTE ".length).replace(/\s+\((fetch|push)\)\s*$/, "");
      const tab = body.indexOf("\t");
      if (tab <= 0) continue;
      const name = body.slice(0, tab).trim();
      // Scrub BEFORE the value exists anywhere: the discover response feeds
      // the editor's candidate pre-fill, so a token-bearing in-guest remote
      // must not survive even in transit.
      const url = stripRemoteCredentials(body.slice(tab + 1).trim());
      if (!name || !url) continue;
      const key = `${name}\0${url}`;
      if (seenRemotes.has(key)) continue;
      seenRemotes.add(key);
      current.remotes.push({ name, url, parsed: parseGitRemote(url) });
    }
  }
  return repos;
}
