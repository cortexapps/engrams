/**
 * The Linear issue calls the spec ticket sync makes (ADR 0114 D6, R43-R44).
 *
 * There is no new Linear client here. The org connector already exists, and
 * `runIntegrationOp` (Mode A) is the sessionless seam over it: the orchestrator
 * hands the coordinator a request, the coordinator adds the workspace's OAuth
 * token and makes the call, and the token never reaches this tier. This module
 * is only the three GraphQL documents the sync needs, plus honest error
 * classification.
 *
 * The important call is {@link LinearIssueClient.findIssue}. Linear lets the
 * caller choose an issue's id, so the sync reserves one before it creates.
 * A driver that dies between the create and its own bookkeeping can then look
 * the issue up by that id and adopt it, which is what makes a retry after a pod
 * roll create zero duplicates (N4).
 */

import {
  runIntegrationOp as defaultRunIntegrationOp,
  type IntegrationOpResult,
} from "./run-op.ts";

export type RunIntegrationOp = typeof defaultRunIntegrationOp;

/** Why a Linear call failed, in the terms the ledger reports to a person. */
export type LinearErrorCode =
  /** The token is missing, expired or rejected — the Reconnect case. */
  | "unauthenticated"
  /** The token is valid but may not do this. */
  | "forbidden"
  /** The request is wrong: an unknown team, a missing title, a bad label. */
  | "invalid_request"
  /** Linear is rate-limiting us. */
  | "rate_limited"
  /** Linear failed, or the response made no sense. Retryable. */
  | "unavailable";

export class LinearError extends Error {
  constructor(
    readonly code: LinearErrorCode,
    message: string,
    /** The HTTP status, when there was one. */
    readonly status?: number,
  ) {
    super(message);
    this.name = "LinearError";
  }

  /** True when a later attempt could succeed without a person doing anything. */
  get retryable(): boolean {
    return this.code === "rate_limited" || this.code === "unavailable";
  }
}

/** One Linear issue, as the ledger shows it. */
export interface LinearIssue {
  /** The UUID. This is what the draft row stores. */
  id: string;
  /** The human identity, e.g. `ENG-412`. */
  identifier: string;
  url: string;
}

export interface CreateLinearIssueInput {
  /** Chosen by the caller before the create, so a retry can adopt. */
  id: string;
  teamId: string;
  title: string;
  description: string;
  projectId?: string | undefined;
  labelIds?: readonly string[] | undefined;
}

export interface CreateLinearRelationInput {
  /** Chosen by the caller, so a replay does not add a second relation. */
  id: string;
  /** The issue that blocks. */
  blockerIssueId: string;
  /** The issue that waits. */
  blockedIssueId: string;
}

/** One named thing a person can point a spec's tickets at (R45). */
export interface LinearNamedEntity {
  id: string;
  name: string;
}

/** What the sync target picker offers. */
export interface LinearWorkspace {
  teams: LinearNamedEntity[];
  projects: LinearNamedEntity[];
  labels: LinearNamedEntity[];
}

export interface LinearIssueClient {
  /** The issue with this id, or null when Linear has no such issue. */
  findIssue(id: string): Promise<LinearIssue | null>;
  createIssue(input: CreateLinearIssueInput): Promise<LinearIssue>;
  /** A "blocks" relation. Linear supports these, so R44 uses them. */
  createBlockingRelation(input: CreateLinearRelationInput): Promise<void>;
  /** The teams, projects and labels a person may choose between. */
  readWorkspace(): Promise<LinearWorkspace>;
}

const FIND_ISSUE = `query EngramsFindIssue($id: String!) {
  issue(id: $id) { id identifier url }
}`;

const CREATE_ISSUE = `mutation EngramsCreateIssue($input: IssueCreateInput!) {
  issueCreate(input: $input) { success issue { id identifier url } }
}`;

/**
 * A "blocks" relation (R44).
 *
 * `issueRelationCreate` is not one of the connector's declared operations,
 * which gate what a *sandbox* may send through the egress proxy. This call is
 * server-side and sessionless: the coordinator makes it with the workspace
 * credential and never consults that list. The cost of using an undeclared
 * mutation is that it does not appear in the capability catalog, so it is named
 * here instead.
 */
const CREATE_RELATION = `mutation EngramsCreateIssueRelation($input: IssueRelationCreateInput!) {
  issueRelationCreate(input: $input) { success }
}`;

const READ_WORKSPACE = `query EngramsReadWorkspace {
  teams(first: 100) { nodes { id name } }
  projects(first: 100) { nodes { id name } }
  issueLabels(first: 100) { nodes { id name } }
}`;

export function makeLinearIssueClient(
  deps: { runIntegrationOp?: RunIntegrationOp } = {},
): LinearIssueClient {
  const runOp = deps.runIntegrationOp ?? defaultRunIntegrationOp;

  async function graphql(
    operation: string,
    query: string,
    variables: Record<string, unknown>,
  ): Promise<Record<string, unknown>> {
    let response: IntegrationOpResult;
    try {
      response = await runOp("linear", {
        method: "POST",
        path: "/graphql",
        body: JSON.stringify({ query, variables }),
        contentType: "application/json",
      });
    } catch (error) {
      // The coordinator could not make the call at all: no connector, no
      // credential, or no route to it. A person must connect Linear (R42).
      throw new LinearError("unauthenticated", connectFailure(operation, error));
    }
    return readGraphqlBody(operation, response);
  }

  return {
    async findIssue(id) {
      const data = await graphql("find issue", FIND_ISSUE, { id }).catch((error: unknown) => {
        // Linear answers an unknown id with an error, not with a null issue.
        if (error instanceof LinearError && error.code === "invalid_request") return null;
        throw error;
      });
      if (data === null) return null;
      const issue = data["issue"];
      return isRecord(issue) ? readIssue("find issue", issue) : null;
    },

    async createIssue(input) {
      const data = await graphql("create issue", CREATE_ISSUE, {
        input: {
          id: input.id,
          teamId: input.teamId,
          title: input.title,
          description: input.description,
          ...(input.projectId === undefined ? {} : { projectId: input.projectId }),
          ...(input.labelIds === undefined || input.labelIds.length === 0
            ? {}
            : { labelIds: [...input.labelIds] }),
        },
      });
      const payload = data["issueCreate"];
      if (!isRecord(payload) || payload["success"] !== true || !isRecord(payload["issue"])) {
        throw new LinearError("unavailable", "Linear accepted the create but returned no issue.");
      }
      return readIssue("create issue", payload["issue"]);
    },

    async createBlockingRelation(input) {
      const data = await graphql("create issue relation", CREATE_RELATION, {
        input: {
          id: input.id,
          issueId: input.blockerIssueId,
          relatedIssueId: input.blockedIssueId,
          type: "blocks",
        },
      });
      const payload = data["issueRelationCreate"];
      if (!isRecord(payload) || payload["success"] !== true) {
        throw new LinearError("unavailable", "Linear refused the blocking relation.");
      }
    },

    async readWorkspace() {
      const data = await graphql("read the workspace", READ_WORKSPACE, {});
      return {
        teams: readNodes(data["teams"]),
        projects: readNodes(data["projects"]),
        labels: readNodes(data["issueLabels"]),
      };
    },
  };
}

/** The `{ nodes: [{id, name}] }` shape every Linear collection uses. */
function readNodes(value: unknown): LinearNamedEntity[] {
  if (!isRecord(value) || !Array.isArray(value["nodes"])) return [];
  return value["nodes"].flatMap((node) => {
    if (!isRecord(node)) return [];
    const id = node["id"];
    const name = node["name"];
    return typeof id === "string" && typeof name === "string" ? [{ id, name }] : [];
  });
}

function connectFailure(operation: string, error: unknown): string {
  const reason = error instanceof Error ? error.message : String(error);
  return `Could not reach Linear to ${operation}: ${reason}`;
}

/**
 * Read one GraphQL response.
 *
 * GraphQL puts errors in a 200 body, so the status alone never tells us
 * whether the call worked. Both arms end at the same {@link LinearError}, so a
 * caller never has to know which arm it came from.
 */
function readGraphqlBody(
  operation: string,
  response: IntegrationOpResult,
): Record<string, unknown> {
  const text = new TextDecoder().decode(response.body);
  if (response.status < 200 || response.status >= 300) {
    throw new LinearError(
      statusCode(response.status),
      `Linear returned ${response.status} for ${operation}${detail(text)}`,
      response.status,
    );
  }
  let value: unknown;
  try {
    value = JSON.parse(text);
  } catch {
    throw new LinearError("unavailable", `Linear returned an unreadable body for ${operation}.`);
  }
  if (!isRecord(value)) {
    throw new LinearError("unavailable", `Linear returned a non-object body for ${operation}.`);
  }
  const errors = value["errors"];
  if (Array.isArray(errors) && errors.length > 0) {
    throw new LinearError(graphqlCode(errors), `Linear refused ${operation}: ${errorText(errors)}`);
  }
  const data = value["data"];
  if (!isRecord(data)) {
    throw new LinearError("unavailable", `Linear returned no data for ${operation}.`);
  }
  return data;
}

function statusCode(status: number): LinearErrorCode {
  if (status === 401) return "unauthenticated";
  if (status === 403) return "forbidden";
  if (status === 429) return "rate_limited";
  if (status >= 400 && status < 500) return "invalid_request";
  return "unavailable";
}

/**
 * Classify a GraphQL error array. Linear names the class in
 * `extensions.type` (`authentication_error`, `invalid input`, …), and falls
 * back to prose we match loosely rather than guess a wrong class.
 */
function graphqlCode(errors: readonly unknown[]): LinearErrorCode {
  const types = errors.flatMap((error) => {
    if (!isRecord(error)) return [];
    const extensions = error["extensions"];
    const type = isRecord(extensions) ? extensions["type"] : undefined;
    const message = typeof error["message"] === "string" ? error["message"] : "";
    return [typeof type === "string" ? type : "", message].map((part) => part.toLowerCase());
  });
  const has = (needle: string) => types.some((type) => type.includes(needle));
  if (has("authentication")) return "unauthenticated";
  if (has("ratelimit") || has("rate limit")) return "rate_limited";
  if (has("forbidden") || has("permission")) return "forbidden";
  if (has("not found") || has("invalid input") || has("invalid_input")) return "invalid_request";
  if (has("internal") || has("timeout")) return "unavailable";
  return "invalid_request";
}

function errorText(errors: readonly unknown[]): string {
  const messages = errors
    .map((error) => (isRecord(error) && typeof error["message"] === "string" ? error["message"] : ""))
    .filter((message) => message !== "");
  return messages.length > 0 ? messages.join("; ") : "no reason given";
}

function detail(text: string): string {
  const trimmed = text.trim();
  return trimmed === "" ? "." : `: ${trimmed.slice(0, 200)}`;
}

function readIssue(operation: string, issue: Record<string, unknown>): LinearIssue {
  const id = issue["id"];
  const identifier = issue["identifier"];
  const url = issue["url"];
  if (typeof id !== "string" || typeof identifier !== "string" || typeof url !== "string") {
    throw new LinearError("unavailable", `Linear returned an incomplete issue for ${operation}.`);
  }
  return { id, identifier, url };
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}
