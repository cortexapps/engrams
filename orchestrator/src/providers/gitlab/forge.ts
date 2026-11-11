/** GitLab as a git forge. Every call goes through the coordinator's
 * IntegrationOp, which holds the access token and knows the host. A project
 * is named by its path (`group/subgroup/project`), URL-encoded as one
 * segment. */

import {
  ForgeRequestError,
  type GitForge,
  type GitForgeSpec,
  type InlineComment,
  type PrReviewComment,
  type ReviewEvent,
  type ReviewEventPullRequest,
} from "../../forges/forge.ts";
import type { PrContext } from "../../forges/pr-context.ts";
import {
  runIntegrationOp as defaultRunIntegrationOp,
  type IntegrationOpResult,
} from "../../integrations/run-op.ts";
import { REVIEW_DISPATCH_EVENT_KEY } from "../../automations/builtins/review-variant.ts";
import { GITLAB_REVIEW } from "./review-variant.ts";

const GITLAB_PROVIDER = "gitlab";
const PER_PAGE = 100;
// IntegrationOp returns no response headers, so a short page is the end.
const MAX_PAGES = 10;
/** Developer: the lowest role that can push to a project. */
const MIN_ACCESS_TO_REQUEST_REVIEW = 30;
/** A project or group access token acts as a bot user with this name. */
const TOKEN_BOT_USERNAME = /^(project|group)_\d+_bot(_|$)/;

type Json = Record<string, unknown>;

function isObject(value: unknown): value is Json {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

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

function isoTime(value: unknown): string | null {
  const text = str(value);
  if (text === null) return null;
  const parsed = new Date(text);
  return Number.isNaN(parsed.getTime()) ? null : parsed.toISOString();
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

function decodeBody(body: Uint8Array): string {
  return new TextDecoder().decode(body);
}

function succeeded(result: IntegrationOpResult): boolean {
  return result.status >= 200 && result.status < 300;
}

function responseError(operation: string, result: IntegrationOpResult): ForgeRequestError {
  const detail = decodeBody(result.body).trim();
  return new ForgeRequestError(
    result.status,
    `${operation} failed with GitLab status ${result.status}${detail ? `: ${detail}` : ""}`,
    detail,
  );
}

/** A 2xx response whose body is not what the operation returns. */
function unusable(operation: string, what: string): ForgeRequestError {
  return new ForgeRequestError(200, `${operation} returned ${what}`, "");
}

function parseJson(result: IntegrationOpResult, operation: string): unknown {
  try {
    return JSON.parse(decodeBody(result.body));
  } catch {
    throw unusable(operation, "invalid JSON");
  }
}

/** GitLab answers 404 for a project the token cannot see. A 403 is a missing
 * scope or role. Neither changes on a retry. */
function isPermanentError(error: unknown): boolean {
  return (
    error instanceof ForgeRequestError
    && (error.status === 401 || error.status === 403 || error.status === 404 || error.status === 410)
  );
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
      providerId: identifier(mr["id"]),
      title: str(mr["title"]),
      state: prState(mr["state"], isDraft(mr)),
      url,
      providerUpdatedAt: isoTime(mr["updated_at"]),
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
        return leftDraft(payload) ? change("ready_for_review") : null;
      default:
        return null;
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
      pr,
    };
  }
  return null;
}

/** The merge request as `GET merge_requests/:iid` returns it. GitLab gives no
 * line counts. `changes_count` is a string, and "1000+" past the cap. */
function readPrContext(mr: Json): PrContext {
  const author = isObject(mr["author"]) ? mr["author"] : null;
  const updatedAt = isoTime(mr["updated_at"]);
  const changes = str(mr["changes_count"]);
  return {
    providerId: identifier(mr["id"]),
    title: str(mr["title"]),
    author: author ? str(author["username"]) : null,
    state: prState(mr["state"], isDraft(mr)),
    url: str(mr["web_url"]),
    providerUpdatedAt: updatedAt === null ? null : new Date(updatedAt),
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

/** A diff position. A finding names a line of the new file, or of the old
 * file when its side is LEFT. GitLab takes one line: a range needs line
 * codes, which this client does not compute, so a range anchors on its last
 * line. */
function position(refs: DiffRefs, comment: InlineComment): Json {
  return {
    ...refs,
    position_type: "text",
    new_path: comment.path,
    old_path: comment.path,
    ...(comment.side === "LEFT" ? { old_line: comment.line } : { new_line: comment.line }),
  };
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
    if (!succeeded(response)) throw responseError(operation, response);
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
  /** Every item of a list, up to `MAX_PAGES` pages. */
  const list = async (operation: string, path: string, query = ""): Promise<Json[]> => {
    const items: Json[] = [];
    for (let page = 1; page <= MAX_PAGES; page++) {
      const value = await request(operation, "GET", `${path}?per_page=${PER_PAGE}&page=${page}${query}`);
      if (!Array.isArray(value)) throw unusable(operation, "a non-array response");
      items.push(...value.filter(isObject));
      if (value.length < PER_PAGE) break;
    }
    return items;
  };
  const discussionNotes = async (repo: string, prNumber: number): Promise<Json[][]> =>
    (await list("list merge request discussions", `${mergeRequest(repo, prNumber)}/discussions`)).map(
      (discussion) => (Array.isArray(discussion["notes"]) ? discussion["notes"].filter(isObject) : []),
    );
  const readMergeRequest = (repo: string, prNumber: number): Promise<Json> =>
    requestObject("get merge request", "GET", mergeRequest(repo, prNumber));

  return {
    provider: GITLAB_PROVIDER,
    // GitLab finds a project by its path in any case.
    caseInsensitiveRepos: true,
    parsePullRequestUrl,
    isPermanentError,
    reviewEvent,

    async mayRequestReview(repo, authorId) {
      const response = await send("GET", `${project(repo)}/members/all/${encodeURIComponent(authorId)}`);
      // Not a member of the project or of a group above it.
      if (response.status === 404) return false;
      if (!succeeded(response)) throw responseError("get project member", response);
      const member = parseJson(response, "get project member");
      const level = isObject(member) ? member["access_level"] : null;
      return typeof level === "number" && level >= MIN_ACCESS_TO_REQUEST_REVIEW;
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
            authorLogin: (author && str(author["username"])) ?? "",
            body: str(note["body"]) ?? "",
            path: where ? str(where["new_path"]) : null,
          });
        }
      }
      return comments;
    },

    async hasReviewWithMarker(repo, prNumber, marker) {
      const discussions = await discussionNotes(repo, prNumber);
      return discussions.some((notes) => notes.some((note) => str(note["body"])?.includes(marker)));
    },

    async findCommentWithMarker(repo, prNumber, marker) {
      const response = await send(
        "GET",
        `${mergeRequest(repo, prNumber)}/notes?per_page=${PER_PAGE}&sort=desc&order_by=created_at`,
      );
      if (!succeeded(response)) return null;
      let value: unknown;
      try {
        value = JSON.parse(decodeBody(response.body));
      } catch {
        return null;
      }
      if (!Array.isArray(value)) return null;
      const note = value.find(
        (item): item is Json => isObject(item) && (str(item["body"])?.includes(marker) ?? false),
      );
      const id = note && identifier(note["id"]);
      return note && id ? { ...note, id: commentId(prNumber, id) } : null;
    },

    async createComment(repo, prNumber, body) {
      const note = await requestObject("create merge request note", "POST", `${mergeRequest(repo, prNumber)}/notes`, {
        body,
      });
      const id = identifier(note["id"]);
      if (id === null) throw unusable("create merge request note", "no id");
      return commentId(prNumber, id);
    },

    async updateComment(repo, id, body) {
      const { prNumber, noteId } = parseCommentId(id);
      await requestObject("update merge request note", "PUT", `${mergeRequest(repo, prNumber)}/notes/${noteId}`, {
        body,
      });
    },

    /** The inline comments post as draft notes, which nobody sees until
     * each is published. So a comment the diff refuses costs nothing
     * visible: the drafts are deleted and the summary quotes every finding.
     * Every comment carries the marker, so a pass that stops between a
     * publish and the summary does not post the comments again. */
    async postReview(input) {
      const base = mergeRequest(input.repo, input.prNumber);
      const deleteDraft = async (id: string): Promise<void> => {
        const response = await send("DELETE", `${base}/draft_notes/${id}`);
        if (!succeeded(response) && response.status !== 404) {
          throw responseError("delete draft note", response);
        }
      };
      // Drafts an earlier attempt of this post left behind.
      for (const draft of await list("list draft notes", `${base}/draft_notes`)) {
        const id = identifier(draft["id"]);
        if (id !== null && str(draft["note"])?.includes(input.marker)) await deleteDraft(id);
      }
      const refs = readDiffRefs(await readMergeRequest(input.repo, input.prNumber));
      const drafts: string[] = [];
      let inlinePosted = refs !== null;
      if (refs !== null) {
        for (const comment of input.comments) {
          const response = await send("POST", `${base}/draft_notes`, {
            note: `${comment.body}\n\n${input.marker}`,
            position: position(refs, comment),
          });
          if (!succeeded(response)) {
            // GitLab refuses a position that is not in the diff.
            if (response.status !== 400 && response.status !== 422) {
              throw responseError("create draft note", response);
            }
            inlinePosted = false;
            break;
          }
          const draft = parseJson(response, "create draft note");
          const id = isObject(draft) ? identifier(draft["id"]) : null;
          if (id === null) throw unusable("create draft note", "no id");
          drafts.push(id);
        }
      }
      // One publish per draft: the bulk call also publishes every other
      // draft of the token's user on the merge request.
      for (const id of drafts) {
        if (!inlinePosted) {
          await deleteDraft(id);
          continue;
        }
        const response = await send("PUT", `${base}/draft_notes/${id}/publish`);
        if (!succeeded(response)) throw responseError("publish draft note", response);
      }
      const summaryMd = input.buildSummary(inlinePosted);
      const note = await requestObject("create merge request note", "POST", `${base}/notes`, {
        body: summaryMd,
      });
      const providerReviewId = identifier(note["id"]);
      return {
        ...(providerReviewId ? { providerReviewId } : {}),
        posted: true,
        inlinePosted,
        summaryMd,
      };
    },
  };
}

export const GITLAB_FORGE: GitForgeSpec = {
  provider: GITLAB_PROVIDER,
  make: makeGitlabForge,
  review: GITLAB_REVIEW,
};
