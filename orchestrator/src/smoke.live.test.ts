/**
 * Orchestrator live smoke test (ADR 0039 Tasks 18 & 19).
 *
 * Env-gated: set SMOKE=1 to enable.
 * Default ORCHESTRATOR_URL: http://127.0.0.1:8787
 *
 * Requires a full stack running:
 *   - Orchestrator on ORCHESTRATOR_URL (default http://127.0.0.1:8787)
 *   - Postgres accessible via docker compose (for admin promotion)
 *   - Control plane reachable from the orchestrator
 *
 * Tests (printed as human-readable gate lines):
 *   1.  healthz → 200 {"ok":true,"db":true}
 *   2.  Provision a fresh MEMBER user (smoke-member-<ts>@engram.local);
 *       capture the session cookie from Set-Cookie.
 *   3–6. Authz matrix (passthrough gate):
 *       3. anonymous  GetSession           → HTTP 401 (code: unauthenticated)
 *       4. member     GetSession (bad id)  → 404 (code: not_found; anti-enum)
 *       5. member     ListHosts            → 403 (code: permission_denied)
 *       6. member     ListEnabledImages    → 200
 *   7.  Admin half:
 *       a. Provision smoke-admin@engram.local (sign-up; tolerate already-exists).
 *       b. Promote via docker compose exec psql (Bun.spawn; idempotent).
 *       c. Fresh sign-in → admin cookie.
 *       d. Admin ListHosts → 200 with hosts array.
 *   8.  Task lifecycle (native TaskService — Task 19):
 *       8. CreateTask(chat, no-harness image) → task returned with live session state
 *       9. ListTasks → task visible with session; GetTask → task returned
 *      10. DeleteTask → ListTasks empty (member's view of created task)
 *
 * Honest skip: tests skip without SMOKE=1 (bun test --cwd orchestrator
 * passes 0 failures even without the stack running).
 */

import { expect, test, describe, beforeAll } from "bun:test";

const SMOKE = process.env["SMOKE"] === "1";

const BASE =
  process.env["ORCHESTRATOR_URL"] ?? "http://127.0.0.1:8787";

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/**
 * Extract the Set-Cookie header value for `better-auth.session_token` from a
 * fetch Response. Returns the raw `name=value` pair (without attributes) so
 * it can be forwarded verbatim in subsequent Cookie headers.
 */
function extractSessionCookie(res: Response): string {
  // fetch merges multiple Set-Cookie into a single comma-joined header value.
  // better-auth only sets one cookie, so a simple grab is fine.
  const raw = res.headers.get("set-cookie");
  if (!raw) throw new Error("No Set-Cookie in response");
  // Strip attributes (everything after the first `;`)
  const token = raw.split(";")[0]?.trim();
  if (!token) throw new Error("Empty Set-Cookie header");
  return token;
}

/**
 * POST to an RPC method with Connect-protocol-style JSON body.
 * Sets Content-Type: application/json (the Connect adapter accepts this for
 * unary requests when no framing is needed).
 */
async function rpc(
  service: string,
  method: string,
  body: unknown,
  cookie?: string,
): Promise<Response> {
  const headers: Record<string, string> = {
    "Content-Type": "application/json",
  };
  if (cookie) headers["Cookie"] = cookie;
  return fetch(`${BASE}/rpc/${service}/${method}`, {
    method: "POST",
    headers,
    body: JSON.stringify(body),
  });
}

/**
 * Promote email to admin via docker compose exec psql.
 * Idempotent — runs UPDATE even if already admin.
 */
async function promoteToAdmin(email: string): Promise<void> {
  const sql = `UPDATE "user" SET role='admin' WHERE email='${email}'`;
  const proc = Bun.spawn(
    [
      "docker",
      "compose",
      "-f",
      "../deploy/docker-compose.dev.yml",
      "exec",
      "-T",
      "postgres",
      "psql",
      "-U",
      "engram",
      "-d",
      "engram_orchestrator",
      "-c",
      sql,
    ],
    { stdout: "pipe", stderr: "pipe" },
  );
  const exitCode = await proc.exited;
  if (exitCode !== 0) {
    const stderr = await new Response(proc.stderr).text();
    throw new Error(`Admin promotion failed (exit ${exitCode}): ${stderr}`);
  }
}

// ---------------------------------------------------------------------------
// Smoke suite
// ---------------------------------------------------------------------------

describe("orchestrator live smoke (SMOKE=1 to enable)", () => {
  // -------------------------------------------------------------------------
  // 1. Health check
  // -------------------------------------------------------------------------
  test.skipIf(!SMOKE)("1/10 healthz → 200 {ok:true,db:true}", async () => {
    const res = await fetch(`${BASE}/healthz`);
    expect(res.status).toBe(200);
    const body = await res.json();
    expect(body).toMatchObject({ ok: true, db: true });
    console.log("Smoke 1/10 PASS: healthz → 200 {ok:true,db:true}");
  });

  // -------------------------------------------------------------------------
  // 2–6. Member-provisioning + authz matrix
  // -------------------------------------------------------------------------

  // Cookie captured during the provisioning test and shared with the matrix tests.
  let memberCookie = "";

  test.skipIf(!SMOKE)(
    "2/10 provision fresh MEMBER user and capture session cookie",
    async () => {
      const ts = Date.now();
      const email = `smoke-member-${ts}@engram.local`;
      const res = await fetch(`${BASE}/api/auth/sign-up/email`, {
        method: "POST",
        headers: {
          "Content-Type": "application/json",
          Origin: "http://localhost:5173",
        },
        body: JSON.stringify({
          email,
          password: "SmokeTest123!",
          name: "Smoke Member",
        }),
      });
      expect(res.status).toBe(200);
      memberCookie = extractSessionCookie(res);
      expect(memberCookie).toMatch(/^better-auth\.session_token=/);
      console.log(`Smoke 2/10 PASS: provisioned member ${email}`);
    },
  );

  test.skipIf(!SMOKE)(
    "3/10 anonymous GetSession → 401 (unauthenticated)",
    async () => {
      const res = await rpc(
        "engram.app.v1.SessionService",
        "GetSession",
        { sessionId: crypto.randomUUID() },
        // No cookie — anonymous
      );
      // Connect returns 401 for Unauthenticated
      expect(res.status).toBe(401);
      const body = (await res.json()) as { code?: string };
      expect(body.code).toBe("unauthenticated");
      console.log(
        "Smoke 3/10 PASS: anonymous GetSession → 401 unauthenticated",
      );
    },
  );

  test.skipIf(!SMOKE)(
    "4/10 member GetSession (non-existent id) → 404 not_found (anti-enum)",
    async () => {
      const res = await rpc(
        "engram.app.v1.SessionService",
        "GetSession",
        { sessionId: crypto.randomUUID() },
        memberCookie,
      );
      // Connect returns 404 for NotFound
      expect(res.status).toBe(404);
      const body = (await res.json()) as { code?: string };
      expect(body.code).toBe("not_found");
      console.log("Smoke 4/10 PASS: member GetSession (bad id) → 404 not_found");
    },
  );

  test.skipIf(!SMOKE)(
    "5/10 member ListHosts → 403 permission_denied",
    async () => {
      const res = await rpc(
        "engram.app.v1.FleetService",
        "ListHosts",
        {},
        memberCookie,
      );
      expect(res.status).toBe(403);
      const body = (await res.json()) as { code?: string };
      expect(body.code).toBe("permission_denied");
      console.log("Smoke 5/10 PASS: member ListHosts → 403 permission_denied");
    },
  );

  test.skipIf(!SMOKE)(
    "6/10 member ListEnabledImages → 200 (member-readable)",
    async () => {
      const res = await rpc(
        "engram.app.v1.ImageService",
        "ListEnabledImages",
        {},
        memberCookie,
      );
      expect(res.status).toBe(200);
      const body = (await res.json()) as { images?: unknown[] };
      expect(Array.isArray(body.images)).toBe(true);
      console.log(
        `Smoke 6/10 PASS: member ListEnabledImages → 200 (${(body.images ?? []).length} image(s))`,
      );
    },
  );

  // -------------------------------------------------------------------------
  // 7. Admin half
  // -------------------------------------------------------------------------
  test.skipIf(!SMOKE)(
    "7/10 admin ListHosts → 200 with hosts array",
    async () => {
      const ADMIN_EMAIL = "smoke-admin@engram.local";
      const ADMIN_PASS = "SmokeAdmin123!";

      // a. Provision smoke-admin (idempotent — tolerate already-exists 422).
      const signUpRes = await fetch(`${BASE}/api/auth/sign-up/email`, {
        method: "POST",
        headers: {
          "Content-Type": "application/json",
          Origin: "http://localhost:5173",
        },
        body: JSON.stringify({
          email: ADMIN_EMAIL,
          password: ADMIN_PASS,
          name: "Smoke Admin",
        }),
      });
      // 200 = created now; 422 = already exists — both are acceptable.
      if (signUpRes.status !== 200 && signUpRes.status !== 422) {
        throw new Error(`Unexpected sign-up status: ${signUpRes.status}`);
      }

      // b. Promote via docker compose exec psql (idempotent UPDATE).
      //    Documented: this runs in the test setup to flip role='admin' in the
      //    better-auth `user` table. Idempotent — re-running is safe.
      await promoteToAdmin(ADMIN_EMAIL);

      // c. Fresh sign-in so the new session is created after promotion.
      //    better-auth reads role from the DB on getSession, so a pre-promotion
      //    session might work too — but a fresh sign-in is the safe path.
      const signInRes = await fetch(`${BASE}/api/auth/sign-in/email`, {
        method: "POST",
        headers: {
          "Content-Type": "application/json",
          Origin: "http://localhost:5173",
        },
        body: JSON.stringify({ email: ADMIN_EMAIL, password: ADMIN_PASS }),
      });
      expect(signInRes.status).toBe(200);
      const adminCookie = extractSessionCookie(signInRes);

      // d. Admin ListHosts → 200 with hosts array.
      const res = await rpc(
        "engram.app.v1.FleetService",
        "ListHosts",
        {},
        adminCookie,
      );
      expect(res.status).toBe(200);
      const body = (await res.json()) as { hosts?: unknown[] };
      expect(Array.isArray(body.hosts)).toBe(true);
      console.log(
        `Smoke 7/10 PASS: admin ListHosts → 200 (${(body.hosts ?? []).length} host(s))`,
      );
    },
  );

  // -------------------------------------------------------------------------
  // 8–10. Task lifecycle — native TaskService (Task 19)
  //
  // Uses the member account provisioned in test 2 (memberCookie).
  // Picks the first no-harness image from ListEnabledImages (harnessName null/
  // absent). The created task id is shared across tests 8–10.
  // -------------------------------------------------------------------------

  let smokeTaskId = "";

  test.skipIf(!SMOKE)(
    "8/10 member CreateTask(chat, no-harness image) → task with live session state",
    async () => {
      // Pick a no-harness image.
      const imagesRes = await rpc(
        "engram.app.v1.ImageService",
        "ListEnabledImages",
        {},
        memberCookie,
      );
      expect(imagesRes.status).toBe(200);
      const imagesBody = (await imagesRes.json()) as { images?: Array<{ imageUri?: string; harnessName?: string }> };
      const images = imagesBody.images ?? [];
      const noHarnessImage = images.find((img) => !img.harnessName);
      if (!noHarnessImage?.imageUri) {
        throw new Error(
          "No no-harness image found in ListEnabledImages — stack may not have images enabled",
        );
      }

      const createRes = await rpc(
        "engram.app.v1.TaskService",
        "CreateTask",
        { type: "chat", imageUri: noHarnessImage.imageUri, title: "Smoke task" },
        memberCookie,
      );
      expect(createRes.status).toBe(200);
      const createBody = (await createRes.json()) as {
        task?: { id?: string; type?: string; status?: string; sessions?: unknown[] };
      };
      expect(createBody.task).toBeDefined();
      expect(createBody.task!.type).toBe("chat");
      // Status should be "working" (session just created → pending/created → working).
      // Accept "open" as well in case the session transitions faster than the
      // status read (unlikely but possible in a slow CI).
      const taskStatus: string = createBody.task!.status ?? "open";
      expect(["working", "open"]).toContain(taskStatus);
      expect(Array.isArray(createBody.task!.sessions)).toBe(true);
      expect((createBody.task!.sessions ?? []).length).toBeGreaterThan(0);

      smokeTaskId = createBody.task!.id!;
      expect(smokeTaskId).toBeTruthy();
      console.log(
        `Smoke 8/10 PASS: CreateTask → task ${smokeTaskId} status=${createBody.task!.status} sessions=${(createBody.task!.sessions ?? []).length}`,
      );
    },
  );

  test.skipIf(!SMOKE)(
    "9/10 ListTasks shows created task with session; GetTask returns it",
    async () => {
      // ListTasks.
      const listRes = await rpc(
        "engram.app.v1.TaskService",
        "ListTasks",
        {},
        memberCookie,
      );
      expect(listRes.status).toBe(200);
      const listBody = (await listRes.json()) as { tasks?: Array<{ id?: string; sessions?: unknown[] }> };
      const tasks = listBody.tasks ?? [];
      const found = tasks.find((t) => t.id === smokeTaskId);
      expect(found).toBeDefined();
      expect(Array.isArray(found!.sessions)).toBe(true);
      expect((found!.sessions ?? []).length).toBeGreaterThan(0);

      // GetTask.
      const getRes = await rpc(
        "engram.app.v1.TaskService",
        "GetTask",
        { taskId: smokeTaskId },
        memberCookie,
      );
      expect(getRes.status).toBe(200);
      const getBody = (await getRes.json()) as { task?: { id?: string } };
      expect(getBody.task?.id).toBe(smokeTaskId);

      console.log(
        `Smoke 9/10 PASS: ListTasks shows task; GetTask returns task ${smokeTaskId}`,
      );
    },
  );

  test.skipIf(!SMOKE)(
    "10/10 DeleteTask → ListTasks no longer shows the task (member's view)",
    async () => {
      const deleteRes = await rpc(
        "engram.app.v1.TaskService",
        "DeleteTask",
        { taskId: smokeTaskId },
        memberCookie,
      );
      expect(deleteRes.status).toBe(200);

      // ListTasks should no longer show the task.
      const listRes = await rpc(
        "engram.app.v1.TaskService",
        "ListTasks",
        {},
        memberCookie,
      );
      expect(listRes.status).toBe(200);
      const listBody = (await listRes.json()) as { tasks?: Array<{ id?: string }> };
      const tasks = listBody.tasks ?? [];
      const found = tasks.find((t) => t.id === smokeTaskId);
      expect(found).toBeUndefined();

      console.log(
        `Smoke 10/10 PASS: DeleteTask → task ${smokeTaskId} gone from ListTasks`,
      );
    },
  );
});
