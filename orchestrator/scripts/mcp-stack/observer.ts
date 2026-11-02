import pg from "pg";
import { z } from "zod";
import {
  privateFile,
  requireThat,
  RunnerFailure,
  type FixtureKind,
  type Context,
} from "./context.ts";
import { jsonRequest } from "./transport.ts";
import { canonicalJson } from "./guest.ts";

const count = z.number().int().nonnegative().max(Number.MAX_SAFE_INTEGER);
const digest = z.string().regex(/^sha256:[a-f0-9]{64}$/);
const sourceKeySchema = z.object({
  creation: z.object({
    session_id: z.uuid(),
    task_id: z.string(),
    policy: z.object({
      actorId: z.string(),
      inputId: z.string(),
      parentSessionId: z.string().nullable().optional(),
      eligibility: z.string(),
    }),
    input_digest: digest,
  }),
  context: z.object({
    target: z.object({
      session_id: z.uuid(),
      sandbox_id: z.uuid(),
      host_id: z.uuid(),
      binding_epoch: z.number().int().positive().safe(),
    }),
    operator_generation: z.number().int().positive().safe(),
    operator_identity: digest,
    binding_hash: z.null(),
  }),
  boot_attempt: z.string(),
  native_digest: digest,
});
const FixtureObservationSchema = z.strictObject({
  version: z.literal(1),
  healthy: z.literal(true),
  effects: z.strictObject({
    version: z.literal(1),
    executions: count,
    count: z.number().int().safe(),
    events: z
      .array(
        z.strictObject({
          sequence: count,
          callHash: z.string().regex(/^[a-f0-9]{64}$/),
        }),
      )
      .max(64),
  }),
  reads: count,
  held: count,
  cancellations: count,
  oauth: z.strictObject({
    account: z.enum(["account-one", "account-two"]),
    scope: z
      .enum(["tools:read", "tools:write", "tools:read tools:write"])
      .nullable(),
    authorizations: count,
    exchanges: count,
    rotations: count,
    rejections: count,
    expiresInSeconds: z.number().int().min(1).max(3600),
  }),
});
export class FixtureObserver {
  constructor(
    private readonly ctx: Context,
    private readonly protocol: FixtureKind = "modern",
  ) {}
  private async request(method: "GET" | "POST", route: string, body?: unknown) {
    const token = (
      await privateFile(
        this.ctx.runRoot,
        this.ctx.protocolFixtures[this.protocol].tokenFile,
      )
    )
      .toString("utf8")
      .trim();
    requireThat(/^[A-Za-z0-9_-]{32,128}$/.test(token), "observer_token");
    const response = await jsonRequest(
      this.ctx,
      this.ctx.protocolFixtures[this.protocol],
      method,
      route,
      body,
      { authorization: `Bearer ${token}` },
    );
    requireThat(response.status === 200, "fixture_observer_status");
    return response.body;
  }
  async ready() {
    const response = z
      .strictObject({
        version: z.literal(1),
        ready: z.literal(true),
        resource: z.string(),
        issuer: z.string(),
      })
      .safeParse(await this.request("GET", "/v1/ready"));
    requireThat(
      response.success &&
        response.data.resource ===
          `${this.ctx.protocolFixtures[this.protocol].resourceOrigin}/mcp` &&
        response.data.issuer ===
          this.ctx.protocolFixtures[this.protocol].issuerOrigin,
      "fixture_identity",
    );
  }
  async observe() {
    const response = FixtureObservationSchema.safeParse(
      await this.request("GET", "/v1/observe"),
    );
    requireThat(response.success, "fixture_durable_observation");
    return response.data;
  }
  async configure(input: {
    reply?: "json" | "sse";
    response?: "normal" | "error";
    holdCalls?: boolean;
    revision?: number;
  }) {
    const checked = z
      .strictObject({
        reply: z.enum(["json", "sse"]).optional(),
        response: z.enum(["normal", "error"]).optional(),
        holdCalls: z.boolean().optional(),
        revision: z.number().int().min(1).max(100).optional(),
      })
      .parse(input);
    await this.request("POST", "/v1/case", checked);
  }
  async release() {
    await this.request("POST", "/v1/release", {});
  }
  async cancel() {
    await this.request("POST", "/v1/cancel", {});
  }
  async oauth(input: {
    account?: "account-one" | "account-two";
    scope?: "tools:read" | "tools:write" | "tools:read tools:write";
    issuer?: "configured" | "mismatch";
    expiresInSeconds?: number;
  }) {
    const checked = z
      .strictObject({
        account: z.enum(["account-one", "account-two"]).optional(),
        scope: z
          .enum(["tools:read", "tools:write", "tools:read tools:write"])
          .optional(),
        issuer: z.enum(["configured", "mismatch"]).optional(),
        expiresInSeconds: z.number().int().min(1).max(3600).optional(),
      })
      .parse(input);
    await this.request("POST", "/v1/oauth", checked);
  }
}

/** No caller SQL. Every observation is scoped to a task/session obtained from public CreateTask. */
export class DatabaseObserver {
  private readonly owned = new Map<string, string>();
  private constructor(
    private readonly coordinator: pg.Pool,
    private readonly orchestrator: pg.Pool,
  ) {}
  static async connect(ctx: Context) {
    const pool = async (path: string) => {
      const dsn = (await privateFile(ctx.runRoot, path))
        .toString("utf8")
        .trim();
      let url: URL;
      try {
        url = new URL(dsn);
      } catch {
        throw new RunnerFailure("INVALID_CONTEXT", "observer_dsn");
      }
      requireThat(
        ["postgres:", "postgresql:"].includes(url.protocol) &&
          url.hostname.endsWith(".fixture.example") &&
          url.pathname === `/mcp_${ctx.runId.replaceAll("-", "_")}` &&
          url.search === "",
        "observer_database_scope",
      );
      return new pg.Pool({
        connectionString: dsn,
        max: 1,
        connectionTimeoutMillis: ctx.deadlines.requestMs,
        idleTimeoutMillis: 1000,
        statement_timeout: ctx.deadlines.requestMs,
        query_timeout: ctx.deadlines.requestMs,
        options: "-c default_transaction_read_only=on",
      });
    };
    return new DatabaseObserver(
      await pool(ctx.observer.coordinatorDsnFile),
      await pool(ctx.observer.orchestratorDsnFile),
    );
  }
  own(taskId: string, sessionId: string) {
    requireThat(
      /^[A-Za-z0-9_-]{1,128}$/.test(taskId) &&
        z.uuid().safeParse(sessionId).success &&
        !this.owned.has(sessionId),
      "observer_owned_session",
    );
    this.owned.set(sessionId, taskId);
  }
  private task(sessionId: string) {
    const task = this.owned.get(sessionId);
    requireThat(task, "observer_unowned_session");
    return task;
  }
  async started(sessionId: string, hostId: string) {
    const taskId = this.task(sessionId);
    const result = await this.coordinator.query(
      "SELECT b.phase,b.host_id::text,b.binding_epoch::text,b.boot_attempt,c.native_digest,b.record->'request'->'key' AS source_key,b.record->'finalization'->>'combined_manifest_digest' AS combined_manifest_digest,b.record->'finalization'->'policy'->'context'->>'binding_hash' AS binding_hash FROM session_managed_boots b JOIN session_managed_creations c USING(session_id) WHERE b.session_id=$1 AND c.task_id=$2 AND b.host_id=$3",
      [sessionId, taskId, hostId],
    );
    requireThat(result.rowCount === 1, "coordinator_managed_boot_row");
    const row = z
      .strictObject({
        phase: z.string(),
        host_id: z.string(),
        binding_epoch: z.string().regex(/^[0-9]+$/),
        boot_attempt: z.string(),
        native_digest: z.string().regex(/^sha256:[a-f0-9]{64}$/),
        source_key: sourceKeySchema,
        combined_manifest_digest: digest.nullable(),
        binding_hash: digest.nullable(),
      })
      .safeParse(result.rows[0]);
    requireThat(row.success, "coordinator_managed_boot_shape");
    return row.data;
  }
  async managedLifecycle(
    sessionId: string,
    expected: Awaited<ReturnType<DatabaseObserver["started"]>>,
  ) {
    const taskId = this.task(sessionId);
    const result = await this.orchestrator.query(
      "SELECT mcp_managed_lifecycle FROM task_session WHERE task_id=$1 AND session_id=$2",
      [taskId, sessionId],
    );
    requireThat(result.rowCount === 1, "orchestrator_lifecycle_row");
    const phase = z
      .object({
        phase: z.enum([
          "reserving",
          "held",
          "preparing",
          "finalizing",
          "started",
          "recovering",
          "terminal",
        ]),
      })
      .safeParse(result.rows[0]?.mcp_managed_lifecycle);
    requireThat(phase.success, "orchestrator_lifecycle_phase");
    if (phase.data.phase !== "started") return { phase: phase.data.phase };
    const parsed = z
      .object({
        version: z.literal(1),
        identity: z.object({
          sessionId: z.uuid(),
          taskId: z.string(),
          inputId: z.string(),
          actorId: z.string(),
          inputDigest: digest,
          writeEligibility: z.string(),
          parentSessionId: z.string().optional(),
        }),
        nativeVersion: z.literal(1),
        nativeDigest: digest,
        phase: z.enum([
          "reserving",
          "held",
          "preparing",
          "finalizing",
          "started",
          "recovering",
          "terminal",
        ]),
        bindingHash: digest,
        sourceKey: sourceKeySchema,
      })
      .safeParse(result.rows[0]?.mcp_managed_lifecycle);
    requireThat(parsed.success, "orchestrator_lifecycle_shape");
    const value = parsed.data;
    requireThat(
      value.identity.sessionId === sessionId &&
        value.identity.taskId === taskId &&
        value.nativeDigest === expected.native_digest &&
        value.bindingHash === expected.binding_hash &&
        canonicalJson(value.sourceKey) === canonicalJson(expected.source_key) &&
        value.identity.inputId ===
          expected.source_key.creation.policy.inputId &&
        value.identity.actorId ===
          expected.source_key.creation.policy.actorId &&
        value.identity.inputDigest ===
          expected.source_key.creation.input_digest &&
        value.identity.writeEligibility ===
          expected.source_key.creation.policy.eligibility &&
        (value.identity.parentSessionId ?? null) ===
          (expected.source_key.creation.policy.parentSessionId ?? null),
      "orchestrator_coordinator_source_identity",
    );
    return {
      phase: value.phase,
      nativeDigest: value.nativeDigest,
      bindingHash: value.bindingHash,
    };
  }
  async listener(sessionId: string) {
    const taskId = this.task(sessionId);
    const result = await this.orchestrator.query(
      "SELECT l.owner IS NOT NULL AS leased,l.terminal_at IS NOT NULL AS terminal FROM session_listeners l JOIN task_session ts ON ts.session_id=l.session_id WHERE l.session_id=$1 AND ts.task_id=$2",
      [sessionId, taskId],
    );
    requireThat(result.rowCount === 1, "orchestrator_listener_row");
    const row = z
      .strictObject({ leased: z.boolean(), terminal: z.boolean() })
      .safeParse(result.rows[0]);
    requireThat(row.success, "orchestrator_listener_shape");
    return row.data;
  }
  async outbox(sessionId: string, callId: string) {
    this.task(sessionId);
    requireThat(/^[A-Za-z0-9_-]{1,64}$/.test(callId), "outbox_call_id");
    const result = await this.coordinator.query(
      "SELECT attempts,delivered_at IS NOT NULL AS relayed,acked_at IS NOT NULL AS acknowledged FROM session_outbox WHERE session_id=$1 AND prompt_id=$2 AND kind='tool_result'",
      [sessionId, `tool_result:${sessionId}:${callId}`],
    );
    requireThat(result.rowCount === 1, "coordinator_outbox_row");
    const row = z
      .strictObject({
        attempts: count,
        relayed: z.boolean(),
        acknowledged: z.boolean(),
      })
      .safeParse(result.rows[0]);
    requireThat(row.success, "coordinator_outbox_shape");
    return row.data;
  }
  async close() {
    await Promise.all([this.coordinator.end(), this.orchestrator.end()]);
  }
}
