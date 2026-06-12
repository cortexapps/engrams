// SCAFFOLDING: delete in Task 28
//
// Cross-stack parity smoke (ADR 0039 Task 21b).
//
// Reads every overlapping resource through both the legacy axum REST
// (http://127.0.0.1:8090/api/v1/..., unauthenticated in dev AuthMode::None)
// and the new gated /rpc (http://127.0.0.1:8787/rpc/..., admin cookie) and
// diffs. This is the only check that catches a Rust core extraction or
// convert.rs copy silently defaulting a field — the conformance gate in
// Task 18 can't see below the orchestrator.
//
// Usage:
//   bun run --cwd orchestrator scripts/parity.ts
//   just smoke-parity        (requires dev stack up + at least one session)
//
// Exit code: 0 = all probes pass, 1 = any diff or fatal error.
//
// -----------------------------------------------------------------------
// Wire shapes (documented so the normalizer makes sense):
//
// Legacy REST (coordinator axum, :8090):
//   - snake_case keys (serde)
//   - int64 fields are JSON numbers (serde_json)
//   - GET /sessions?scope=all → { sessions: [{ <flat Session fields>,
//       owner_email, owner_name }] }   (serde flatten — session fields inlined)
//   - GET /hosts → { hosts: [HostView, ...] }  (already wrapped)
//   - GET /storage/summary → StorageSummaryResponse flat + rows[]
//   - GET /enabled-images → { images: [...] }
//   - GET /registries → { registries: [...] }   (already wrapped)
//   - GET /sessions/:id/cow-state → { session_id, state: CowStateView | null }
//   - GET /sessions/:id/checkpoints → { session_id, checkpoints: [...] }
//   Timestamps: ISO-8601 "Z" suffix (serde/chrono default)
//   SSE: GET /api/v1/sessions/:id/events?since=-1
//     Frame order (per axum sse): event: <kind>\ndata: <json>\nid: <idx>\n\n
//     The -1 sentinel means "from the start of the log" (events.rs:41).
//
// Connect JSON (/rpc, orchestrator :8787):
//   - camelCase keys (protobuf-es JSON mapping)
//   - int64 fields are JSON strings ("123") per proto JSON spec
//   - ListSessionsResponse.sessions = [{ session: {...}, ownerEmail, ownerName }]
//     (NESTED — session is a sub-object, not flattened)
//   - ListHostsResponse.hosts = [HostView, ...]
//   - GetStorageSummaryResponse = flat + rows[]
//   - ListEnabledImagesResponse.images = [...]
//   - ListRegistriesResponse.registries = [...]
//   - GetCowStateResponse = { sessionId, state?: {...} }
//   - ListCheckpointsResponse = { sessionId, checkpoints: [...] }
//   Timestamps: ISO-8601 "+00:00" suffix (proto JSON mapping)
//   emitDefaults=false: zero-value scalars and empty arrays are OMITTED.
//   SSE: GET /api/v1/sessions/:id/events (no ?since → from start)
//     data: {"idx":<number|null>,"kind":"<kind>","payload_json":"..."}
//     (extra envelope — unwrap before comparing triples)
//
// Sanctioned diffs (NOT bugs — encoded per-probe):
//   - REST sessions carry user_id; proto Session omits it (ADR §2.1 contract
//     narrowing). Drop user_id from REST side before diffing.
//   - owner_kind: TS-mirror-only field, absent on Rust REST wire. Dropped
//     from RPC side if present.
//   - Connect JSON omits default/unset fields (emitDefaults=false); REST
//     may include zero-numbers or empty arrays. Normalizer drops zero-numeric
//     and empty-array values from both sides.
//   - Timestamps: REST uses "Z", proto JSON uses "+00:00". Normalizer
//     canonicalises both to "Z".
//   - live_disk_manifest: REST may include null for sessions without manifest;
//     proto omits it. Covered by absent==null normalizer.
// -----------------------------------------------------------------------

const LEGACY_BASE = "http://127.0.0.1:8090/api/v1";
const ORCH_BASE = "http://127.0.0.1:8787";
const ADMIN_EMAIL = "smoke-admin@engram.local";
const ADMIN_PASS = "SmokeAdmin123!";

// ---------------------------------------------------------------------------
// Admin session
// ---------------------------------------------------------------------------

async function signUpIdempotent(email: string, pass: string, name: string) {
  const res = await fetch(`${ORCH_BASE}/api/auth/sign-up/email`, {
    method: "POST",
    headers: { "Content-Type": "application/json", Origin: "http://localhost:5173" },
    body: JSON.stringify({ email, password: pass, name }),
  });
  // 200 = created; 422 = already exists — both are fine
  if (res.status !== 200 && res.status !== 422) {
    throw new Error(`sign-up failed: ${res.status} ${await res.text()}`);
  }
}

async function promoteToAdmin(email: string): Promise<void> {
  const sql = `UPDATE "user" SET role='admin' WHERE email='${email}'`;
  const proc = Bun.spawn(
    [
      "docker", "compose",
      "-f", "deploy/docker-compose.dev.yml",
      "exec", "-T", "postgres",
      "psql", "-U", "engram", "-d", "engram_orchestrator", "-c", sql,
    ],
    { stdout: "pipe", stderr: "pipe", cwd: ".." },
  );
  const exitCode = await proc.exited;
  if (exitCode !== 0) {
    const stderr = await new Response(proc.stderr).text();
    throw new Error(`admin promotion failed (exit ${exitCode}): ${stderr}`);
  }
}

async function signIn(email: string, pass: string): Promise<string> {
  const res = await fetch(`${ORCH_BASE}/api/auth/sign-in/email`, {
    method: "POST",
    headers: { "Content-Type": "application/json", Origin: "http://localhost:5173" },
    body: JSON.stringify({ email, password: pass }),
  });
  if (res.status !== 200) {
    throw new Error(`sign-in failed: ${res.status} ${await res.text()}`);
  }
  const raw = res.headers.get("set-cookie");
  if (!raw) throw new Error("no Set-Cookie after sign-in");
  const token = raw.split(";")[0]?.trim();
  if (!token) throw new Error("empty Set-Cookie");
  return token;
}

async function getAdminCookie(): Promise<string> {
  await signUpIdempotent(ADMIN_EMAIL, ADMIN_PASS, "Parity Admin");
  await promoteToAdmin(ADMIN_EMAIL);
  return signIn(ADMIN_EMAIL, ADMIN_PASS);
}

// ---------------------------------------------------------------------------
// RPC helper — Connect JSON unary
// ---------------------------------------------------------------------------

async function rpc(
  service: string,
  method: string,
  body: unknown,
  cookie: string,
): Promise<unknown> {
  const res = await fetch(`${ORCH_BASE}/rpc/${service}/${method}`, {
    method: "POST",
    headers: { "Content-Type": "application/json", Cookie: cookie },
    body: JSON.stringify(body),
  });
  if (!res.ok) {
    const text = await res.text();
    throw new Error(`RPC ${service}/${method} failed: ${res.status} ${text}`);
  }
  return res.json();
}

// ---------------------------------------------------------------------------
// Discover session ID
// ---------------------------------------------------------------------------

type SessionDiscovery = {
  sid: string;
  created: boolean;
  taskId?: string;
};

async function discoverSession(adminCookie: string): Promise<SessionDiscovery> {
  // Use ?scope=all so the REST /sessions list returns the same scope as
  // the gRPC ListSessions (which returns ALL sessions — the orchestrator
  // then filters by ownership, but for parity we compare the raw data).
  const res = await fetch(`${LEGACY_BASE}/sessions?scope=all`);
  if (!res.ok) throw new Error(`/sessions failed: ${res.status}`);
  const body = (await res.json()) as { sessions?: Array<{ id?: string }> };
  const sessions = body.sessions ?? [];
  if (sessions.length > 0 && sessions[0]?.id) {
    return { sid: sessions[0].id, created: false };
  }

  // No sessions — create one via orchestrator CreateTask (no-harness image).
  console.log("  No sessions found — creating temp session via CreateTask...");
  const imagesRes = (await rpc(
    "engram.app.v1.ImageService",
    "ListEnabledImages",
    {},
    adminCookie,
  )) as { images?: Array<{ imageUri?: string; harnessName?: string }> };
  const noHarness = (imagesRes.images ?? []).find((img) => !img.harnessName);
  if (!noHarness?.imageUri) {
    throw new Error("No no-harness image found — cannot create temp session");
  }
  const taskRes = (await rpc(
    "engram.app.v1.TaskService",
    "CreateTask",
    { type: "chat", imageUri: noHarness.imageUri, title: "parity-smoke-temp" },
    adminCookie,
  )) as { task?: { id?: string; sessions?: Array<{ sessionId?: string }> } };
  const taskId = taskRes.task?.id;
  const sid = taskRes.task?.sessions?.[0]?.sessionId;
  if (!sid || !taskId) throw new Error("CreateTask did not return session id");
  console.log(`  Created temp session ${sid} (task ${taskId})`);
  return { sid, created: true, taskId };
}

async function cleanupTempSession(taskId: string, adminCookie: string) {
  try {
    await rpc("engram.app.v1.TaskService", "DeleteTask", { taskId }, adminCookie);
    console.log(`  Cleaned up temp task ${taskId}`);
  } catch (e) {
    console.warn(`  Cleanup failed for task ${taskId}: ${e}`);
  }
}

// ---------------------------------------------------------------------------
// Normalization
// ---------------------------------------------------------------------------

/** Convert camelCase key to snake_case. */
function camelToSnake(s: string): string {
  return s
    .replace(/([A-Z]+)([A-Z][a-z])/g, "$1_$2")
    .replace(/([a-z\d])([A-Z])/g, "$1_$2")
    .toLowerCase();
}

/** Recursively convert all object keys from camelCase to snake_case. */
function keysToSnake(val: unknown): unknown {
  if (Array.isArray(val)) return val.map(keysToSnake);
  if (val !== null && typeof val === "object") {
    const out: Record<string, unknown> = {};
    for (const [k, v] of Object.entries(val as Record<string, unknown>)) {
      out[camelToSnake(k)] = keysToSnake(v);
    }
    return out;
  }
  return val;
}

/**
 * Normalize int64: Connect JSON encodes int64 as string ("123");
 * REST encodes as number. Normalize both to number.
 */
function normalizeInt64(val: unknown): unknown {
  if (Array.isArray(val)) return val.map(normalizeInt64);
  if (val !== null && typeof val === "object") {
    const out: Record<string, unknown> = {};
    for (const [k, v] of Object.entries(val as Record<string, unknown>)) {
      out[k] = normalizeInt64(v);
    }
    return out;
  }
  if (typeof val === "string" && /^-?\d+$/.test(val)) {
    const n = Number(val);
    if (!Number.isNaN(n) && Math.abs(n) < Number.MAX_SAFE_INTEGER) return n;
  }
  return val;
}

/**
 * Normalize ISO-8601 timestamps: REST uses "Z" suffix; Connect JSON uses
 * "+00:00" suffix. Canonicalise both to "Z".
 */
function normalizeTimestamps(val: unknown): unknown {
  if (Array.isArray(val)) return val.map(normalizeTimestamps);
  if (val !== null && typeof val === "object") {
    const out: Record<string, unknown> = {};
    for (const [k, v] of Object.entries(val as Record<string, unknown>)) {
      out[k] = normalizeTimestamps(v);
    }
    return out;
  }
  // ISO-8601 timestamp ending in +00:00 → Z
  if (typeof val === "string" && val.endsWith("+00:00")) {
    return val.slice(0, -6) + "Z";
  }
  return val;
}

/** Drop keys recursively by name. */
function dropKeys(val: unknown, keys: string[]): unknown {
  if (!keys.length) return val;
  const set = new Set(keys);
  function walk(v: unknown): unknown {
    if (Array.isArray(v)) return v.map(walk);
    if (v !== null && typeof v === "object") {
      const out: Record<string, unknown> = {};
      for (const [k, iv] of Object.entries(v as Record<string, unknown>)) {
        if (!set.has(k)) out[k] = walk(iv);
      }
      return out;
    }
    return v;
  }
  return walk(val);
}

/** Sort arrays of objects by primary key. */
function sortArrays(val: unknown, sortKeys: string[] = ["id", "snapshot_id", "sandbox_id", "registry_host"]): unknown {
  if (Array.isArray(val)) {
    const sorted = val.map((item) => sortArrays(item, sortKeys)) as unknown[];
    sorted.sort((a, b) => {
      for (const k of sortKeys) {
        const ak = (a as Record<string, unknown>)?.[k];
        const bk = (b as Record<string, unknown>)?.[k];
        if (ak !== undefined && bk !== undefined) {
          return String(ak) < String(bk) ? -1 : String(ak) > String(bk) ? 1 : 0;
        }
      }
      return 0;
    });
    return sorted;
  }
  if (val !== null && typeof val === "object") {
    const out: Record<string, unknown> = {};
    for (const [k, v] of Object.entries(val as Record<string, unknown>)) {
      out[k] = sortArrays(v, sortKeys);
    }
    return out;
  }
  return val;
}

/**
 * Normalize absent/null/undefined to absent, AND drop zero-numeric and
 * empty-array values.
 *
 * Connect JSON (emitDefaults=false) omits:
 *   - zero scalars (0, 0.0, "", false)
 *   - empty repeated fields ([])
 *   - unset optional fields
 *
 * REST (serde) may include all of them. To make both sides comparable,
 * we drop zero-numbers (0), empty strings, and empty arrays from BOTH.
 * This is the "absent == default" rule.
 *
 * NOTE: booleans false are NOT dropped — proto emits false as absent but
 * we keep it to avoid hiding real boolean mismatches. They surface in
 * per-probe volatile lists instead.
 *
 * KNOWN LIMITS of the symmetric drop rule (accepted for scaffolding):
 *
 * (a) A field default-valued on BOTH sides is invisible BY CONSTRUCTION:
 *     Connect JSON (emitDefaults=false) omits it at serialization, before
 *     any normalizer runs. No drop rule — symmetric or asymmetric — can
 *     recover information the wire never carried. Closing this would need
 *     emitDefaultValues:true on the orchestrator or binary comparison;
 *     disproportionate for Task-28-doomed code.
 *
 * (b) For explicit-presence (`optional`) proto fields, Connect JSON DOES
 *     emit set-to-default values (Some(0) → "0"), so dropping defaults
 *     from both sides deliberately erases the Some(0)-vs-None presence
 *     distinction — a converter conflating them (unwrap_or_default class)
 *     passes silently. Accepted: a schema-BLIND asymmetric rule would
 *     false-diff every legitimately-zero optional field, and a
 *     schema-AWARE one means walking protobuf-es descriptors — a rebuild
 *     this scaffolding doesn't earn. convert.rs maps Rust Option → proto
 *     optional directly, keeping the conflation class narrow.
 */
function normalizeDefaults(val: unknown): unknown {
  if (Array.isArray(val)) {
    const items = val.map(normalizeDefaults).filter((v) => v !== undefined);
    // Drop empty arrays (they're the default for repeated fields).
    if (items.length === 0) return undefined;
    return items;
  }
  if (val !== null && typeof val === "object") {
    const out: Record<string, unknown> = {};
    for (const [k, v] of Object.entries(val as Record<string, unknown>)) {
      if (v === null || v === undefined) continue;
      const nv = normalizeDefaults(v);
      if (nv === undefined) continue;
      out[k] = nv;
    }
    return out;
  }
  // Drop numeric zeros and empty strings (proto default values).
  if (typeof val === "number" && val === 0) return undefined;
  if (typeof val === "string" && val === "") return undefined;
  return val;
}

/** Stable JSON stringify (sorts keys for deterministic output). */
function stableStringify(val: unknown): string {
  if (Array.isArray(val)) return `[${val.map(stableStringify).join(",")}]`;
  if (val !== null && typeof val === "object") {
    const keys = Object.keys(val as object).sort();
    const pairs = keys.map(
      (k) => `${JSON.stringify(k)}:${stableStringify((val as Record<string, unknown>)[k])}`,
    );
    return `{${pairs.join(",")}}`;
  }
  return JSON.stringify(val);
}

/** Line-diff between two JSON strings for readable output. */
function simpleDiff(a: string, b: string): string {
  let aLines: string[];
  let bLines: string[];
  try {
    aLines = JSON.stringify(JSON.parse(a), null, 2).split("\n");
    bLines = JSON.stringify(JSON.parse(b), null, 2).split("\n");
  } catch {
    aLines = a.split("\n");
    bLines = b.split("\n");
  }
  const maxLen = Math.max(aLines.length, bLines.length);
  const diffs: string[] = [];
  for (let i = 0; i < maxLen; i++) {
    const al = aLines[i] ?? "(absent)";
    const bl = bLines[i] ?? "(absent)";
    if (al !== bl) {
      diffs.push(`  REST  @${i + 1}: ${al}`);
      diffs.push(`  RPC   @${i + 1}: ${bl}`);
    }
  }
  return diffs.slice(0, 60).join("\n");
}

// ---------------------------------------------------------------------------
// Probe definition
// ---------------------------------------------------------------------------

interface Probe {
  name: string;
  /** Path appended to LEGACY_BASE (e.g. "/sessions?scope=all"). */
  restPath: string;
  /** Fetch the RPC side and return its raw response body. */
  rpcFetch: (adminCookie: string) => Promise<unknown>;
  /** Keys to drop from BOTH sides before diffing (volatile/live data). */
  volatile?: string[];
  /**
   * Reshape the REST response into the canonical form for diffing.
   * Called after all normalization passes.
   */
  reshapeRest?: (data: unknown) => unknown;
  /**
   * Reshape the RPC response into the canonical form for diffing.
   * Called after all normalization passes.
   */
  reshapeRpc?: (data: unknown) => unknown;
  /** Keys to drop only from the REST side (sanctioned diffs). */
  dropRestOnly?: string[];
  /** Keys to drop only from the RPC side (sanctioned diffs). */
  dropRpcOnly?: string[];
}

function buildProbes(sid: string, adminCookie: string): Probe[] {
  return [
    // -----------------------------------------------------------------------
    // sessions
    //
    // REST: GET /sessions?scope=all → { sessions: [{ <flat Session fields>,
    //   owner_email, owner_name }] }  (serde flatten — session fields inlined)
    //   We use scope=all so both sides carry owner identity for comparison.
    //
    // RPC: ListSessionsResponse.sessions = [{ session: {...}, ownerEmail, ownerName }]
    //   (NESTED — session is a sub-object; camel→snake gives owner_email/owner_name)
    //
    // reshapeRest: extract SESSION_CORE_KEYS into a nested { session: {...} }
    //   to match the proto nesting, leaving owner_email/owner_name at item level.
    //
    // dropRestOnly: user_id — ADR §2.1 contract narrowing; proto Session omits it.
    // dropRpcOnly: owner_kind — TS-mirror-only, absent from Rust REST wire.
    // volatile: created_at, last_active_at — may change between calls.
    //   sandbox_id: present in REST when session has a live sandbox, absent
    //   when evicted; proto omits it when unset. Covered by normalizeDefaults
    //   (null → absent) — but keep created_at/last_active_at in volatile since
    //   they can change.
    // -----------------------------------------------------------------------
    {
      name: "sessions",
      restPath: "/sessions?scope=all",
      rpcFetch: (cookie) =>
        rpc("engram.app.v1.SessionService", "ListSessions", {}, cookie),
      volatile: ["created_at", "last_active_at"],
      reshapeRest: (data) => {
        // REST: flat { sessions: [{ id, status, host_id, ..., owner_email, owner_name }] }
        // Target: { sessions: [{ session: { core fields }, owner_email, owner_name }] }
        const SESSION_CORE_KEYS = new Set([
          "id", "status", "host_id", "sandbox_id", "image", "mode",
          "created_at", "last_active_at", "live_disk_manifest",
        ]);
        const d = data as { sessions?: Array<Record<string, unknown>> };
        const sessions = (d.sessions ?? []).map((item) => {
          const session: Record<string, unknown> = {};
          const rest: Record<string, unknown> = {};
          for (const [k, v] of Object.entries(item)) {
            if (SESSION_CORE_KEYS.has(k)) session[k] = v;
            else rest[k] = v;
          }
          return { session, ...rest };
        });
        return { sessions };
      },
      // user_id: ADR §2.1 — proto Session intentionally omits user attribution.
      dropRestOnly: ["user_id"],
      // owner_kind: TS-mirror-only field; Rust REST never serializes it.
      dropRpcOnly: ["owner_kind"],
    },
    // -----------------------------------------------------------------------
    // hosts
    //
    // REST: GET /hosts → { hosts: [HostView, ...] }  (already wrapped)
    // RPC: ListHostsResponse = { hosts: [HostView, ...] }
    // No reshape needed — shapes match after camel→snake + normalizeDefaults.
    //
    // volatile: last_heartbeat_at (ticks every heartbeat), util_cpu_pct (live).
    // -----------------------------------------------------------------------
    {
      name: "hosts",
      restPath: "/hosts",
      rpcFetch: (cookie) =>
        rpc("engram.app.v1.FleetService", "ListHosts", {}, cookie),
      // last_heartbeat_at ticks; util_cpu_pct changes every heartbeat.
      volatile: ["last_heartbeat_at", "util_cpu_pct"],
    },
    // -----------------------------------------------------------------------
    // storage/summary
    //
    // REST: GET /storage/summary → StorageSummaryResponse (flat + rows[])
    // RPC: GetStorageSummaryResponse (same shape, camelCase)
    // normalizeDefaults removes zero-scalars and empty rows[] from REST so
    // they match the proto's emitDefaults=false output.
    //
    // volatile: dirty_chunks, unflushed_bytes, avg_locality_pct (live COW data).
    // -----------------------------------------------------------------------
    {
      name: "storage",
      restPath: "/storage/summary",
      rpcFetch: (cookie) =>
        rpc("engram.app.v1.FleetService", "GetStorageSummary", {}, cookie),
      volatile: ["dirty_chunks", "unflushed_bytes", "avg_locality_pct", "last_flush_at"],
    },
    // -----------------------------------------------------------------------
    // enabled-images
    //
    // REST: GET /enabled-images → { images: [...] }
    // RPC: ListEnabledImagesResponse = { images: [...] }
    // No reshape needed.
    // -----------------------------------------------------------------------
    {
      name: "images",
      restPath: "/enabled-images",
      rpcFetch: (cookie) =>
        rpc("engram.app.v1.ImageService", "ListEnabledImages", {}, cookie),
    },
    // -----------------------------------------------------------------------
    // registries
    //
    // REST: GET /registries → { registries: [...] }  (already wrapped)
    // RPC: ListRegistriesResponse = { registries: [...] }
    // No reshape needed — shapes match.
    // normalizeDefaults makes empty registries == absent registries key.
    // -----------------------------------------------------------------------
    {
      name: "registries",
      restPath: "/registries",
      rpcFetch: (cookie) =>
        rpc("engram.app.v1.ImageService", "ListRegistries", {}, cookie),
    },
    // -----------------------------------------------------------------------
    // cow-state (session-scoped)
    //
    // REST: GET /sessions/:id/cow-state → { session_id, state: CowStateView | null }
    // RPC: GetCowStateResponse = { sessionId, state?: CowStateView }
    // normalizeDefaults handles state: null → absent.
    //
    // volatile: last_flush_at (ticks on each flush), dirty_chunks, dirty_bytes,
    //   disk_manifest_version, disk_manifest_id (all change with COW activity).
    // -----------------------------------------------------------------------
    {
      name: "cow-state",
      restPath: `/sessions/${sid}/cow-state`,
      rpcFetch: (cookie) =>
        rpc("engram.app.v1.SessionService", "GetCowState", { sessionId: sid }, cookie),
      volatile: [
        "last_flush_at", "dirty_chunks", "dirty_bytes",
        "disk_manifest_version", "disk_manifest_id",
      ],
    },
    // -----------------------------------------------------------------------
    // checkpoints (session-scoped)
    //
    // REST: GET /sessions/:id/checkpoints → { session_id, checkpoints: [...] }
    // RPC: ListCheckpointsResponse = { sessionId, checkpoints: [...] }
    // No reshape needed (after camel→snake normalization).
    // -----------------------------------------------------------------------
    {
      name: "checkpoints",
      restPath: `/sessions/${sid}/checkpoints`,
      rpcFetch: (cookie) =>
        rpc("engram.app.v1.SessionService", "ListCheckpoints", { sessionId: sid }, cookie),
    },
  ];
}

// ---------------------------------------------------------------------------
// Run a single probe
// ---------------------------------------------------------------------------

type ProbeResult = {
  name: string;
  pass: boolean;
  diff?: string;
  error?: string;
};

async function runProbe(probe: Probe, adminCookie: string): Promise<ProbeResult> {
  try {
    const [restRes, rpcRaw] = await Promise.all([
      fetch(`${LEGACY_BASE}${probe.restPath}`).then(async (r) => {
        if (!r.ok) throw new Error(`REST ${probe.restPath} failed: ${r.status} ${await r.text()}`);
        return r.json();
      }),
      probe.rpcFetch(adminCookie),
    ]);

    // --- REST normalization pipeline ---
    // 1. keys already snake_case
    // 2. normalize int64 (REST uses numbers, safe no-op)
    // 3. normalize timestamps (Z → Z no-op for REST, but symmetric)
    let restNorm: unknown = normalizeInt64(restRes);
    restNorm = normalizeTimestamps(restNorm);

    // --- RPC normalization pipeline ---
    // 1. camel→snake keys
    // 2. int64 string → number
    // 3. timestamp +00:00 → Z
    let rpcNorm: unknown = keysToSnake(rpcRaw);
    rpcNorm = normalizeInt64(rpcNorm);
    rpcNorm = normalizeTimestamps(rpcNorm);

    // Apply reshapes
    if (probe.reshapeRest) restNorm = probe.reshapeRest(restNorm);
    if (probe.reshapeRpc) rpcNorm = probe.reshapeRpc(rpcNorm);

    // Drop volatile keys from both sides
    const vol = probe.volatile ?? [];
    restNorm = dropKeys(restNorm, vol);
    rpcNorm = dropKeys(rpcNorm, vol);

    // Drop sanctioned-diff keys
    if (probe.dropRestOnly?.length) restNorm = dropKeys(restNorm, probe.dropRestOnly);
    if (probe.dropRpcOnly?.length) rpcNorm = dropKeys(rpcNorm, probe.dropRpcOnly);

    // Normalize default values (absent == 0 == "" == []) — Connect JSON
    // omits proto defaults; REST includes them explicitly.
    restNorm = normalizeDefaults(restNorm);
    rpcNorm = normalizeDefaults(rpcNorm);

    // Sort arrays by primary key
    restNorm = sortArrays(restNorm);
    rpcNorm = sortArrays(rpcNorm);

    const restStr = stableStringify(restNorm);
    const rpcStr = stableStringify(rpcNorm);

    if (restStr === rpcStr) {
      return { name: probe.name, pass: true };
    }

    const diff = simpleDiff(restStr, rpcStr);
    return {
      name: probe.name,
      pass: false,
      diff: `REST vs RPC diff:\n${diff}\n\nREST (normalized):\n${JSON.stringify(JSON.parse(restStr), null, 2).slice(0, 1000)}\n\nRPC (normalized):\n${JSON.stringify(JSON.parse(rpcStr), null, 2).slice(0, 1000)}`,
    };
  } catch (e) {
    return { name: probe.name, pass: false, error: String(e) };
  }
}

// ---------------------------------------------------------------------------
// SSE probe
// ---------------------------------------------------------------------------

type SseEvent = { idx: number; kind: string; payload: string };

/**
 * Parse SSE blocks from a text stream.
 *
 * SSE spec: blank-line-delimited blocks. Each block may have
 * multiple field lines. Fields within a block may appear in any order.
 * We accumulate all field values for the block and emit an event when
 * the block ends (blank line).
 *
 * Legacy coordinator frame order (per axum SSE builder):
 *   event: <kind>
 *   data: <payload JSON>
 *   id: <idx>
 *   <blank line>
 */
function parseSseBlock(fields: string[]): SseEvent | null {
  let id: string | undefined;
  let event: string | undefined;
  let data: string | undefined;
  for (const line of fields) {
    if (line.startsWith("id:")) id = line.slice(3).trim();
    else if (line.startsWith("event:")) event = line.slice(6).trim();
    else if (line.startsWith("data:")) data = line.slice(5).trim();
  }
  if (id === undefined || event === undefined || data === undefined) return null;
  if (event === "ping" || event === "") return null; // keepalive
  const idx = Number(id);
  if (Number.isNaN(idx)) return null;
  return { idx, kind: event, payload: data };
}

/**
 * Collect up to N events from the legacy coordinator SSE stream.
 *
 * Legacy wire shape:
 *   event: <kind>
 *   data: <payload JSON string>
 *   id: <idx>
 *   <blank line>
 *
 * No auth needed — coordinator runs AuthMode::None in dev.
 * ?since=-1 = from the start of the log (events.rs:41 -1 sentinel).
 */
async function collectLegacySse(sessionId: string, n: number, timeoutMs: number): Promise<SseEvent[]> {
  const ac = new AbortController();
  const timer = setTimeout(() => ac.abort(), timeoutMs);

  const events: SseEvent[] = [];
  try {
    const res = await fetch(`${LEGACY_BASE}/sessions/${sessionId}/events?since=-1`, {
      signal: ac.signal,
    });
    if (!res.ok) throw new Error(`legacy SSE failed: ${res.status}`);
    if (!res.body) throw new Error("legacy SSE: no body");

    const reader = res.body.getReader();
    const decoder = new TextDecoder();
    let buf = "";
    let currentBlock: string[] = [];

    outer: while (true) {
      const { value, done } = await reader.read();
      if (done) break;
      buf += decoder.decode(value, { stream: true });
      const lines = buf.split("\n");
      buf = lines.pop() ?? "";
      for (const line of lines) {
        if (line === "" || line === "\r") {
          // End of event block
          if (currentBlock.length > 0) {
            const ev = parseSseBlock(currentBlock);
            if (ev) {
              events.push(ev);
              if (events.length >= n) break outer;
            }
            currentBlock = [];
          }
        } else {
          currentBlock.push(line);
        }
      }
    }
  } catch (e) {
    const err = e as Error;
    if (err.name !== "AbortError" && !String(err).includes("aborted")) throw e;
  } finally {
    clearTimeout(timer);
  }
  return events;
}

/**
 * Collect up to N events from the orchestrator SSE stream.
 *
 * Orchestrator wire shape (events.ts):
 *   id: <idx>          (omitted for lagged frames)
 *   event: <kind>
 *   data: {"idx":<number|null>,"kind":"<kind>","payload_json":"..."}
 *
 * No ?since param → from start (the orchestrator route treats absent since
 * as BigInt undefined = no cursor = replay all events).
 */
async function collectOrchestratorSse(
  sessionId: string,
  adminCookie: string,
  n: number,
  timeoutMs: number,
): Promise<SseEvent[]> {
  const ac = new AbortController();
  const timer = setTimeout(() => ac.abort(), timeoutMs);

  const events: SseEvent[] = [];
  try {
    // No ?since param — orchestrator treats absence as "from start".
    const res = await fetch(`${ORCH_BASE}/api/v1/sessions/${sessionId}/events`, {
      headers: { Cookie: adminCookie },
      signal: ac.signal,
    });
    if (!res.ok) throw new Error(`orchestrator SSE failed: ${res.status}`);
    if (!res.body) throw new Error("orchestrator SSE: no body");

    const reader = res.body.getReader();
    const decoder = new TextDecoder();
    let buf = "";
    let currentBlock: string[] = [];

    outer: while (true) {
      const { value, done } = await reader.read();
      if (done) break;
      buf += decoder.decode(value, { stream: true });
      const lines = buf.split("\n");
      buf = lines.pop() ?? "";
      for (const line of lines) {
        if (line === "" || line === "\r") {
          if (currentBlock.length > 0) {
            // Find the data line and parse the orchestrator envelope.
            // data: {"idx":<number|null>,"kind":"<kind>","payload_json":"..."}
            const dataLine = currentBlock.find((l) => l.startsWith("data:"));
            if (dataLine) {
              const raw = dataLine.slice(5).trim();
              try {
                const envelope = JSON.parse(raw) as {
                  idx?: number | null;
                  kind?: string;
                  payload_json?: string;
                };
                if (
                  envelope.kind &&
                  envelope.kind !== "ping" &&
                  envelope.idx !== null &&
                  envelope.idx !== undefined
                ) {
                  events.push({
                    idx: envelope.idx,
                    kind: envelope.kind,
                    payload: envelope.payload_json ?? "",
                  });
                  if (events.length >= n) break outer;
                }
              } catch {
                // skip malformed frame
              }
            }
            currentBlock = [];
          }
        } else {
          currentBlock.push(line);
        }
      }
    }
  } catch (e) {
    const err = e as Error;
    if (err.name !== "AbortError" && !String(err).includes("aborted")) throw e;
  } finally {
    clearTimeout(timer);
  }
  return events;
}

/**
 * Normalize event payload for comparison.
 * Legacy REST: raw coordinator payload JSON with _recovery_epoch/_rewound injected.
 * Orchestrator: payload_json from the proto SessionEvent — same JSON string
 *   passed through (the coordinator inject _recovery_epoch/_rewound; the
 *   orchestrator streams this verbatim via StreamEvents).
 *
 * Both sides should carry identical JSON strings if the converter is correct.
 * Parse both and compare structurally (stable stringify) to be whitespace-tolerant.
 */
function normalizeEventPayload(payload: string): string {
  try {
    return stableStringify(JSON.parse(payload));
  } catch {
    return payload;
  }
}

async function runSseProbe(sid: string, adminCookie: string): Promise<ProbeResult> {
  // Plan says first 20 events; a quiet demo session usually has fewer, so
  // both collectors stop early at whatever arrives before TIMEOUT — the
  // comparison below requires the two SEQUENCES to match, not the count.
  const N = 20;
  const TIMEOUT = 10_000;

  let legacyEvents: SseEvent[];
  let orchEvents: SseEvent[];

  try {
    [legacyEvents, orchEvents] = await Promise.all([
      collectLegacySse(sid, N, TIMEOUT),
      collectOrchestratorSse(sid, adminCookie, N, TIMEOUT),
    ]);
  } catch (e) {
    return { name: "sse", pass: false, error: String(e) };
  }

  if (legacyEvents.length === 0 && orchEvents.length === 0) {
    console.log("  SSE: both feeds empty (session has no events) — skipping comparison");
    return { name: "sse", pass: true };
  }

  if (legacyEvents.length === 0 || orchEvents.length === 0) {
    return {
      name: "sse",
      pass: false,
      diff: `Legacy returned ${legacyEvents.length} events, orchestrator returned ${orchEvents.length} events; cannot compare`,
    };
  }

  const compareN = Math.min(N, legacyEvents.length, orchEvents.length);
  const diffs: string[] = [];
  for (let i = 0; i < compareN; i++) {
    const le = legacyEvents[i]!;
    const oe = orchEvents[i]!;
    const lPayload = normalizeEventPayload(le.payload);
    const oPayload = normalizeEventPayload(oe.payload);
    if (le.idx !== oe.idx || le.kind !== oe.kind || lPayload !== oPayload) {
      diffs.push(
        `  event[${i}]: legacy=(idx=${le.idx},kind=${le.kind}) vs orch=(idx=${oe.idx},kind=${oe.kind})`,
      );
      if (lPayload !== oPayload) {
        diffs.push(`    payload legacy: ${le.payload.slice(0, 200)}`);
        diffs.push(`    payload orch:   ${oe.payload.slice(0, 200)}`);
      }
    }
  }

  if (diffs.length > 0) {
    return {
      name: "sse",
      pass: false,
      diff: `SSE triple mismatches (first ${compareN} events):\n${diffs.join("\n")}`,
    };
  }

  console.log(
    `  SSE: ${compareN} events matched (legacy collected=${legacyEvents.length}, orch collected=${orchEvents.length})`,
  );
  return { name: "sse", pass: true };
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

async function main() {
  console.log("=== parity smoke (ADR 0039 Task 21b) ===\n");

  // 1. Admin cookie.
  console.log("Provisioning admin cookie...");
  let adminCookie: string;
  try {
    adminCookie = await getAdminCookie();
  } catch (e) {
    console.error(`FATAL: Could not get admin cookie: ${e}`);
    process.exit(1);
  }
  console.log("  Admin cookie acquired.\n");

  // 2. Discover session.
  console.log("Discovering session...");
  let discovery: SessionDiscovery;
  try {
    discovery = await discoverSession(adminCookie);
  } catch (e) {
    console.error(`FATAL: Could not discover session: ${e}`);
    process.exit(1);
  }
  console.log(
    `  Session: ${discovery.sid}${discovery.created ? " (created for this run)" : " (existing)"}\n`,
  );

  // 3. Build and run probes.
  const probes = buildProbes(discovery.sid, adminCookie);
  const results: ProbeResult[] = [];

  console.log("Running probes...");
  for (const probe of probes) {
    process.stdout.write(`  ${probe.name.padEnd(14)} ... `);
    const result = await runProbe(probe, adminCookie);
    results.push(result);
    console.log(result.pass ? "✓" : `✗${result.error ? ` (error: ${result.error})` : ""}`);
  }

  // 4. SSE probe.
  process.stdout.write(`  ${"sse".padEnd(14)} ... `);
  const sseResult = await runSseProbe(discovery.sid, adminCookie);
  results.push(sseResult);
  console.log(sseResult.pass ? "✓" : `✗${sseResult.error ? ` (error: ${sseResult.error})` : ""}`);

  // 5. Cleanup.
  if (discovery.created && discovery.taskId) {
    await cleanupTempSession(discovery.taskId, adminCookie);
  }

  // 6. Summary table.
  console.log("\n--- Results ---");
  const colWidth = 14;
  const failures: ProbeResult[] = [];
  for (const r of results) {
    const icon = r.pass ? "✓" : "✗";
    console.log(`  ${icon} ${r.name.padEnd(colWidth)}`);
    if (!r.pass) failures.push(r);
  }

  // 7. Print diffs.
  if (failures.length > 0) {
    console.log("\n--- Diffs ---");
    for (const f of failures) {
      console.log(`\n[${f.name}]`);
      if (f.error) console.log(`  Error: ${f.error}`);
      if (f.diff) console.log(f.diff);
    }
    console.log(`\n${failures.length}/${results.length} probe(s) FAILED`);
    process.exit(1);
  }

  console.log(`\nAll ${results.length}/${results.length} probes PASSED`);
  process.exit(0);
}

main().catch((e) => {
  console.error("Unexpected error:", e);
  process.exit(1);
});
