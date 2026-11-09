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
export const sourceKeySchema = z.object({
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
  private async request(
    method: "GET" | "POST",
    route: string,
    body?: unknown,
    requestMs = this.ctx.deadlines.requestMs,
  ) {
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
      { ...this.ctx, deadlines: { ...this.ctx.deadlines, requestMs } },
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
    const deadline = performance.now() + this.ctx.deadlines.stepMs;
    let body: unknown;
    // Container start can complete before the HTTPS listener accepts requests.
    for (;;) {
      try {
        body = await this.request(
          "GET",
          "/v1/ready",
          undefined,
          Math.max(
            1,
            Math.min(
              this.ctx.deadlines.requestMs,
              deadline - performance.now(),
            ),
          ),
        );
        break;
      } catch (error) {
        if (
          !(error instanceof RunnerFailure) ||
          error.code !== "TRANSPORT_FAILED" ||
          error.seam !== "https_request"
        )
          throw error;
        const remaining = deadline - performance.now();
        if (remaining <= 0) throw error;
        await new Promise((resolve) =>
          setTimeout(resolve, Math.min(100, remaining)),
        );
        if (performance.now() >= deadline) throw error;
      }
    }
    const response = z
      .strictObject({
        version: z.literal(1),
        ready: z.literal(true),
        resource: z.string(),
        issuer: z.string(),
      })
      .safeParse(body);
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
    private readonly databaseName: string,
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
      `mcp_${ctx.runId.replaceAll("-", "_")}`,
    );
  }
  /** Fixed schema-only prerequisites, before public signup or publication. No authority rows are read. */
  async prerequisites() {
    const expected = {
      coordinator: [
        "session_ops.id:bigint",
        "session_ops.session_id:uuid",
        "session_ops.kind:text",
        "session_ops.state:text",
        "session_managed_creations.session_id:uuid",
        "session_managed_creations.task_id:text",
        "session_managed_creations.native_version:integer",
        "session_managed_creations.native_bytes:bytea",
        "session_managed_creations.native_digest:text",
        "session_managed_boots.session_id:uuid",
        "session_managed_boots.host_id:uuid",
        "session_managed_boots.binding_epoch:bigint",
        "session_managed_boots.boot_attempt:text",
        "session_managed_boots.phase:text",
        "session_managed_boots.record:jsonb",
        "session_managed_boot_intents.intent_id:text",
        "session_managed_boot_intents.session_id:uuid",
        "session_managed_boot_intents.binding_epoch:bigint",
        "session_managed_boot_intents.completed:boolean",
        "session_managed_boot_intents.record:jsonb",
        "session_outbox.session_id:uuid",
        "session_outbox.prompt_id:text",
        "session_outbox.kind:text",
        "session_outbox.attempts:integer",
        "session_outbox.delivered_at:timestamp with time zone",
        "session_outbox.acked_at:timestamp with time zone",
        "session_events.session_id:uuid",
        "session_events.idx:bigint",
        "session_events.kind:text",
        "session_events.payload:jsonb",
        "session_managed_interrupted_targets.session_id:uuid",
        "session_managed_interrupted_targets.event_floor:bigint",
      ],
      orchestrator: [
        "task_session.task_id:text",
        "task_session.session_id:text",
        "task_session.mcp_managed_lifecycle:jsonb",
      ],
    };
    for (const role of ["coordinator", "orchestrator"] as const) {
      const pool = this[role];
      const tables = [...new Set(expected[role].map((v) => v.split(".")[0]!))];
      const result = await pool.query(
        "SELECT current_database() AS database,table_name,column_name,data_type FROM information_schema.columns WHERE table_schema='public' AND table_name=ANY($1::text[])",
        [tables],
      );
      const rows = z
        .array(
          z.strictObject({
            database: z.string(),
            table_name: z.string(),
            column_name: z.string(),
            data_type: z.string(),
          }),
        )
        .max(256)
        .parse(result.rows);
      requireThat(
        rows.every((v) => v.database === this.databaseName),
        "observer_actual_database",
      );
      const actual = new Set(
        rows.map((v) => `${v.table_name}.${v.column_name}:${v.data_type}`),
      );
      for (const column of expected[role])
        if (!actual.has(column))
          throw new RunnerFailure(
            "NEEDS_CONTEXT",
            `${role}_schema_${column.split(":")[0]}`,
          );
    }
  }
  own(taskId: string, sessionId: string) {
    requireThat(
      /^[A-Za-z0-9_:-]{1,128}$/.test(taskId) &&
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
  async destroyOperation(sessionId: string) {
    this.task(sessionId);
    const result = await this.coordinator.query(
      "SELECT id::text,state FROM session_ops WHERE session_id=$1 AND kind='destroy' ORDER BY id DESC LIMIT 1",
      [sessionId],
    );
    if (result.rowCount === 0) return null;
    requireThat(result.rowCount === 1, "coordinator_destroy_row");
    const parsed = z
      .strictObject({
        id: z.string().regex(/^[1-9][0-9]*$/),
        state: z.enum(["queued", "running", "done", "failed", "cancelled"]),
      })
      .safeParse(result.rows[0]);
    requireThat(parsed.success, "coordinator_destroy_shape");
    return parsed.data;
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
  /** Select only nonsecret assets, start receipt and frozen manifests for the exact current boot. */
  async physicalBoot(sessionId: string, bootAttempt: string) {
    const taskId = this.task(sessionId);
    requireThat(
      /^[A-Za-z0-9-]{1,128}$/.test(bootAttempt),
      "physical_boot_attempt",
    );
    const result = await this.coordinator.query(
      "SELECT i.completed,c.native_version,c.native_bytes,c.native_digest,CASE WHEN octet_length((i.record->'assets')::text)<=65536 THEN (i.record->'assets')::text END AS assets,CASE WHEN octet_length((b.record->'start')::text)<=65536 THEN (b.record->'start')::text END AS start,CASE WHEN octet_length((b.record->'finalization'->'combined_manifest')::text)<=8388608 THEN (b.record->'finalization'->'combined_manifest')::text END AS combined_manifest,b.record->'finalization'->>'combined_manifest_digest' AS combined_manifest_digest FROM session_managed_boot_intents i JOIN session_managed_boots b ON b.session_id=i.session_id AND b.boot_attempt=i.intent_id JOIN session_managed_creations c ON c.session_id=i.session_id WHERE i.session_id=$1 AND c.task_id=$2 AND i.intent_id=$3 AND b.phase='started' AND i.binding_epoch=b.binding_epoch",
      [sessionId, taskId, bootAttempt],
    );
    requireThat(result.rowCount === 1, "physical_current_intent_row");
    return z
      .strictObject({
        completed: z.literal(true),
        native_version: z.literal(1),
        native_bytes: z
          .instanceof(Buffer)
          .refine((v) => v.length > 0 && v.length <= 1048576),
        native_digest: digest,
        assets: z.string().min(1).max(65536),
        start: z.string().min(1).max(65536),
        combined_manifest: z.string().min(1).max(8388608),
        combined_manifest_digest: digest,
      })
      .parse(result.rows[0]);
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
  async deliveryTarget(sessionId: string, callId: string) {
    const taskId = this.task(sessionId);
    requireThat(
      /^[A-Za-z0-9_-]{1,64}$/.test(callId),
      "delivery_target_call_id",
    );
    const result = await this.coordinator.query(
      "SELECT (SELECT min(e.idx)::text FROM session_events e WHERE e.session_id=c.session_id AND e.kind='tool_call_requested' AND e.payload->>'tool_call_id'=$3) AS requested_index,COALESCE((SELECT max(event_floor) FROM session_managed_interrupted_targets WHERE session_id=c.session_id),0)::text AS interruption_floor FROM session_managed_creations c WHERE c.session_id=$1 AND c.task_id=$2",
      [sessionId, taskId, callId],
    );
    requireThat(result.rowCount === 1, "delivery_target_creation");
    const parsed = z
      .strictObject({
        requested_index: z.string().regex(/^[0-9]+$/),
        interruption_floor: z.string().regex(/^[0-9]+$/),
      })
      .safeParse(result.rows[0]);
    requireThat(parsed.success, "delivery_target_actual_request");
    return {
      ...parsed.data,
      targetGone:
        BigInt(parsed.data.requested_index) <
        BigInt(parsed.data.interruption_floor),
    };
  }
  async close() {
    await Promise.all([this.coordinator.end(), this.orchestrator.end()]);
  }
}
