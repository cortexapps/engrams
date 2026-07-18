/**
 * Orchestrator database schema — ADR 0051 §3/§10.
 *
 * Tables:
 *   - task / task_session: task model (Task 15).
 *   - user / session / account / verification: better-auth core tables (Task 16).
 *     The `user` table carries admin-plugin fields (role, banned, banReason,
 *     banExpires) and the `session` table carries `impersonatedBy` from the
 *     admin plugin.
 *
 * Schema generated via:
 *   bunx --bun @better-auth/cli@latest generate \
 *     --config src/auth/better-auth.ts --output /tmp/better-auth-schema.ts --yes
 * then merged manually below (relations included for completeness; drizzle
 * does not require them for queries but they document the FK graph).
 */

import { relations, sql } from "drizzle-orm";
import {
  pgTable,
  text,
  integer,
  jsonb,
  timestamp,
  boolean,
  primaryKey,
  index,
  uniqueIndex,
  customType,
  bigint,
  uuid,
} from "drizzle-orm/pg-core";

/** Raw binary column (Postgres `bytea`). node-postgres maps `bytea` ⇄ Buffer. */
const bytea = customType<{ data: Buffer }>({
  dataType() {
    return "bytea";
  },
});

// ---------------------------------------------------------------------------
// Task model (Task 15)
// ---------------------------------------------------------------------------

export const task = pgTable("task", {
  id: text("id").primaryKey(), // nanoid/uuid
  type: text("type").notNull(), // 'chat' (UI) | 'slack_thread' (ADR 0060 trigger)
  // Session titles. The effective display title is DERIVED (buildTask):
  //   custom_title ?? liveSuggested(session) ?? suggested_title ?? title
  // `title` is the truncated-prompt DEFAULT set at create; `suggested_title`
  // snapshots the coordinator's live harness suggestion so it survives session
  // GC; `custom_title` is the user's STICKY rename (harness suggestions never
  // override it) — null once reset.
  title: text("title"),
  suggestedTitle: text("suggested_title"),
  customTitle: text("custom_title"),
  status: text("status").notNull().default("open"), // open|working|awaiting_review|done|failed
  createdByUserId: text("created_by_user_id"), // better-auth user id; null = automation (future)
  source: jsonb("source"), // type-specific trigger ref
  workflowRunId: text("workflow_run_id"), // DBOS run — null for chat (ADR §4)
  createdAt: timestamp("created_at").notNull().defaultNow(),
  updatedAt: timestamp("updated_at").notNull().defaultNow(),
});

export const taskSession = pgTable(
  "task_session",
  {
    taskId: text("task_id")
      .notNull()
      .references(() => task.id, { onDelete: "cascade" }),
    sessionId: text("session_id").notNull(), // control-plane session id
    role: text("role"), // nullable until multi-session types exist
    // ADR 0053: which profile started this session. Real intra-DB FK (§2).
    // Nullable for pre-feature / out-of-band sessions. Profiles are only ever
    // soft-deleted, so the target always exists; ON DELETE is moot.
    profileId: text("profile_id").references(() => profile.id),
    createdAt: timestamp("created_at").notNull().defaultNow(),
  },
  (t) => [
    primaryKey({ columns: [t.taskId, t.sessionId] }),
    index("task_session_session_idx").on(t.sessionId), // the authz join (ADR §6) hits this
  ],
);

// ---------------------------------------------------------------------------
// Generic tool protocol pending-call ledger (ADR 0089 P1)
// ---------------------------------------------------------------------------

/** Durable lifecycle bookkeeping for generic tool calls. The coordinator event
 *  log remains the wire source of truth; this orchestrator-owned projection
 *  supports external-completion policy and stale session-call watchdogs. */
export const pendingToolCall = pgTable(
  "pending_tool_calls",
  {
    sessionId: text("session_id").notNull(),
    toolCallId: text("tool_call_id").notNull(),
    toolName: text("tool_name").notNull(),
    handling: text("handling").notNull(),
    requestedAt: timestamp("requested_at", { withTimezone: true }).notNull(),
    submittedAt: timestamp("submitted_at", { withTimezone: true }),
    completedAt: timestamp("completed_at", { withTimezone: true }),
  },
  (t) => [
    uniqueIndex("pending_tool_calls_session_tool_call_unique").on(
      t.sessionId,
      t.toolCallId,
    ),
    index("pending_tool_calls_session_idx").on(t.sessionId),
  ],
);

// ---------------------------------------------------------------------------
// Papercuts
// ---------------------------------------------------------------------------

/** Small, concrete sources of friction reported by agents while they work. */
export const papercut = pgTable(
  "papercuts",
  {
    id: text("id").primaryKey(), // uuid string (crypto.randomUUID())
    summary: text("summary").notNull(),
    description: text("description").notNull(),
    category: text("category").notNull(),
    severity: text("severity"),
    tags: jsonb("tags").$type<string[]>().default([]),
    sessionId: text("session_id").notNull(),
    toolCallId: text("tool_call_id"),
    taskId: text("task_id"),
    profileId: text("profile_id"),
    userId: text("user_id"),
    archivedAt: timestamp("archived_at", { withTimezone: true }),
    createdAt: timestamp("created_at", { withTimezone: true }).notNull().defaultNow(),
  },
  (t) => [
    uniqueIndex("papercuts_session_tool_call_unique").on(
      t.sessionId,
      t.toolCallId,
    ),
    index("papercuts_created_at_idx").on(t.createdAt),
    index("papercuts_archived_at_idx").on(t.archivedAt),
  ],
);

// ---------------------------------------------------------------------------
// Pull request references (ADR 0100)
// ---------------------------------------------------------------------------

/** Durable link from a PR observed in a session to the task that authored it. */
export const prRef = pgTable(
  "pr_ref",
  {
    id: text("id").primaryKey(), // uuid string (crypto.randomUUID())
    repo: text("repo").notNull(),
    prNumber: integer("pr_number").notNull(),
    authoringTaskId: text("authoring_task_id").references(() => task.id, {
      onDelete: "set null",
    }),
    sessionId: text("session_id").notNull(),
    title: text("title").notNull(),
    url: text("url").notNull(),
    headBranch: text("head_branch").notNull(),
    baseBranch: text("base_branch").notNull(),
    observedAt: timestamp("observed_at", { withTimezone: true }).notNull(),
  },
  (t) => [
    uniqueIndex("pr_ref_repo_pr_number_unique").on(t.repo, t.prNumber),
    index("pr_ref_authoring_task_idx").on(t.authoringTaskId),
    index("pr_ref_session_idx").on(t.sessionId),
  ],
);

// ---------------------------------------------------------------------------
// Pull request reviews (ADR 0100)
// ---------------------------------------------------------------------------

/** One durable review pass over a pull request at a pinned head SHA. */
export const review = pgTable(
  "review",
  {
    id: uuid("id").primaryKey().defaultRandom(),
    repo: text("repo").notNull(),
    prNumber: integer("pr_number").notNull(),
    taskId: text("task_id")
      .notNull()
      .references(() => task.id),
    headSha: text("head_sha").notNull(),
    baseSha: text("base_sha").notNull(),
    trigger: text("trigger").notNull(),
    status: text("status").notNull().default("queued"), // queued|finding|verifying|posted|failed|superseded|halted
    githubReviewId: text("github_review_id"),
    summaryMd: text("summary_md"),
    createdAt: timestamp("created_at", { withTimezone: true }).notNull().defaultNow(),
    updatedAt: timestamp("updated_at", { withTimezone: true }).notNull().defaultNow(),
  },
  (t) => [
    index("review_repo_pr_number_idx").on(t.repo, t.prNumber),
    index("review_task_idx").on(t.taskId),
  ],
);

/** A finder-reported candidate and its durable lifecycle state. */
export const reviewFinding = pgTable(
  "review_finding",
  {
    id: uuid("id").primaryKey().defaultRandom(),
    reviewId: uuid("review_id")
      .notNull()
      .references(() => review.id, { onDelete: "cascade" }),
    path: text("path").notNull(),
    startLine: integer("start_line"),
    endLine: integer("end_line"),
    side: text("side"),
    category: text("category").notNull(),
    severity: text("severity").notNull(),
    confidence: text("confidence").notNull(),
    title: text("title").notNull(),
    bodyMd: text("body_md").notNull(),
    suggestedFix: text("suggested_fix"),
    evidence: jsonb("evidence").$type<string[]>().notNull().default([]),
    // candidate|confirmed|suppressed_refuted|posted|ui_only|suppressed_by_config|superseded
    state: text("state").notNull().default("candidate"),
    verdictReason: text("verdict_reason"),
    githubThreadId: text("github_thread_id"),
    resolution: text("resolution"),
    sessionId: text("session_id").notNull(),
    toolCallId: text("tool_call_id").notNull(),
    createdAt: timestamp("created_at", { withTimezone: true }).notNull().defaultNow(),
  },
  (t) => [
    uniqueIndex("review_finding_session_tool_call_unique").on(
      t.sessionId,
      t.toolCallId,
    ),
    index("review_finding_review_idx").on(t.reviewId),
  ],
);

/** The first verifier judgment recorded for a finding. */
export const reviewVerdict = pgTable(
  "review_verdict",
  {
    id: uuid("id").primaryKey().defaultRandom(),
    findingId: uuid("finding_id")
      .notNull()
      .references(() => reviewFinding.id, { onDelete: "cascade" }),
    verdict: text("verdict").notNull(), // confirmed|refuted
    confidence: text("confidence").notNull(),
    reasoning: text("reasoning").notNull(),
    sessionId: text("session_id").notNull(),
    toolCallId: text("tool_call_id").notNull(),
    createdAt: timestamp("created_at", { withTimezone: true }).notNull().defaultNow(),
  },
  (t) => [
    uniqueIndex("review_verdict_session_tool_call_unique").on(
      t.sessionId,
      t.toolCallId,
    ),
    uniqueIndex("review_verdict_finding_unique").on(t.findingId),
  ],
);

/** Per-repository PR-review enrollment. The text fields are constrained by
 * ReviewService to triggerMode: auto|manual and autofix: auto|manual|off. */
export const reviewEnrollment = pgTable("review_enrollment", {
  repo: text("repo").primaryKey(), // "owner/name"
  triggerMode: text("trigger_mode").notNull().default("manual"), // auto|manual
  autofix: text("autofix").notNull().default("off"), // auto|manual|off
  profileId: text("profile_id").references(() => profile.id),
  createdAt: timestamp("created_at", { withTimezone: true }).notNull().defaultNow(),
  updatedAt: timestamp("updated_at", { withTimezone: true })
    .notNull()
    .defaultNow()
    .$onUpdate(() => new Date()),
});

// ---------------------------------------------------------------------------
// Stream-fed session listeners (ingest v2)
// ---------------------------------------------------------------------------

/** Desired listener rows also serve as cross-process leases. Terminal rows are
 * retained so a completed session is never accidentally listened to again. */
export const sessionListener = pgTable("session_listeners", {
  sessionId: text("session_id").primaryKey(),
  owner: text("owner"),
  leaseExpiresAt: timestamp("lease_expires_at", { withTimezone: true }),
  terminalAt: timestamp("terminal_at", { withTimezone: true }),
  createdAt: timestamp("created_at", { withTimezone: true }).notNull().defaultNow(),
});

/** Durable progress is independent for each consumer of a session event log. */
export const consumerCursor = pgTable(
  "consumer_cursors",
  {
    sessionId: text("session_id").notNull(),
    consumer: text("consumer").notNull(),
    lastIdx: bigint("last_idx", { mode: "bigint" }).notNull(),
    updatedAt: timestamp("updated_at", { withTimezone: true }).notNull(),
  },
  (t) => [primaryKey({ columns: [t.sessionId, t.consumer] })],
);

/** Slack-backed sessions route listener output into their owning thread
 * workflow mailbox. Absence means the Slack consumer does not apply. */
export const slackSession = pgTable("slack_session", {
  sessionId: text("session_id").primaryKey(),
  threadWfId: text("thread_wf_id").notNull(),
});

/** Review worker sessions route terminal state into their owning review
 * workflow mailbox. Absence means the review consumer does not apply. */
export const reviewSession = pgTable("review_session", {
  sessionId: text("session_id").primaryKey(),
  reviewWorkflowId: text("review_workflow_id").notNull(),
  role: text("role").notNull(), // finder|verifier
  createdAt: timestamp("created_at", { withTimezone: true }).notNull().defaultNow(),
});

// ---------------------------------------------------------------------------
// Session profiles (ADR 0053)
//
// Admin-curated session starting points. Orchestrator-only data — the control
// plane never learns about profiles. `image_id` is a LOGICAL ref to the
// coordinator's enabled_images.id (not a DB FK — different tier, ADR §3);
// integrity is enforced in application code. Soft delete only (deleted_at).
// ---------------------------------------------------------------------------

// ADR 0057: profile-defined egress + secret policy, lifted off the image
// manifest. Carried as jsonb on the profile and compiled into the per-session
// SessionPolicy at create (B2). The org secret store holds the values; a
// ProfileSecret only references one by `ref`.
export interface ProfileNetwork {
  default: "deny" | "allow"; // posture for hosts not matched by an allow entry
  allowHosts: string[]; // exact hostnames
  allowHostPatterns: string[]; // leading-wildcard globs (*.example.com)
}

export interface ProfileSecret {
  ref: string; // org-secret name (the value-store key)
  envVar: string; // env var the value is exposed as
  mode: "broker" | "literal"; // broker = placeholder + proxy substitution
  allowHosts: string[]; // broker-mode substitution hosts
  allowHostPatterns: string[];
}

export const DEFAULT_PROFILE_NETWORK: ProfileNetwork = {
  default: "deny",
  allowHosts: [],
  allowHostPatterns: [],
};

export const profile = pgTable(
  "profile",
  {
    id: text("id").primaryKey(), // uuid string (crypto.randomUUID())
    name: text("name").notNull(),
    description: text("description").notNull().default(""),
    icon: text("icon").notNull().default("Bot"), // lucide icon name
    imageId: text("image_id").notNull(), // logical ref → enabled_images.id (§3)
    // ADR 0062/0063: the default harness (a HarnessCatalogService catalog name)
    // this profile's sessions run, with default model + effort (catalog option
    // ids). `harness` is REQUIRED — a profile always names a concrete harness (the
    // "inherit deployment default" semantics were superseded; existing rows were
    // backfilled to `claude`). model/effort stay nullable → the harness
    // descriptor's defaults. All overridable per session.
    harness: text("harness").notNull(),
    model: text("model"),
    effort: text("effort"),
    includeUserTokens: boolean("include_user_tokens").notNull().default(false),
    envVars: jsonb("env_vars").notNull().default({}), // { KEY: VALUE }
    // ADR 0055: dynamic skill bundle names this profile's sessions mount (e.g.
    // ["skills", "browser"]). Resolved by the coordinator to reserved-slot
    // mounts at session create. Empty = base session (no skills).
    skills: jsonb("skills").$type<string[]>().notNull().default([]),
    // ADR 0056: integration capabilities ("provider:action[@resource]") this
    // profile's sessions are granted. Passed to the coordinator at session create
    // (CreateSessionRequest.capabilities), which binds + (later) clamps. Empty =
    // no third-party integration access.
    capabilities: jsonb("capabilities").$type<string[]>().notNull().default([]),
    // ADR 0057: egress network allow-list (deny by default) + secrets this
    // profile's sessions get, lifted off the image manifest. Additive in B1;
    // compiled into the per-session SessionPolicy + consumed at boot in B2.
    network: jsonb("network").$type<ProfileNetwork>().notNull().default(DEFAULT_PROFILE_NETWORK),
    secrets: jsonb("secrets").$type<ProfileSecret[]>().notNull().default([]),
    // ADR 0060: the org's default profile — a trigger (no UI to pick one) launches
    // its session with this. At most one active default; the store clears the
    // prior when one is set.
    isDefault: boolean("is_default").notNull().default(false),
    // ADR 0064: guest ports auto-exposed (private) for every session from this
    // profile. The orchestrator mints one private port_exposure per declared port
    // at session create (best-effort). Empty = no auto-exposed ports.
    portExposures: jsonb("port_exposures").$type<number[]>().notNull().default([]),
    // System marker (ADR 0100): at most one profile per value; the review
    // workflow finds its profile by this marker, and designated profiles cannot
    // be deleted.
    designation: text("designation"),
    createdAt: timestamp("created_at").notNull().defaultNow(),
    updatedAt: timestamp("updated_at")
      .notNull()
      .defaultNow()
      .$onUpdate(() => new Date()),
    deletedAt: timestamp("deleted_at"), // null = active; soft delete only (§4)
  },
  (t) => [
    uniqueIndex("profile_designation_unique")
      .on(t.designation)
      .where(sql`designation is not null`),
  ],
);

// ---------------------------------------------------------------------------
// Connector catalog (ADR 0057 C1)
// ---------------------------------------------------------------------------

// A custom (admin-authored) connector. The built-ins (`github`/`datadog`) stay
// as read-only file seeds (`src/connectors/*.json`); this table holds ONLY the
// connectors an admin adds via the integrations UI (C3/C4). `config` is the raw
// connector JSON, re-validated by `parseConnector` at load (the admin-trust
// boundary) — never trusted verbatim. `provider` is the registry key; built-ins
// take precedence, so a custom row can't shadow one. Hard delete (no history):
// a removed connector should stop granting capabilities immediately.
export const connector = pgTable("connector", {
  provider: text("provider").primaryKey(),
  config: jsonb("config").notNull(), // the raw Connector JSON (validated at load)
  createdAt: timestamp("created_at").notNull().defaultNow(),
  updatedAt: timestamp("updated_at")
    .notNull()
    .defaultNow()
    .$onUpdate(() => new Date()),
});

// ---------------------------------------------------------------------------
// Connector logo (redesign): optional uploaded brand mark, keyed by provider.
//
// Orchestrator-owned (the coordinator never sees connectors) — a presentation
// overlay on the connector catalog, deliberately NOT part of the connector
// config. One row per provider (built-in or custom) that has an uploaded logo;
// absence ⇒ the renderer falls back to the deterministic monogram. Bytes are
// small (≤512 KB, SVG or square PNG, enforced at the upload RPC) so a `bytea`
// column is the right home — no blob bucket, transactional with the catalog.
// Served via GET /api/v1/integrations/:provider/logo.
export const connectorLogo = pgTable("connector_logo", {
  provider: text("provider").primaryKey(),
  mediaType: text("media_type").notNull(), // "image/svg+xml" | "image/png"
  data: bytea("data").notNull(),
  updatedAt: timestamp("updated_at")
    .notNull()
    .defaultNow()
    .$onUpdate(() => new Date()),
});

// ---------------------------------------------------------------------------
// Port exposures (ADR 0064): live-host a session's guest port at a vanity
// subdomain.
//
// Orchestrator-owned (the coordinator never learns about exposures) — a row maps
// an auto-minted, opaque tri-word `slug` to a `(session, port)` pair the edge
// reverse-proxy (P2b) tunnels to via PortRelayService. `session_id` is a LOGICAL
// ref to the control-plane session (different tier, like profiles' image_id — no
// DB FK). The slug is the routing key (`<slug>.preview.<domain>`); it deliberately
// does NOT encode the port, so it can't be used to scan a session's other ports.
// Hard delete: revoking an exposure must stop serving it immediately (no history).
// `share_token` is a capability for `visibility = "shared"` links (still inside
// the IAP wall); null for `private`. Admins can view any exposure regardless.
// ---------------------------------------------------------------------------

export const portExposure = pgTable(
  "port_exposure",
  {
    slug: text("slug").primaryKey(), // tri-word, e.g. "jumping-fat-kittens"
    sessionId: text("session_id").notNull(), // control-plane session id (logical ref, §3-style)
    port: integer("port").notNull(), // guest TCP port (1..=65535)
    label: text("label").notNull().default(""), // human label, e.g. "Vite dev server"
    ownerUserId: text("owner_user_id")
      .notNull()
      .references(() => user.id, { onDelete: "cascade" }), // creator (= session owner)
    visibility: text("visibility").notNull().default("private"), // "private" | "shared"
    shareToken: text("share_token"), // capability for visibility=shared; null for private
    createdAt: timestamp("created_at").notNull().defaultNow(),
    expiresAt: timestamp("expires_at"), // null = no expiry
  },
  (t) => [
    // One exposure per (session, port) — re-exposing returns the existing slug.
    uniqueIndex("port_exposure_session_port_idx").on(t.sessionId, t.port),
    // List-by-session + cascade-on-session-delete (P2b/cleanup).
    index("port_exposure_session_idx").on(t.sessionId),
    index("port_exposure_owner_idx").on(t.ownerUserId),
  ],
);

// ---------------------------------------------------------------------------
// better-auth core tables + admin plugin fields (Task 16)
//
// Generated by: bunx --bun @better-auth/cli@latest generate
// Schema path:  src/auth/better-auth.ts
// ---------------------------------------------------------------------------

export const user = pgTable("user", {
  id: text("id").primaryKey(),
  name: text("name").notNull(),
  email: text("email").notNull().unique(),
  emailVerified: boolean("email_verified").default(false).notNull(),
  image: text("image"),
  createdAt: timestamp("created_at").notNull(),
  updatedAt: timestamp("updated_at")
    .$onUpdate(() => new Date())
    .notNull(),
  // admin plugin fields:
  role: text("role"),          // 'admin' | 'user' (we read 'user' as "member")
  banned: boolean("banned").default(false),
  banReason: text("ban_reason"),
  banExpires: timestamp("ban_expires"),
});

export const session = pgTable(
  "session",
  {
    id: text("id").primaryKey(),
    expiresAt: timestamp("expires_at").notNull(),
    token: text("token").notNull().unique(),
    createdAt: timestamp("created_at").notNull(),
    updatedAt: timestamp("updated_at")
      .$onUpdate(() => new Date())
      .notNull(),
    ipAddress: text("ip_address"),
    userAgent: text("user_agent"),
    userId: text("user_id")
      .notNull()
      .references(() => user.id, { onDelete: "cascade" }),
    // admin plugin field:
    impersonatedBy: text("impersonated_by"),
  },
  (table) => [index("session_userId_idx").on(table.userId)],
);

export const account = pgTable(
  "account",
  {
    id: text("id").primaryKey(),
    accountId: text("account_id").notNull(),
    providerId: text("provider_id").notNull(),
    userId: text("user_id")
      .notNull()
      .references(() => user.id, { onDelete: "cascade" }),
    accessToken: text("access_token"),
    refreshToken: text("refresh_token"),
    idToken: text("id_token"),
    accessTokenExpiresAt: timestamp("access_token_expires_at"),
    refreshTokenExpiresAt: timestamp("refresh_token_expires_at"),
    scope: text("scope"),
    password: text("password"),
    createdAt: timestamp("created_at").notNull(),
    updatedAt: timestamp("updated_at")
      .$onUpdate(() => new Date())
      .notNull(),
  },
  (table) => [index("account_userId_idx").on(table.userId)],
);

export const verification = pgTable(
  "verification",
  {
    id: text("id").primaryKey(),
    identifier: text("identifier").notNull(),
    value: text("value").notNull(),
    expiresAt: timestamp("expires_at").notNull(),
    createdAt: timestamp("created_at").notNull(),
    updatedAt: timestamp("updated_at")
      .$onUpdate(() => new Date())
      .notNull(),
  },
  (table) => [index("verification_identifier_idx").on(table.identifier)],
);

// ---------------------------------------------------------------------------
// Global API keys — @better-auth/api-key plugin table (ADR 0086)
//
// Field set mirrors the plugin's `apikey` model exactly (the drizzle adapter
// maps by TS property name). `key` is the SHA-256 hash — plaintext is returned
// once at creation and never stored. `referenceId` points at the key's
// dedicated service-account user (`apikey+<uuid>@service.local`), whose `role`
// is the key's authorization level; deleting that user cascades the key row
// (revocation). Admin-only management via ApiKeyService (src/rpc/api-key.ts);
// the plugin's own HTTP endpoints are 404'd in better-auth.ts.
// ---------------------------------------------------------------------------

export const apikey = pgTable(
  "apikey",
  {
    id: text("id").primaryKey(),
    configId: text("config_id").notNull().default("default"),
    name: text("name"),
    start: text("start"), // masked preview (first chars of the plaintext key)
    prefix: text("prefix"),
    key: text("key").notNull(), // SHA-256 hash of the full key
    referenceId: text("reference_id")
      .notNull()
      .references(() => user.id, { onDelete: "cascade" }),
    refillInterval: integer("refill_interval"),
    refillAmount: integer("refill_amount"),
    lastRefillAt: timestamp("last_refill_at"),
    enabled: boolean("enabled").default(true),
    rateLimitEnabled: boolean("rate_limit_enabled").default(false),
    rateLimitTimeWindow: integer("rate_limit_time_window"),
    rateLimitMax: integer("rate_limit_max"),
    requestCount: integer("request_count").default(0),
    remaining: integer("remaining"),
    lastRequest: timestamp("last_request"),
    expiresAt: timestamp("expires_at"), // null = no expiry; expired rows are auto-deleted at verify
    createdAt: timestamp("created_at").notNull(),
    updatedAt: timestamp("updated_at").notNull(),
    permissions: text("permissions"),
    metadata: text("metadata"),
  },
  (t) => [
    index("apikey_reference_id_idx").on(t.referenceId),
    index("apikey_key_idx").on(t.key), // verify path looks up by hash
  ],
);

// ---------------------------------------------------------------------------
// Device-authorization grants — better-auth device-authorization plugin table
// (the `engrams auth login` rail). Field set mirrors the plugin's `deviceCode`
// model exactly (the drizzle adapter maps by TS property name; the exported
// const name MUST be `deviceCode` — that's the model-name lookup key). Rows
// are short-lived (10-minute expiry) and terminal-state rows are deleted by
// the plugin on the token poll that consumes them.
// ---------------------------------------------------------------------------

export const deviceCode = pgTable(
  "device_code",
  {
    id: text("id").primaryKey(),
    deviceCode: text("device_code").notNull(),
    userCode: text("user_code").notNull(),
    // Set at approve time (the approving better-auth user).
    userId: text("user_id"),
    expiresAt: timestamp("expires_at").notNull(),
    // pending | approved | denied.
    status: text("status").notNull(),
    lastPolledAt: timestamp("last_polled_at"),
    pollingInterval: integer("polling_interval"),
    clientId: text("client_id"),
    scope: text("scope"),
  },
  (t) => [
    index("device_code_device_code_idx").on(t.deviceCode), // token-poll lookup
    index("device_code_user_code_idx").on(t.userCode), // approve/verify lookup
  ],
);

// ---------------------------------------------------------------------------
// User session secrets — KEK-envelope sealed at rest (ADR 0051 Drip A)
//
// A generic, env-var-name-keyed per-user secret store. The orchestrator OWNS
// the user's harness identity secrets (today just the Claude
// CLAUDE_CODE_OAUTH_TOKEN) here in its own Postgres, next to the users they
// belong to, replacing the coordinator's per-user sealed SecretService vault.
// At session-create the orchestrator opens all of a user's secrets and passes
// them to the control plane via CreateSession.harness_env; the coordinator
// injects + persists them so they replay on resume.
//
// At-rest posture: every value is KEK-envelope SEALED with the SAME key + SAME
// envelope format as the Rust coordinator (engram-crypto / ENGRAM_KEK_MASTER_KEY)
// — see src/crypto/seal.ts. The row holds ciphertext only; the master key lives
// outside the DB (env var, sourced from GCP Secret Manager in prod). A read of
// the row alone does not yield plaintext. wrapped_dek / nonce / ciphertext are
// stored as base64 `text` (simplest in drizzle/Bun). Plaintext is NEVER logged.
// ---------------------------------------------------------------------------

export const userSessionSecrets = pgTable(
  "user_session_secrets",
  {
    userId: text("user_id")
      .notNull()
      .references(() => user.id, { onDelete: "cascade" }),
    // e.g. "CLAUDE_CODE_OAUTH_TOKEN" — the harness env var this secret feeds.
    envVarName: text("env_var_name").notNull(),
    // KEK-envelope sealed columns (base64 text). Mirrors engram-crypto SealedCred.
    wrappedDek: text("wrapped_dek").notNull(),
    nonce: text("nonce").notNull(),
    ciphertext: text("ciphertext").notNull(),
    keyId: text("key_id").notNull(),
    createdAt: timestamp("created_at").notNull().defaultNow(),
    updatedAt: timestamp("updated_at")
      .notNull()
      .defaultNow()
      .$onUpdate(() => new Date()),
  },
  (t) => [primaryKey({ columns: [t.userId, t.envVarName] })],
);

// ---------------------------------------------------------------------------
// Relations (informational — drizzle does not require these for queries)
// ---------------------------------------------------------------------------

export const userRelations = relations(user, ({ many }) => ({
  sessions: many(session),
  accounts: many(account),
}));

export const sessionRelations = relations(session, ({ one }) => ({
  user: one(user, {
    fields: [session.userId],
    references: [user.id],
  }),
}));

export const accountRelations = relations(account, ({ one }) => ({
  user: one(user, {
    fields: [account.userId],
    references: [user.id],
  }),
}));
