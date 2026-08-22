/** The integration_action executor (ADR 0119 D5).
 *
 * Resolves a connector's declared action, validates the block's params
 * against its inputSchema, executes it with the connection's org credential
 * (HTTP / GraphQL through Mode-A runIntegrationOp, or a builtin from the
 * table), applies the declared idempotency strategy, and maps the response
 * onto the action's declared outputs. Never touches a secret: Mode A keeps
 * credentials coordinator-side (ADR 0106).
 */

import { makeConnectorStore } from "../../db/connectors.ts";
import { getDb } from "../../db/client.ts";
import { loadRegistry, type ActionSpec, type Connector } from "../../connectors/registry.ts";
import { validateFieldValue } from "../../connectors/field-schema.ts";
import {
  runIntegrationOp as defaultRunIntegrationOp,
  type IntegrationOpResult,
  type RunOpDeps,
} from "../../integrations/run-op.ts";
import { LinearError } from "../../integrations/linear-issues.ts";
import { ownPath } from "../paths.ts";
import { actionClientId } from "./client-id.ts";
import {
  BUILTIN_ACTIONS,
  defaultBuiltinActionDeps,
  type BuiltinActionDeps,
  type BuiltinActionTable,
} from "./builtin.ts";
import { IntegrationActionError, statusIsTransient } from "./errors.ts";

type RunIntegrationOp = typeof defaultRunIntegrationOp;

export const ACTION_OUTPUT_MAX_CHARS = 256 * 1024;
const DEFAULT_SUCCESS_STATUS = [200, 201, 204];
const PLACEHOLDER_RE = /^\{input\.([A-Za-z0-9_]+)\}$/;

export interface IntegrationActionConfigInput {
  provider: string;
  actionId: string;
  connectionId?: string;
  params: Record<string, unknown>;
}

export interface IntegrationActionCallContext {
  runId: string;
  stepPath: string;
}

export interface ExecuteIntegrationActionDeps {
  runOp?: RunIntegrationOp;
  builtins?: BuiltinActionTable;
  builtinDeps?: BuiltinActionDeps;
  connectors?: RunOpDeps["connectors"];
}

function decodeJson(result: IntegrationOpResult, what: string): unknown {
  const text = new TextDecoder().decode(result.body);
  if (text === "") return {};
  try {
    return JSON.parse(text);
  } catch {
    throw new IntegrationActionError(`${what} returned a non-JSON body`, true, result.status);
  }
}

/** Substitute `{input.X}` path placeholders with URI-encoded values. A
 * placeholder that IS a whole template segment may carry `/` in its value
 * (GitHub's `owner/repo`): each sub-segment is validated (no empty, no `.`,
 * no `..`) and encoded separately. A placeholder embedded in a longer segment
 * is fully encoded, so it can never smuggle separators. */
function renderPathTemplate(template: string, params: Record<string, unknown>): string {
  const read = (key: string): string => {
    const value = params[key];
    if (value === undefined || value === null) {
      throw new IntegrationActionError(`path needs missing param "${key}"`, true);
    }
    return String(value);
  };
  return template
    .split("/")
    .map((segment) => {
      // PLACEHOLDER_RE is anchored, so it only matches a whole segment.
      const whole = PLACEHOLDER_RE.exec(segment);
      if (whole) {
        return read(whole[1]!)
          .split("/")
          .map((sub) => {
            if (sub === "" || sub === "." || sub === "..") {
              throw new IntegrationActionError(
                `path param "${whole[1]}" has an invalid segment`,
                true,
              );
            }
            return encodeURIComponent(sub);
          })
          .join("/");
      }
      return segment.replace(/\{input\.([A-Za-z0-9_]+)\}/g, (_all, key: string) =>
        encodeURIComponent(read(key)),
      );
    })
    .join("/");
}

/** Deep-substitute `"{input.X}"` string leaves in a body template. A leaf that
 * is EXACTLY one placeholder takes the param's typed value — and is dropped
 * when the (optional) param is absent; a placeholder embedded in a longer
 * string requires the param. */
function renderBodyTemplate(
  template: Record<string, unknown>,
  params: Record<string, unknown>,
): Record<string, unknown> {
  const rendered: Record<string, unknown> = {};
  for (const [key, value] of Object.entries(template)) {
    if (typeof value === "string") {
      const whole = PLACEHOLDER_RE.exec(value);
      if (whole) {
        const param = params[whole[1]!];
        if (param !== undefined) rendered[key] = param;
        continue;
      }
      rendered[key] = value.replace(/\{input\.([A-Za-z0-9_]+)\}/g, (_all, name: string) => {
        const param = params[name];
        if (param === undefined || param === null) {
          throw new IntegrationActionError(`body needs missing param "${name}"`, true);
        }
        return String(param);
      });
      continue;
    }
    if (typeof value === "object" && value !== null && !Array.isArray(value)) {
      rendered[key] = renderBodyTemplate(value as Record<string, unknown>, params);
      continue;
    }
    rendered[key] = value;
  }
  return rendered;
}

function classifyStatus(status: number, what: string): IntegrationActionError {
  return new IntegrationActionError(`${what} returned ${status}`, !statusIsTransient(status), status);
}

/** Map the declared output dot-paths over a result object; absent mapping →
 * the fallback. Serialized size is capped; overflow keeps the mapped keys and
 * flags truncation instead of failing a succeeded call. */
function mapOutputs(
  spec: ActionSpec,
  result: Record<string, unknown>,
  fallback: Record<string, unknown>,
): Record<string, unknown> {
  const mapped: Record<string, unknown> = {};
  if (spec.output === undefined) {
    Object.assign(mapped, fallback);
  } else {
    for (const [field, path] of Object.entries(spec.output)) {
      const value = ownPath(result, path);
      if (value !== undefined) mapped[field] = value;
    }
  }
  let serialized: string;
  try {
    serialized = JSON.stringify(mapped);
  } catch {
    throw new IntegrationActionError("action output is not JSON-serializable", true);
  }
  if (serialized.length > ACTION_OUTPUT_MAX_CHARS) {
    const truncated: Record<string, unknown> = { truncated: true };
    for (const [key, value] of Object.entries(mapped)) {
      const piece = typeof value === "string" ? value.slice(0, 4096) : value;
      if (JSON.stringify(truncated).length < ACTION_OUTPUT_MAX_CHARS / 4) truncated[key] = piece;
    }
    return truncated;
  }
  return mapped;
}

async function findAction(
  provider: string,
  actionId: string,
  connectors: ExecuteIntegrationActionDeps["connectors"],
): Promise<{ connector: Connector; action: ActionSpec }> {
  const source = connectors ?? makeConnectorStore(getDb());
  const registry = await loadRegistry(source);
  const connector = registry.get(provider);
  if (!connector) throw new IntegrationActionError(`unknown provider "${provider}"`, true);
  const action = connector.actions?.find((a) => a.id === actionId);
  if (!action) {
    throw new IntegrationActionError(`provider "${provider}" has no action "${actionId}"`, true);
  }
  return { connector, action };
}

export async function executeIntegrationAction(
  config: IntegrationActionConfigInput,
  ctx: IntegrationActionCallContext,
  deps: ExecuteIntegrationActionDeps = {},
): Promise<Record<string, unknown>> {
  const runOp: RunIntegrationOp = deps.runOp ?? defaultRunIntegrationOp;
  const { connector, action } = await findAction(config.provider, config.actionId, deps.connectors);

  const violations = validateFieldValue(action.inputSchema, config.params);
  if (violations.length > 0) {
    const first = violations[0]!;
    throw new IntegrationActionError(
      `invalid params: ${first.path === "" ? "root" : first.path} ${first.message}` +
        (violations.length > 1 ? ` (+${violations.length - 1} more)` : ""),
      true,
    );
  }

  const marker =
    action.idempotency.kind === "marker_comment"
      ? (action.idempotency.markerTemplate ?? "<!-- engrams-automation:{runId}:{stepPath} -->")
          .replaceAll("{runId}", ctx.runId)
          .replaceAll("{stepPath}", ctx.stepPath)
      : null;

  switch (action.execute.kind) {
    case "http": {
      let params = config.params;
      if (marker !== null) {
        const guard = await commentMarkerGuard(runOp, config, action, marker);
        if (guard.kind === "exists") return guard.outputs;
        params = guard.params;
      }
      const path = renderPathTemplate(action.execute.pathTemplate, params);
      const body =
        action.execute.bodyTemplate !== undefined
          ? JSON.stringify(renderBodyTemplate(action.execute.bodyTemplate, params))
          : undefined;
      const result = await runOp(config.provider, {
        method: action.execute.method,
        path,
        ...(body !== undefined ? { body } : {}),
        contentType: "application/json",
      });
      const success = action.successStatus ?? DEFAULT_SUCCESS_STATUS;
      if (!success.includes(result.status)) {
        throw classifyStatus(result.status, `${config.provider}.${config.actionId}`);
      }
      const parsed = decodeJson(result, `${config.provider}.${config.actionId}`);
      return mapOutputs(
        action,
        (typeof parsed === "object" && parsed !== null
          ? parsed
          : {}) as Record<string, unknown>,
        { status: result.status },
      );
    }
    case "graphql": {
      const endpoint = connector.graphqlEndpoint;
      if (endpoint === undefined) {
        throw new IntegrationActionError(`provider "${config.provider}" has no GraphQL endpoint`, true);
      }
      // client_id idempotency: inject a deterministic $id variable when the
      // caller did not supply one; the document must thread it into the input.
      const variables =
        action.idempotency.kind === "client_id" && config.params["id"] === undefined
          ? { ...config.params, id: actionClientId(ctx.runId, ctx.stepPath) }
          : config.params;
      const result = await runOp(config.provider, {
        method: "POST",
        path: endpoint,
        body: JSON.stringify({ query: action.execute.document, variables }),
        contentType: "application/json",
      });
      if (result.status < 200 || result.status >= 300) {
        throw classifyStatus(result.status, `${config.provider}.${config.actionId}`);
      }
      const parsed = decodeJson(result, `${config.provider}.${config.actionId}`) as Record<
        string,
        unknown
      >;
      const errors = parsed["errors"];
      if (Array.isArray(errors) && errors.length > 0) {
        const message =
          (errors[0] as { message?: unknown }).message?.toString() ?? "GraphQL error";
        // Same posture as LinearError: request-shaped errors are permanent,
        // availability-shaped ones are transient.
        const transient = /rate ?limit|timeout|unavailable|internal/i.test(message);
        throw new IntegrationActionError(message, !transient, result.status);
      }
      return mapOutputs(action, parsed, { status: result.status });
    }
    case "builtin": {
      const table = deps.builtins ?? BUILTIN_ACTIONS;
      const fn = table[action.execute.id];
      if (fn === undefined) {
        throw new IntegrationActionError(`builtin action "${action.execute.id}" is not implemented`, true);
      }
      const builtinDeps =
        deps.builtinDeps ?? defaultBuiltinActionDeps(deps.connectors ? { connectors: deps.connectors } : undefined);
      let result: Record<string, unknown>;
      try {
        result = await fn(config.params, { runId: ctx.runId, stepPath: ctx.stepPath, marker }, builtinDeps);
      } catch (error) {
        if (error instanceof IntegrationActionError) throw error;
        if (error instanceof LinearError) {
          throw new IntegrationActionError(error.message, !error.retryable, error.status);
        }
        throw new IntegrationActionError(
          error instanceof Error ? error.message : String(error),
          // Unknown SDK failures default to permanent: a retry storm against a
          // provider is worse than one failed run a person can re-run.
          true,
        );
      }
      return mapOutputs(action, result, result);
    }
  }
}

const SAFE_REPO_RE = /^[A-Za-z0-9._-]+\/[A-Za-z0-9._-]+$/;

type MarkerGuardOutcome =
  | { kind: "exists"; outputs: Record<string, unknown> }
  | { kind: "proceed"; params: Record<string, unknown> };

/** marker_comment for HTTP comment creators: pre-scan the newest 100 issue
 * comments for the marker; found → short-circuit with the existing comment's
 * mapped outputs; else append the marker to the body param. Actions without
 * comment-shaped params (repo + number + body) just get the marker appended —
 * the pre-scan is a crash-window optimization, not a correctness gate. */
async function commentMarkerGuard(
  runOp: RunIntegrationOp,
  config: IntegrationActionConfigInput,
  action: ActionSpec,
  marker: string,
): Promise<MarkerGuardOutcome> {
  const repo = config.params["repo"];
  const number = config.params["number"];
  const body = config.params["body"];
  const proceed = (): MarkerGuardOutcome => ({
    kind: "proceed",
    params: {
      ...config.params,
      ...(typeof body === "string" ? { body: `${body}\n\n${marker}` } : {}),
    },
  });
  if (typeof repo !== "string" || !SAFE_REPO_RE.test(repo) || typeof number !== "number") {
    return proceed();
  }
  const result = await runOp(config.provider, {
    method: "GET",
    path: `/repos/${repo}/issues/${number}/comments?per_page=100&sort=created&direction=desc`,
    contentType: "application/json",
  });
  if (result.status >= 200 && result.status < 300) {
    const parsed = decodeJson(result, `${config.provider}.${config.actionId} marker scan`);
    if (Array.isArray(parsed)) {
      for (const comment of parsed) {
        const record = comment as { body?: unknown };
        if (typeof record.body === "string" && record.body.includes(marker)) {
          return {
            kind: "exists",
            outputs: mapOutputs(action, comment as Record<string, unknown>, {
              status: result.status,
            }),
          };
        }
      }
    }
  }
  return proceed();
}
