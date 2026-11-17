/** GitLab as a git forge. Every call goes through the coordinator's
 * IntegrationOp, which holds the access token and knows the host. A project
 * is named by its path (`group/subgroup/project`), URL-encoded as one
 * segment. */

import {
  escapeGitPath,
  ForgeRequestError,
  linkLines,
  type ForgeLinks,
  type GitForge,
  type GitForgeSpec,
  type InlineComment,
  type PrReviewComment,
  type ReviewEvent,
  type ReviewEventPullRequest,
} from "../../forges/forge.ts";
import {
  isObject,
  parseJson,
  responseError,
  succeeded,
  unusable,
  type Json,
} from "../../forges/http.ts";
import { EMPTY_PR_CONTEXT, type PrContext } from "../../forges/pr-context.ts";
import {
  runIntegrationOp as defaultRunIntegrationOp,
  type IntegrationOpResult,
} from "../../integrations/run-op.ts";
import { REVIEW_DISPATCH_EVENT_KEY } from "../../automations/builtins/review-variant.ts";
import { GITLAB_REVIEW } from "./review-variant.ts";

const GITLAB_PROVIDER = "gitlab";
const FORGE = "GitLab";
const PER_PAGE = 100;
// IntegrationOp returns no response headers, so a short page is the end.
const MAX_PAGES = 20;
/** Developer: the lowest role that can push to a project. */
const MIN_ACCESS_TO_REQUEST_REVIEW = 30;
/** A project or group access token acts as a bot user with this name. */
const TOKEN_BOT_USERNAME = /^(project|group)_\d+_bot(_|$)/;

function str(value: unknown): string | null {
  return typeof value === "string" && value !== "" ? value : null;
}

function positiveInt(value: unknown): number | null {
  return typeof value === "number" && Number.isSafeInteger(value) && value > 0 ? value : null;
}

/** GitLab ids are JSON numbers; they are carried as strings. */
function identifier(value: unknown): string | null {
  return typeof value === "string" && value !== "" ? value : positiveInt(value)?.toString() ?? null;
}

function time(value: unknown): Date | null {
  const text = str(value);
  if (text === null) return null;
  const parsed = new Date(text);
  return Number.isNaN(parsed.getTime()) ? null : parsed;
}

/** GitLab's states are opened, closed, merged and locked (a merge in
 * progress). Draft applies only while open. */
function prState(state: unknown, draft: unknown): string | null {
  switch (state) {
    case "opened":
    case "locked":
      return draft === true ? "draft" : "open";
    case "closed":
    case "merged":
      return state;
    default:
      return null;
  }
}

function isDraft(mr: Json): boolean {
  return mr["draft"] === true || mr["work_in_progress"] === true;
}

/** GitLab answers 404 for a project the token cannot see. A 403 is a missing
 * scope or role. Neither changes on a retry. */
function isPermanentError(error: unknown): boolean {
  return error instanceof ForgeRequestError && error.isClientError;
}

/** `/<group>/<project>/-/merge_requests/<iid>`, with any number of groups. */
function parsePullRequestUrl(url: string): { repo: string; number: number | null } | null {
  let parsed: URL;
  try {
    parsed = new URL(url);
  } catch {
    return null;
  }
  const segments = parsed.pathname.split("/").filter(Boolean);
  const dash = segments.indexOf("-");
  if (dash < 2 || segments[dash + 1] !== "merge_requests") return null;
  const iid = segments[dash + 2] ?? "";
  return {
    repo: segments.slice(0, dash).join("/"),
    number: /^[1-9]\d*$/.test(iid) ? Number(iid) : null,
  };
}

/** The host is the connection's, so every link starts from the merge
 * request's URL: `<project>/-/merge_requests/<iid>`. */
const GITLAB_LINKS: ForgeLinks = {
  pullRequest: () => null,
  review: (prUrl, noteId) => `${prUrl}#note_${noteId}`,
  file(prUrl, at) {
    const base = `${prUrl.split("/-/")[0]}/-/blob/${at.sha}/${escapeGitPath(at.path)}`;
    const lines = linkLines(at);
    if (lines === null) return base;
    return lines.to === undefined ? `${base}#L${lines.from}` : `${base}#L${lines.from}-${lines.to}`;
  },
};

/** The pull request an event names, from the hook's merge-request object.
 * The hook carries no diff base and no author name, so the pass reads the
 * merge request before it starts. */
function eventPullRequest(project: Json, mr: Json, url: string | null): ReviewEventPullRequest | null {
  const repo = str(project["path_with_namespace"]);
  const number = positiveInt(mr["iid"]);
  if (repo === null || number === null) return null;
  const lastCommit = isObject(mr["last_commit"]) ? mr["last_commit"] : {};
  return {
    repo,
    number,
    url: url ?? "",
    headSha: str(lastCommit["id"]) ?? "",
    baseSha: "",
    context: {
      ...EMPTY_PR_CONTEXT,
      providerId: identifier(mr["id"]),
      title: str(mr["title"]),
      state: prState(mr["state"], isDraft(mr)),
      url,
      providerUpdatedAt: time(mr["updated_at"]),
      headBranch: str(mr["source_branch"]),
      baseBranch: str(mr["target_branch"]),
    },
  };
}

/** True when an `update` hook says the merge request left draft. */
function leftDraft(payload: Json): boolean {
  const changes = isObject(payload["changes"]) ? payload["changes"] : {};
  const draft = isObject(changes["draft"]) ? changes["draft"] : null;
  return draft !== null && draft["previous"] === true && draft["current"] === false;
}

function reviewEvent(eventKey: string, payload: Json): ReviewEvent | null {
  const project = isObject(payload["project"]) ? payload["project"] : null;
  const attributes = isObject(payload["object_attributes"]) ? payload["object_attributes"] : null;
  if (project === null || attributes === null) return null;

  if (eventKey === REVIEW_DISPATCH_EVENT_KEY) {
    const pr = eventPullRequest(project, attributes, null);
    return pr && { kind: "dispatch", pr };
  }

  if (eventKey.startsWith("merge_request.")) {
    const pr = eventPullRequest(project, attributes, str(attributes["url"]));
    if (pr === null) return null;
    const change = (trigger: "opened" | "synchronize" | "ready_for_review"): ReviewEvent => ({
      kind: "change",
      trigger,
      draft: isDraft(attributes),
      pr,
    });
    switch (eventKey) {
      case "merge_request.open":
      case "merge_request.reopen":
        return change("opened");
      case "merge_request.update":
        // `oldrev` is present only when the source branch got commits.
        if (str(attributes["oldrev"]) !== null) return change("synchronize");
        return leftDraft(payload) ? change("ready_for_review") : { kind: "update", pr };
      default:
        return { kind: "update", pr };
    }
  }

  if (eventKey === "note.merge_request") {
    const mr = isObject(payload["merge_request"]) ? payload["merge_request"] : null;
    const user = isObject(payload["user"]) ? payload["user"] : null;
    const body = str(attributes["note"]);
    const authorId = user && identifier(user["id"]);
    // An edit of a comment is not a new request.
    if (mr === null || body === null || !authorId || attributes["action"] === "update") return null;
    const webUrl = str(project["web_url"]);
    const iid = positiveInt(mr["iid"]);
    const pr = eventPullRequest(
      project,
      mr,
      webUrl !== null && iid !== null ? `${webUrl}/-/merge_requests/${iid}` : null,
    );
    if (pr === null) return null;
    return {
      kind: "comment",
      body,
      authorId,
      fromBot:
        attributes["system"] === true || TOKEN_BOT_USERNAME.test(str(user?.["username"]) ?? ""),
      // The hook states no role; `mayRequestReview` reads the membership.
      authorAccess: null,
      pr,
    };
  }
  return null;
}

/** The merge request as `GET merge_requests/:iid` returns it. GitLab gives no
 * line counts. `changes_count` is a string, and "1000+" past the cap. */
function readPrContext(mr: Json): PrContext {
  const author = isObject(mr["author"]) ? mr["author"] : null;
  const changes = str(mr["changes_count"]);
  return {
    providerId: identifier(mr["id"]),
    title: str(mr["title"]),
    author: author ? str(author["username"]) : null,
    state: prState(mr["state"], isDraft(mr)),
    url: str(mr["web_url"]),
    providerUpdatedAt: time(mr["updated_at"]),
    headBranch: str(mr["source_branch"]),
    baseBranch: str(mr["target_branch"]),
    additions: null,
    deletions: null,
    changedFiles: changes !== null && /^\d+$/.test(changes) ? Number(changes) : null,
  };
}

interface DiffRefs {
  base_sha: string;
  start_sha: string;
  head_sha: string;
}

function readDiffRefs(mr: Json): DiffRefs | null {
  const refs = isObject(mr["diff_refs"]) ? mr["diff_refs"] : null;
  const base = refs && str(refs["base_sha"]);
  const start = refs && str(refs["start_sha"]);
  const head = refs && str(refs["head_sha"]);
  return base && start && head ? { base_sha: base, start_sha: start, head_sha: head } : null;
}

/** One file of a diff. Each map holds the lines of one side that the diff
 * shows; the value is the line's number on the other side, or null for a
 * line that only this side has. */
interface FileDiff {
  oldPath: string;
  newPath: string;
  newLines: Map<number, number | null>;
  oldLines: Map<number, number | null>;
}

/** The lines of a unified diff. A file with no diff text (collapsed, too
 * large, binary) has none. */
function diffLines(diff: string): Pick<FileDiff, "newLines" | "oldLines"> {
  const newLines = new Map<number, number | null>();
  const oldLines = new Map<number, number | null>();
  let oldLine = 0;
  let newLine = 0;
  for (const line of diff.split("\n")) {
    const hunk = /^@@ -(\d+)(?:,\d+)? \+(\d+)(?:,\d+)? @@/.exec(line);
    if (hunk) {
      oldLine = Number(hunk[1]);
      newLine = Number(hunk[2]);
    } else if (line.startsWith("+")) {
      newLines.set(newLine++, null);
    } else if (line.startsWith("-")) {
      oldLines.set(oldLine++, null);
    } else if (line.startsWith(" ")) {
      newLines.set(newLine, oldLine);
      oldLines.set(oldLine, newLine);
      oldLine++;
      newLine++;
    }
  }
  return { newLines, oldLines };
}

/** The diff a review read: one version of the merge request's diff. */
interface ReviewedDiff {
  refs: DiffRefs;
  /** By new path, and by old path for a renamed or deleted file. */
  files: Map<string, FileDiff>;
}

/** A diff position; null when the diff does not show the line. A finding
 * names a line of the new file, or of the old file when its side is LEFT.
 * GitLab wants both numbers for a line that both sides have, and the old
 * path of a renamed file. It takes one line: a range needs line codes, which
 * this client does not compute, so a range anchors on its last line. */
function position(diff: ReviewedDiff, comment: InlineComment): Json | null {
  const file = diff.files.get(comment.path);
  const left = comment.side === "LEFT";
  const other = (left ? file?.oldLines : file?.newLines)?.get(comment.line);
  if (file === undefined || other === undefined) return null;
  return {
    ...diff.refs,
    position_type: "text",
    new_path: file.newPath,
    old_path: file.oldPath,
    ...(left ? { old_line: comment.line } : { new_line: comment.line }),
    ...(other === null ? {} : left ? { new_line: other } : { old_line: other }),
  };
}

/** One key for a position this client sends and the one GitLab stores. */
function positionKey(where: Json): string {
  return [str(where["new_path"]), positiveInt(where["old_line"]), positiveInt(where["new_line"])].join("\n");
}

/** GitLab runs a line that starts with "/" in column 0 as a quick action
 * (`/approve`, `/merge`) with the token's role, unless the line is code. The
 * text of a review comes from a model that read the merge request, so each
 * such line is escaped. Only a code fence that opens in column 0 and closes
 * keeps its lines: a fence with an indent can end with its list item, and
 * the lines after it are then not code. GitLab also drops "\r". */
export function withoutQuickActions(body: string): string {
  const lines = body.replace(/\r/g, "").split("\n");
  const code = new Set<number>();
  for (let index = 0; index < lines.length; index += 1) {
    // The info string of a backtick fence has no backtick.
    const open = /^(`{3,})[^`]*$|^(~{3,})/.exec(lines[index]!);
    const fence = open?.[1] ?? open?.[2];
    if (fence === undefined) continue;
    const close = new RegExp(`^ {0,3}${fence[0]}{${fence.length},}[ \\t]*$`);
    const end = lines.findIndex((line, at) => at > index && close.test(line));
    if (end === -1) break;
    for (let at = index + 1; at < end; at += 1) code.add(at);
    index = end;
  }
  return lines
    .map((line, index) => (line.startsWith("/") && !code.has(index) ? `\\${line}` : line))
    .join("\n");
}

/** A status comment's id names its merge request too: GitLab addresses a
 * note under its merge request. */
function commentId(prNumber: number, noteId: string): string {
  return `${prNumber}:${noteId}`;
}

function parseCommentId(id: string): { prNumber: string; noteId: string } {
  const match = /^([1-9]\d*):([1-9]\d*)$/.exec(id);
  if (!match) throw new ForgeRequestError(400, `"${id}" is not a GitLab comment id`, "");
  return { prNumber: match[1]!, noteId: match[2]! };
}

export function makeGitlabForge(
  deps: { runIntegrationOp?: typeof defaultRunIntegrationOp } = {},
): GitForge {
  const runOp = deps.runIntegrationOp ?? defaultRunIntegrationOp;
  const send = (method: string, path: string, body?: unknown): Promise<IntegrationOpResult> =>
    runOp(GITLAB_PROVIDER, {
      method,
      path,
      ...(body !== undefined ? { body: JSON.stringify(body) } : {}),
      contentType: "application/json",
    });
  const project = (repo: string): string => `/api/v4/projects/${encodeURIComponent(repo)}`;
  const mergeRequest = (repo: string, prNumber: number | string): string =>
    `${project(repo)}/merge_requests/${prNumber}`;

  const request = async (
    operation: string,
    method: string,
    path: string,
    body?: unknown,
  ): Promise<unknown> => {
    const response = await send(method, path, body);
    if (!succeeded(response)) throw responseError(FORGE, operation, response);
    return parseJson(response, operation);
  };
  const requestObject = async (
    operation: string,
    method: string,
    path: string,
    body?: unknown,
  ): Promise<Json> => {
    const value = await request(operation, method, path, body);
    if (!isObject(value)) throw unusable(operation, "a non-object response");
    return value;
  };
  /** Every item of a list. A list longer than `MAX_PAGES` pages is an error:
   * a caller that read a part of it would act on a wrong answer. */
  const list = async (operation: string, path: string, query = ""): Promise<Json[]> => {
    const items: Json[] = [];
    for (let page = 1; page <= MAX_PAGES; page++) {
      const value = await request(operation, "GET", `${path}?per_page=${PER_PAGE}&page=${page}${query}`);
      if (!Array.isArray(value)) throw unusable(operation, "a non-array response");
      items.push(...value.filter(isObject));
      if (value.length < PER_PAGE) return items;
    }
    throw unusable(operation, `more than ${MAX_PAGES * PER_PAGE} items`);
  };
  /** The user of the connection's token. */
  const selfId = async (): Promise<string> => {
    const id = identifier((await requestObject("get current user", "GET", "/api/v4/user"))["id"]);
    if (id === null) throw unusable("get current user", "no id");
    return id;
  };
  /** A marker in a note proves nothing by itself: any commenter can post
   * one. Only a note of the token's user is this client's. */
  const isOwn = (note: Json, self: string, marker: string): boolean =>
    isObject(note["author"])
    && identifier(note["author"]["id"]) === self
    && (str(note["body"])?.includes(marker) ?? false);
  /** The notes of the merge request, a list per discussion. */
  const discussionNotes = async (repo: string, prNumber: number): Promise<Json[][]> =>
    (await list("list merge request discussions", `${mergeRequest(repo, prNumber)}/discussions`)).map(
      (discussion) => (Array.isArray(discussion["notes"]) ? discussion["notes"].filter(isObject) : []),
    );
  const readMergeRequest = (repo: string, prNumber: number): Promise<Json> =>
    requestObject("get merge request", "GET", mergeRequest(repo, prNumber));
  /** The diff version whose head is `commitId`; null when there is none.
   * The refs and the lines come from one version, so a push between two
   * reads cannot mix them, and a review of an earlier head still anchors. */
  const reviewedDiff = async (repo: string, prNumber: number, commitId: string): Promise<ReviewedDiff | null> => {
    const versions = `${mergeRequest(repo, prNumber)}/versions`;
    const match = (await list("list merge request versions", versions)).find(
      (version) => str(version["head_commit_sha"]) === commitId,
    );
    const id = match && identifier(match["id"]);
    if (!id) return null;
    const version = await requestObject("get merge request version", "GET", `${versions}/${id}`);
    const base = str(version["base_commit_sha"]);
    const start = str(version["start_commit_sha"]);
    if (base === null || start === null) throw unusable("get merge request version", "no commits");
    const files = new Map<string, FileDiff>();
    const diffs = Array.isArray(version["diffs"]) ? version["diffs"].filter(isObject) : [];
    for (const entry of diffs) {
      const newPath = str(entry["new_path"]);
      if (newPath === null) continue;
      const oldPath = str(entry["old_path"]) ?? newPath;
      const file = { oldPath, newPath, ...diffLines(str(entry["diff"]) ?? "") };
      files.set(newPath, file);
      if (!files.has(oldPath)) files.set(oldPath, file);
    }
    return { refs: { base_sha: base, start_sha: start, head_sha: commitId }, files };
  };

  return {
    provider: GITLAB_PROVIDER,
    // GitLab finds a project by its path in any case.
    repoKey: (repo) => repo.toLowerCase(),
    parsePullRequestUrl,
    links: GITLAB_LINKS,
    isPermanentError,
    reviewEvent,

    async mayRequestReview({ pr: { repo }, authorId }) {
      const response = await send("GET", `${project(repo)}/members/all/${encodeURIComponent(authorId)}`);
      // Not a member of the project or of a group above it.
      if (response.status === 404) return false;
      if (!succeeded(response)) throw responseError(FORGE, "get project member", response);
      const member = parseJson(response, "get project member");
      const level = isObject(member) ? member["access_level"] : null;
      if (typeof level !== "number" || level < MIN_ACCESS_TO_REQUEST_REVIEW) return false;
      // A review quotes its findings. A command in a quote, posted with the
      // connection's own token, must not start the next review.
      return (await selfId()) !== authorId;
    },

    async createComment(repo, prNumber, body) {
      const note = await requestObject("create merge request note", "POST", `${mergeRequest(repo, prNumber)}/notes`, {
        body: withoutQuickActions(body),
      });
      const id = identifier(note["id"]);
      if (id === null) throw unusable("create merge request note", "no id");
      return commentId(prNumber, id);
    },

    async updateComment(repo, id, body) {
      const { prNumber, noteId } = parseCommentId(id);
      await requestObject("update merge request note", "PUT", `${mergeRequest(repo, prNumber)}/notes/${noteId}`, {
        body: withoutQuickActions(body),
      });
    },

    async fetchPullRequest(repo, prNumber) {
      const mr = await readMergeRequest(repo, prNumber);
      const refs = readDiffRefs(mr);
      // GitLab fills `diff_refs` a moment after it creates the merge request.
      if (refs === null) throw unusable("get merge request", "no diff refs yet");
      return { headSha: refs.head_sha, baseSha: refs.base_sha, pr: readPrContext(mr) };
    },

    async listReviewComments(repo, prNumber) {
      const comments: PrReviewComment[] = [];
      for (const notes of await discussionNotes(repo, prNumber)) {
        const rootId = identifier(notes[0]?.["id"]);
        for (const note of notes) {
          const id = identifier(note["id"]);
          if (id === null || note["type"] !== "DiffNote" || note["system"] === true) continue;
          const author = isObject(note["author"]) ? note["author"] : null;
          const where = isObject(note["position"]) ? note["position"] : null;
          comments.push({
            id,
            inReplyToId: id === rootId ? null : rootId,
            author: (author && str(author["username"])) ?? "",
            body: str(note["body"]) ?? "",
            path: where ? str(where["new_path"]) : null,
          });
        }
      }
      return comments;
    },

    // The summary note is the last thing a review posts, so it alone says
    // that the review is complete. The inline comments carry the marker too.
    async hasReviewWithMarker(repo, prNumber, marker) {
      const self = await selfId();
      return (await discussionNotes(repo, prNumber)).some((notes) =>
        notes.some((note) => note["type"] !== "DiffNote" && isOwn(note, self, marker)),
      );
    },

    async findCommentWithMarker(repo, prNumber, marker) {
      const self = await selfId();
      const notes = await list(
        "list merge request notes",
        `${mergeRequest(repo, prNumber)}/notes`,
        "&sort=desc&order_by=created_at",
      );
      const id = identifier(notes.find((item) => isOwn(item, self, marker))?.["id"]);
      return id === null ? null : commentId(prNumber, id);
    },

    /** Every position is checked against the reviewed diff before anything
     * posts: with one finding the diff does not show, the summary quotes
     * every finding and no inline comment posts. Each inline comment carries
     * the marker, so an attempt after a failure posts only the comments that
     * are not on the merge request yet. */
    async postReview(input) {
      const { repo, prNumber, marker, comments } = input;
      const base = mergeRequest(repo, prNumber);
      const diff = comments.length > 0 ? await reviewedDiff(repo, prNumber, input.commitId) : null;
      const positions = diff === null ? [] : comments.map((comment) => position(diff, comment));
      let inlinePosted = comments.length === 0 || (diff !== null && !positions.includes(null));
      const inlineFindingIds: string[] = [];
      if (inlinePosted && comments.length > 0) {
        const self = await selfId();
        // How many of this review's comments each position already has.
        const posted = new Map<string, number>();
        for (const note of (await discussionNotes(repo, prNumber)).flat()) {
          if (note["type"] !== "DiffNote" || !isObject(note["position"]) || !isOwn(note, self, marker)) continue;
          const key = positionKey(note["position"]);
          posted.set(key, (posted.get(key) ?? 0) + 1);
        }
        for (const [index, comment] of comments.entries()) {
          const where = positions[index];
          if (!where) continue;
          const key = positionKey(where);
          const already = posted.get(key) ?? 0;
          if (already > 0) {
            posted.set(key, already - 1);
            inlineFindingIds.push(comment.findingId);
            continue;
          }
          const response = await send("POST", `${base}/discussions`, {
            body: `${withoutQuickActions(comment.body)}\n\n${marker}`,
            position: where,
          });
          // GitLab refuses a position it cannot place. The other comments
          // still post; the summary quotes every finding.
          if (response.status === 400) {
            inlinePosted = false;
            continue;
          }
          if (!succeeded(response)) throw responseError(FORGE, "create merge request discussion", response);
          inlineFindingIds.push(comment.findingId);
        }
      }

      const summaryMd = withoutQuickActions(input.buildSummary(inlinePosted));
      const note = await requestObject("create merge request note", "POST", `${base}/notes`, {
        body: summaryMd,
      });
      const providerReviewId = identifier(note["id"]);
      return {
        ...(providerReviewId ? { providerReviewId } : {}),
        inlinePosted,
        inlineFindingIds,
        summaryMd,
      };
    },
  };
}

export const GITLAB_FORGE: GitForgeSpec = {
  make: makeGitlabForge,
  review: GITLAB_REVIEW,
};
