/**
 * Orchestrator live smoke test (ADR 0051 Tasks 18, 19, & 20).
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
 *      7b. Admin CreateProfile (ADR 0052): wrap a no-harness enabled image in a
 *          profile the member tests launch from (tasks start from a profile, not
 *          a raw image). Cleaned up in afterAll.
 *   8.  Task lifecycle (native TaskService — Task 19; ADR 0052 profiles):
 *       8. CreateTask(chat, profile_id) → task returned with live session state
 *       9. ListTasks → task visible with session; GetTask → task returned
 *      10. DeleteTask → ListTasks empty (member's view of created task)
 *   11–13. SSE + token (Task 20):
 *      11. GET /api/v1/sessions/:id/events with member cookie → ≥1 SSE frame with id:
 *      12. Reconnect with Last-Event-ID → no duplicate (cursor respected)
 *      13. harness-env PUT→GET(present)→DELETE→GET(absent) with member cookie
 *
 * Honest skip: tests skip without SMOKE=1 (bun test --cwd orchestrator
 * passes 0 failures even without the stack running).
 */

import { expect, test, describe, afterAll } from "bun:test";
// ws is used for test 14b because Bun's native WebSocket strips the Cookie
// header from HTTP upgrade requests; the ws npm package sends it correctly.
import WsClient from "ws";

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

  // Admin cookie (set in test 7) + the profile created from it (test 7b).
  // ADR 0052: tasks now start from an admin-curated profile, not a raw image,
  // so the member task tests below launch from `smokeProfileId`.
  let adminCookie = "";
  let smokeProfileId = "";

  // Clean up the ephemeral profile created in test 7b (admin-only delete).
  afterAll(async () => {
    if (SMOKE && smokeProfileId && adminCookie) {
      await rpc(
        "engram.app.v1.ProfileService",
        "DeleteProfile",
        { id: smokeProfileId },
        adminCookie,
      );
    }
  });

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
      adminCookie = extractSessionCookie(signInRes);

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
  // 7b. Create a session profile (ADR 0052) — admin-only.
  //
  // Tasks now start from an admin-curated profile, not a raw image. Wrap the
  // first no-harness enabled image in a profile; the member task tests below
  // launch from it. Uses the admin cookie from test 7. Deleted in afterAll.
  // -------------------------------------------------------------------------
  test.skipIf(!SMOKE)(
    "7b/ admin CreateProfile (no-harness image) → profile with id",
    async () => {
      expect(adminCookie).toBeTruthy(); // test 7 must have run

      // Pick any enabled image (admin view) to back the profile. ADR 0062:
      // an image carries no harness identity — harness is a per-session choice.
      const imagesRes = await rpc(
        "engram.app.v1.ImageService",
        "ListEnabledImages",
        {},
        adminCookie,
      );
      expect(imagesRes.status).toBe(200);
      const imagesBody = (await imagesRes.json()) as {
        images?: Array<{ id?: string; imageUri?: string }>;
      };
      const enabledImage = (imagesBody.images ?? [])[0];
      if (!enabledImage?.id) {
        throw new Error(
          "No enabled image found in ListEnabledImages — bake/enable one first " +
            "(deploy/dev/integration-session.sh)",
        );
      }

      const createRes = await rpc(
        "engram.app.v1.ProfileService",
        "CreateProfile",
        {
          name: `smoke-profile-${Date.now()}`,
          description: "ephemeral smoke profile",
          icon: "box",
          imageId: enabledImage.id,
          includeUserTokens: false,
          envVars: {},
        },
        adminCookie,
      );
      expect(createRes.status).toBe(200);
      const createBody = (await createRes.json()) as { profile?: { id?: string } };
      smokeProfileId = createBody.profile?.id ?? "";
      expect(smokeProfileId).toBeTruthy();
      console.log(`Smoke 7b PASS: created profile ${smokeProfileId}`);
    },
  );

  // -------------------------------------------------------------------------
  // 8–10. Task lifecycle — native TaskService (Task 19; ADR 0052 profiles)
  //
  // Uses the member account provisioned in test 2 (memberCookie) and the
  // profile created in test 7b (smokeProfileId). The created task id is shared
  // across tests 8–10.
  // -------------------------------------------------------------------------

  let smokeTaskId = "";

  test.skipIf(!SMOKE)(
    "8/10 member CreateTask(chat, from profile) → task with live session state",
    async () => {
      expect(smokeProfileId).toBeTruthy(); // test 7b must have created it

      const createRes = await rpc(
        "engram.app.v1.TaskService",
        "CreateTask",
        { type: "chat", profileId: smokeProfileId, title: "Smoke task" },
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

  // -------------------------------------------------------------------------
  // 11–13. SSE + token (Task 20)
  //
  // Uses the task + session created in test 8 (smokeTaskId).
  // -------------------------------------------------------------------------

  test.skipIf(!SMOKE)(
    "11/13 GET SSE events with member cookie → ≥1 frame with id:",
    async () => {
      // Test 10 DELETED smokeTaskId (and its session) — the SSE checks need a
      // live session, so provision a fresh task here; test 12 cleans it up.
      expect(smokeProfileId).toBeTruthy();

      const createRes = await rpc(
        "engram.app.v1.TaskService",
        "CreateTask",
        {
          type: "chat",
          profileId: smokeProfileId,
          title: "Smoke SSE task",
        },
        memberCookie,
      );
      expect(createRes.status).toBe(200);
      const createBody = (await createRes.json()) as {
        task?: { id?: string; sessions?: Array<{ sessionId?: string }> };
      };
      const sseTaskId = createBody.task?.id;
      const sessionId = createBody.task?.sessions?.[0]?.sessionId;
      expect(sseTaskId).toBeTruthy();
      expect(sessionId).toBeTruthy();
      (globalThis as Record<string, unknown>).__smokeSseTaskId__ = sseTaskId;

      // Fetch SSE stream; read until we have ≥1 data line with id:.
      const ac = new AbortController();
      setTimeout(() => ac.abort(), 5_000); // 5s timeout

      let foundIdLine = false;
      let firstEventId: string | undefined;

      try {
        const res = await fetch(
          `${BASE}/api/v1/sessions/${sessionId}/events?since=0`,
          {
            headers: { Cookie: memberCookie },
            signal: ac.signal,
          },
        );
        expect(res.status).toBe(200);
        expect(res.headers.get("content-type")).toMatch(/text\/event-stream/);

        const reader = res.body!.getReader();
        const decoder = new TextDecoder();
        let buffer = "";

        outer: while (true) {
          const { value, done } = await reader.read();
          if (done) break;
          buffer += decoder.decode(value, { stream: true });
          // Parse lines for id: fields.
          const lines = buffer.split("\n");
          buffer = lines.pop() ?? "";
          for (const line of lines) {
            if (line.startsWith("id:")) {
              foundIdLine = true;
              firstEventId = line.slice(3).trim();
              break outer;
            }
          }
        }
      } catch (err) {
        // AbortError is expected after finding the first event.
        if (
          !(err instanceof Error) ||
          (err.name !== "AbortError" && !String(err).includes("aborted"))
        ) {
          throw err;
        }
      }

      expect(foundIdLine).toBe(true);
      expect(firstEventId).toBeDefined();
      console.log(
        `Smoke 11/13 PASS: SSE stream has id: line (first id=${firstEventId})`,
      );

      // Store for test 12.
      (globalThis as Record<string, unknown>).__smokeFirstEventId__ =
        firstEventId;
      (globalThis as Record<string, unknown>).__smokeSessionId__ = sessionId;
    },
  );

  test.skipIf(!SMOKE)(
    "12/13 reconnect with Last-Event-ID → no duplicate (cursor respected)",
    async () => {
      const g = globalThis as Record<string, unknown>;
      const firstEventId = g.__smokeFirstEventId__ as string | undefined;
      const sessionId = g.__smokeSessionId__ as string | undefined;

      // If test 11 was skipped or failed, skip gracefully.
      if (!firstEventId || !sessionId) {
        console.log(
          "Smoke 12/13 SKIP: no firstEventId/sessionId from test 11",
        );
        return;
      }

      // 3s read window: an idle (fully-replayed) stream yields nothing — the
      // abort is the expected exit. Keep it well under the test's own budget
      // so the cleanup DeleteTask below still fits.
      const ac = new AbortController();
      setTimeout(() => ac.abort(), 3_000);

      let idAfterCursor: string | undefined;

      try {
        const res = await fetch(
          `${BASE}/api/v1/sessions/${sessionId}/events`,
          {
            headers: {
              Cookie: memberCookie,
              // Reconnect cursor: send Last-Event-ID so server replays from there.
              "Last-Event-ID": firstEventId,
            },
            signal: ac.signal,
          },
        );
        expect(res.status).toBe(200);

        const reader = res.body!.getReader();
        const decoder = new TextDecoder();
        let buffer = "";

        outer: while (true) {
          const { value, done } = await reader.read();
          if (done) break;
          buffer += decoder.decode(value, { stream: true });
          const lines = buffer.split("\n");
          buffer = lines.pop() ?? "";
          for (const line of lines) {
            if (line.startsWith("id:")) {
              idAfterCursor = line.slice(3).trim();
              break outer;
            }
          }
        }
      } catch (err) {
        if (
          !(err instanceof Error) ||
          (err.name !== "AbortError" && !String(err).includes("aborted"))
        ) {
          throw err;
        }
      }

      // If the stream emitted any id after reconnect, it must be > firstEventId.
      // (It's fine if the stream is empty if all events were already replayed.)
      if (idAfterCursor !== undefined) {
        expect(Number(idAfterCursor)).toBeGreaterThan(Number(firstEventId));
      }

      console.log(
        `Smoke 12/13 PASS: reconnect with Last-Event-ID=${firstEventId} → next id=${idAfterCursor ?? "(none — stream empty)"} (no duplicate)`,
      );

      // Cleanup: delete the task test 11 provisioned for the SSE checks.
      const sseTaskId = g.__smokeSseTaskId__ as string | undefined;
      if (sseTaskId) {
        const delRes = await rpc(
          "engram.app.v1.TaskService",
          "DeleteTask",
          { taskId: sseTaskId },
          memberCookie,
        );
        expect(delRes.status).toBe(200);
      }
    },
    15_000, // 3s SSE read window + DeleteTask cleanup won't fit bun's 5s default
  );

  test.skipIf(!SMOKE)(
    "13/13 harness-env PUT→GET(present)→DELETE→GET(absent) with member cookie",
    async () => {
      // Assumes the claude harness is registered (so its user_env appears).
      const ENV = "CLAUDE_CODE_OAUTH_TOKEN";
      const base = `${BASE}/api/v1/me/harness-env`;
      type Vars = { vars?: Array<{ envVar: string; present: boolean }> };
      const present = (b: Vars) => b.vars?.find((v) => v.envVar === ENV)?.present ?? false;

      // PUT → 204.
      const putRes = await fetch(`${base}/${ENV}`, {
        method: "PUT",
        headers: { Cookie: memberCookie, "Content-Type": "application/json" },
        body: JSON.stringify({ value: "sk-ant-oat01-smoke-test-token" }),
      });
      expect(putRes.status).toBe(204);
      console.log(`Smoke 13/13a PASS: PUT /me/harness-env/${ENV} → 204`);

      // GET → the env var is present.
      const getRes1 = await fetch(base, { headers: { Cookie: memberCookie } });
      expect(getRes1.status).toBe(200);
      expect(present((await getRes1.json()) as Vars)).toBe(true);
      console.log("Smoke 13/13b PASS: GET /me/harness-env → present:true");

      // DELETE → 204.
      const delRes = await fetch(`${base}/${ENV}`, {
        method: "DELETE",
        headers: { Cookie: memberCookie },
      });
      expect(delRes.status).toBe(204);
      console.log("Smoke 13/13c PASS: DELETE → 204");

      // GET → no longer present.
      const getRes2 = await fetch(base, { headers: { Cookie: memberCookie } });
      expect(getRes2.status).toBe(200);
      expect(present((await getRes2.json()) as Vars)).toBe(false);
      console.log("Smoke 13/13 PASS: DELETE confirmed; GET → present:false");
    },
  );

  // -------------------------------------------------------------------------
  // 14a–14b. Shell WS (Task 21)
  // -------------------------------------------------------------------------

  test.skipIf(!SMOKE)(
    "14a/14b anonymous shell WS → 401 (no upgrade)",
    async () => {
      // Plain HTTP GET — no Upgrade headers.  The auth guard runs in the Hono
      // request handler and returns HTTP 401 before any WS upgrade is attempted.
      // Under Bun 1.3.14, fetch() with Upgrade headers triggers the node:http
      // 'upgrade' event (not the HTTP request handler), and socket.write() is
      // a no-op there, so the client would hang.  A plain GET correctly exercises
      // the pre-upgrade gate and returns a real HTTP 401.
      const res = await fetch(`${BASE}/api/v1/sessions/any-id/shell`);
      expect(res.status).toBe(401);
      console.log("Smoke 14a PASS: anonymous shell WS → 401");
    },
  );

  test.skipIf(!SMOKE)(
    "14b/14b shell WS member → terminal responds with 'hi'",
    async () => {
      // Create a task for a live session (ADR 0052: from the smoke profile).
      if (!smokeProfileId) {
        console.log("Smoke 14b SKIP: no smoke profile available");
        return;
      }

      const createRes = await rpc(
        "engram.app.v1.TaskService",
        "CreateTask",
        { type: "chat", profileId: smokeProfileId, title: "Smoke shell task" },
        memberCookie,
      );
      expect(createRes.status).toBe(200);
      const createBody = (await createRes.json()) as {
        task?: { id?: string; sessions?: Array<{ sessionId?: string }> };
      };
      const shellTaskId = createBody.task?.id;
      const sessionId = createBody.task?.sessions?.[0]?.sessionId;
      if (!shellTaskId || !sessionId) throw new Error("no task/session from CreateTask");

      let foundHi = false;
      const wsUrl = `${BASE.replace(/^http/, "ws")}/api/v1/sessions/${sessionId}/shell`;
      const ac = new AbortController();
      const abortTimer = setTimeout(() => ac.abort(), 15_000);

      try {
        // Use the ws npm package (not Bun's native WebSocket) so that the
        // Cookie header is included in the HTTP upgrade request.
        // Bun 1.3.14's native WebSocket strips custom headers (including Cookie)
        // from the upgrade handshake — confirmed by inspecting server-side
        // IncomingMessage.headers during the upgrade event.  The ws package
        // sends them correctly, matching real browser behaviour (browsers
        // auto-attach cookies by domain; ws sends them explicitly).
        const ws = new WsClient(wsUrl, {
          headers: { Cookie: memberCookie },
        });

        await new Promise<void>((resolve, reject) => {
          // Mirror the Rust shell_relay_smoke protocol exactly:
          //   1. onopen: send ttyd auth/resize JSON (PTY init frame)
          //   2. onmessage phase 1: await handshake output frame before sending keystrokes —
          //      input sent before ttyd boots the PTY is silently discarded
          //   3. onmessage phase 2: send "0echo hi\r" then watch for "hi" in output frames
          let handshakeDone = false;

          ws.on("open", () => {
            // ttyd requires this JSON as the very first content frame; without it
            // the PTY never initialises and all subsequent input is silently dropped.
            ws.send(JSON.stringify({ AuthToken: "", columns: 80, rows: 24 }));
            // Do NOT send keystrokes here — wait for the handshake frame first.
          });
          ws.on("message", (data: Buffer | string, isBinary: boolean) => {
            let bytes: Uint8Array;
            if (!isBinary) {
              // Text frame: convert to bytes for uniform handling.
              bytes = new TextEncoder().encode(typeof data === "string" ? data : data.toString("utf-8"));
            } else {
              bytes = data instanceof Buffer
                ? new Uint8Array(data.buffer, data.byteOffset, data.byteLength)
                : new Uint8Array(data as unknown as ArrayBuffer);
            }

            if (!handshakeDone) {
              // Phase 1: any output frame (binary or text) from ttyd means PTY is booted.
              // The first frame is typically ttyd's preferences (0x32) or prompt output (0x30).
              // Once we see it, send the echo keystroke with the ttyd input prefix '0'.
              handshakeDone = true;
              ws.send("0echo hi\r");
              return;
            }

            // Phase 2: strip ttyd discriminator byte (0x30 = '0' = output) and look for "hi".
            const text = new TextDecoder().decode(bytes.subarray(1));
            if (text.includes("hi")) {
              foundHi = true;
              clearTimeout(abortTimer);
              ws.close();
              resolve();
            }
          });
          ws.on("error", (e: Error) => reject(new Error(`WS error: ${String(e)}`)));
          ws.on("close", () => { if (!foundHi) resolve(); });
          ac.signal.addEventListener("abort", () => { ws.close(); resolve(); });
        });
      } finally {
        clearTimeout(abortTimer);
        if (shellTaskId) {
          await rpc("engram.app.v1.TaskService", "DeleteTask", { taskId: shellTaskId }, memberCookie);
        }
      }

      expect(foundHi).toBe(true);
      console.log("Smoke 14b PASS: shell WS → 'hi' received via ttyd echo");
    },
    30_000,
  );
});
