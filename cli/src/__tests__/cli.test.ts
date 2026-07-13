/**
 * CLI tests.
 *
 * Group 1 — pure units (config resolution, output formatting, TOML→proto).
 * Group 2 — the real binary against a STUB orchestrator: a node:http server
 *   serving Connect handlers on /rpc plus hand-rolled /api/auth device-flow +
 *   SSE routes. Each test spawns `bun src/main.ts …` with ENGRAMS_URL pointed
 *   at the stub and asserts the stdout/exit-code contract scripts rely on.
 */

import { describe, expect, test, afterAll } from "bun:test";
import { mkdtempSync, readFileSync, statSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createServer, type Server } from "node:http";
import { connectNodeAdapter } from "@connectrpc/connect-node";
import type { ConnectRouter } from "@connectrpc/connect";

import { SessionService } from "../gen/engram/app/v1/session_pb.ts";
import { FleetService } from "../gen/engram/app/v1/fleet_pb.ts";
import { ProfileService } from "../gen/engram/app/v1/profile_pb.ts";
import { TaskService } from "../gen/engram/app/v1/task_pb.ts";
import { ApiKeyService } from "../gen/engram/app/v1/api_key_pb.ts";
import { normalizeHost, resolveHost, storeCredential, storedCredential } from "../config.ts";
import { formatEventLine } from "../commands/session.ts";
import { loadImageConfig } from "../commands/image.ts";
import { truncate } from "../output.ts";

// ---------------------------------------------------------------------------
// Group 1 — pure units
// ---------------------------------------------------------------------------

describe("config", () => {
  test("normalizeHost strips trailing slashes; resolveHost precedence", () => {
    expect(normalizeHost("https://x.example/")).toBe("https://x.example");
    expect(resolveHost("http://flag.example")).toBe("http://flag.example");
    // Flag beats env; env beats default (env set per-subprocess in Group 2).
  });

  test("hosts.json round-trip is 0600 and keyed by normalized host", () => {
    const dir = mkdtempSync(join(tmpdir(), "engrams-test-"));
    process.env["XDG_CONFIG_HOME"] = dir;
    try {
      storeCredential("https://h.example/", { apiKey: "engk_k", keyId: "id-1", user: "a@b.c" });
      const cred = storedCredential("https://h.example");
      expect(cred?.apiKey).toBe("engk_k");
      const mode = statSync(join(dir, "engrams", "hosts.json")).mode & 0o777;
      expect(mode).toBe(0o600);
    } finally {
      delete process.env["XDG_CONFIG_HOME"];
    }
  });
});

describe("output", () => {
  test("truncate is char-based with ellipsis", () => {
    expect(truncate("abcdef", 4)).toBe("abc…");
    expect(truncate("🦀🦀🦀", 2)).toBe("🦀…");
    expect(truncate("hi", 10)).toBe("hi");
  });

  test("formatEventLine matches the Rust CLI line contract", () => {
    const out = formatEventLine("7", "exec_completed", '{"exec_id":"x","exit_status":0}');
    expect(out).toContain('[     7] exec_completed: {"exec_id":"x","exit_status":0}');
    // idx-less lagged frames render `-`; non-JSON payloads become strings.
    expect(formatEventLine(undefined, "lagged", '{"missed":3}')).toContain("[     -] lagged");
    expect(formatEventLine("1", "raw", "not-json")).toContain('"not-json"');
  });
});

describe("image config TOML", () => {
  test("maps the engram_core shape to the proto init", () => {
    const dir = mkdtempSync(join(tmpdir(), "engrams-toml-"));
    const path = join(dir, "image-config.toml");
    writeFileSync(
      path,
      `
name = "dev-engrams"
description = "dogfood"
workdir = "/workspace"

[env]
FOO = "bar"

[resources]
suggested_memory_mib = 8192
suggested_vcpus = 4

[warm]
command = ["just", "dev"]
timeout_secs = 900

[[warm.env]]
name = "LITERAL"
value = "v"

[[warm.env]]
name = "SECRET"
secret_ref = "org://token"

[warm.network]
default = "deny"
allow_hosts = ["github.com"]
`,
    );
    const c = loadImageConfig(path);
    expect(c.name).toBe("dev-engrams");
    expect(c.env).toEqual({ FOO: "bar" });
    expect(c.resources?.suggestedMemoryMib).toBe(8192);
    expect(c.warm?.command).toEqual(["just", "dev"]);
    expect(c.warm?.timeoutSecs).toBe(900n);
    expect(c.warm?.env?.[0]?.value).toEqual({ case: "literal", value: "v" });
    expect(c.warm?.env?.[1]?.value).toEqual({ case: "secretRef", value: "org://token" });
    expect(c.warm?.network?.allowHosts).toEqual(["github.com"]);
  });
});

// ---------------------------------------------------------------------------
// Group 2 — the binary against a stub orchestrator
// ---------------------------------------------------------------------------

/** Requests the stub saw, for header assertions. */
const seen: Array<{ path: string; apiKey?: string; authorization?: string }> = [];

function stubRoutes(router: ConnectRouter): void {
  router.service(SessionService, {
    listSessions: () => ({
      sessions: [
        {
          session: {
            id: "sess-1",
            status: "active",
            image: "ghcr.io/x/y:z",
            mode: "agent",
            createdAt: "2026-07-10T00:00:00Z",
            lastActiveAt: "2026-07-10T00:01:00Z",
            sandboxId: "sb-1",
          },
        },
      ],
    }),
    deleteSession: () => ({}),
    // Streams two stdout chunks, one stderr, exit 3.
    exec: async function* () {
      yield { event: { case: "started" as const, value: { execId: "e-1" } } };
      yield { event: { case: "stdout" as const, value: new TextEncoder().encode("out-a\n") } };
      yield { event: { case: "stderr" as const, value: new TextEncoder().encode("err-b\n") } };
      yield { event: { case: "stdout" as const, value: new TextEncoder().encode("out-c\n") } };
      yield { event: { case: "exit" as const, value: { exitStatus: 3 } } };
    },
  });
  router.service(FleetService, {
    // uint64 capacities arrive as BIGINT on the wire — the field class that
    // crashed `--json hosts list` in the e2e gate (JSON.stringify rejects
    // BigInt). This stub pins the bigint-safe output edge.
    listHosts: () => ({
      hosts: [
        {
          id: "host-1",
          hostname: "stub-host",
          status: "ready",
          capacityTotalMib: 16384n,
          capacityUsedMib: 512n,
          // ADR 0046 figures — what host list actually renders post-#650.
          allocatableMib: 16384n,
          reservedMib: 512n,
          freeMib: 15872n,
          utilBaseShmMib: 1024n,
          cpuBudgetVcpus: 64n,
          freeVcpus: 56n,
          cordoned: false,
          failingCapabilities: [],
          runningSandboxes: 2,
          lastHeartbeatAt: "2026-07-10T00:00:00Z",
        },
      ],
    }),
  });
  router.service(ProfileService, {
    listProfiles: () => ({
      profiles: [
        { id: "prof-1", name: "backend", archived: false },
        { id: "prof-2", name: "dupe", archived: false },
        { id: "prof-3", name: "dupe", archived: false },
      ],
    }),
  });
  router.service(TaskService, {
    createTask: (req: { profileId: string; prompt?: string }) => ({
      task: {
        id: "task-1",
        type: "chat",
        status: "open",
        createdAt: "2026-07-10T00:00:00Z",
        sourceJson: "{}",
        sessions: [{ sessionId: `sess-for-${req.profileId}` }],
      },
    }),
  });
  router.service(ApiKeyService, {
    createCliKey: () => ({
      meta: {
        id: "key-1",
        name: "cli:test",
        role: "user",
        start: "engk_abc123",
        createdAt: "2026-07-10T00:00:00Z",
        expiresAt: "",
        lastUsedAt: "",
        ownerEmail: "",
      },
      key: "engk_minted-plaintext",
    }),
    revokeCliKey: () => ({ revoked: true }),
    whoAmI: () => ({
      userId: "u-alice",
      email: "alice@example.com",
      name: "Alice",
      role: "user",
      serviceAccount: false,
    }),
  });
}

async function startStub(): Promise<{ url: string; server: Server }> {
  const rpc = connectNodeAdapter({ routes: stubRoutes, requestPathPrefix: "/rpc" });
  const server = createServer((req, res) => {
    const path = (req.url ?? "/").split("?", 1)[0]!;
    seen.push({
      path,
      apiKey: req.headers["x-api-key"] as string | undefined,
      authorization: req.headers["authorization"] as string | undefined,
    });
    if (path.startsWith("/rpc")) {
      rpc(req, res);
      return;
    }
    if (path === "/api/auth/device/code") {
      res.writeHead(200, { "content-type": "application/json" });
      res.end(
        JSON.stringify({
          device_code: "dev-code",
          user_code: "ABCD1234",
          verification_uri: "http://127.0.0.1:1/device",
          verification_uri_complete: "http://127.0.0.1:1/device?user_code=ABCD1234",
          expires_in: 600,
          interval: 0,
        }),
      );
      return;
    }
    if (path === "/api/auth/device/token") {
      // First poll pending, then approved — exercises the poll loop.
      tokenPolls += 1;
      if (tokenPolls < 2) {
        res.writeHead(400, { "content-type": "application/json" });
        res.end(JSON.stringify({ error: "authorization_pending" }));
      } else {
        res.writeHead(200, { "content-type": "application/json" });
        res.end(JSON.stringify({ access_token: "session-token-xyz", token_type: "Bearer" }));
      }
      return;
    }
    if (path === "/api/auth/get-session") {
      res.writeHead(200, { "content-type": "application/json" });
      res.end(JSON.stringify({ user: { email: "alice@example.com", role: "user" } }));
      return;
    }
    if (/^\/api\/v1\/sessions\/[^/]+\/events$/.test(path)) {
      res.writeHead(200, { "content-type": "text/event-stream" });
      res.write(`event: ping\ndata: \n\n`);
      res.write(`id: 0\nevent: run_started\ndata: {"run_id":"r-1"}\n\n`);
      res.write(`id: 1\nevent: agent_message\ndata: {"text":"hello"}\n\n`);
      res.end();
      return;
    }
    res.writeHead(404).end();
  });
  let tokenPolls = 0;
  const url = await new Promise<string>((resolve) =>
    server.listen(0, "127.0.0.1", () => {
      const addr = server.address();
      resolve(`http://127.0.0.1:${typeof addr === "object" && addr ? addr.port : 0}`);
    }),
  );
  return { url, server };
}

const stub = await startStub();
afterAll(() => stub.server.close());

const MAIN = join(import.meta.dir, "..", "main.ts");

async function runCli(
  args: string[],
  opts?: { env?: Record<string, string>; stdin?: string },
): Promise<{ code: number; stdout: string; stderr: string }> {
  const proc = Bun.spawn(["bun", MAIN, ...args], {
    env: {
      ...process.env,
      ENGRAMS_URL: stub.url,
      ENGRAMS_API_KEY: "engk_test-key",
      ...opts?.env,
    },
    stdin: opts?.stdin !== undefined ? new TextEncoder().encode(opts.stdin) : undefined,
    stdout: "pipe",
    stderr: "pipe",
  });
  const [stdout, stderr, code] = await Promise.all([
    new Response(proc.stdout).text(),
    new Response(proc.stderr).text(),
    proc.exited,
  ]);
  return { code, stdout, stderr };
}

describe("engrams (binary vs stub orchestrator)", () => {
  test("session list --json prints the script-stable shape and sends x-api-key", async () => {
    const r = await runCli(["--json", "session", "list"]);
    expect(r.code).toBe(0);
    const body = JSON.parse(r.stdout) as { sessions: Array<Record<string, unknown>> };
    expect(body.sessions[0]!["id"]).toBe("sess-1");
    expect(body.sessions[0]!["sandbox_id"]).toBe("sb-1");
    const rpc = seen.find((s) => s.path.includes("ListSessions"));
    expect(rpc?.apiKey).toBe("engk_test-key");
  });

  test("hosts list serializes bigint capacities in BOTH output modes", async () => {
    // Regression: HostView's uint64 fields arrive as bigint; --json must not
    // crash on JSON.stringify and the table must render them. Post-#650 the
    // surfaced figures are the ADR 0046 allocatable/free set (the legacy
    // capacity_used_mib was a dead always-0 field).
    const j = await runCli(["--json", "hosts", "list"]);
    expect(j.code).toBe(0);
    const body = JSON.parse(j.stdout) as { hosts: Array<Record<string, unknown>> };
    expect(body.hosts[0]!["allocatable_mib"]).toBe(16384);
    expect(body.hosts[0]!["hostname"]).toBe("stub-host");
    const t = await runCli(["hosts", "list"]);
    expect(t.code).toBe(0);
    expect(t.stdout).toContain("16384");
  });

  test("no credential → exit 1 with the auth login pointer", async () => {
    const r = await runCli(["session", "list"], {
      env: { ENGRAMS_API_KEY: "", XDG_CONFIG_HOME: mkdtempSync(join(tmpdir(), "engrams-nocred-")) },
    });
    expect(r.code).toBe(1);
    expect(r.stderr).toContain("engrams auth login");
  });

  test("session exec streams stdout/stderr and mirrors a non-zero exit", async () => {
    const r = await runCli(["session", "exec", "sess-1", "ls"]);
    expect(r.stdout).toBe("out-a\nout-c\n");
    expect(r.stderr).toContain("err-b");
    expect(r.code).toBe(1); // remote exit 3 → CLI exit 1 (pipeline-composable)
  });

  test("session exec --json accumulates and reports exit_status", async () => {
    const r = await runCli(["--json", "session", "exec", "sess-1", "ls"]);
    const body = JSON.parse(r.stdout) as { stdout: string; stderr: string; exit_status: number };
    expect(body.stdout).toBe("out-a\nout-c\n");
    expect(body.exit_status).toBe(3);
  });

  test("session logs tails the SSE route, skipping pings", async () => {
    const r = await runCli(["session", "logs", "sess-1"]);
    expect(r.code).toBe(0);
    const lines = r.stdout.trim().split("\n");
    expect(lines[0]).toContain('[     0] run_started: {"run_id":"r-1"}');
    expect(lines[1]).toContain('[     1] agent_message: {"text":"hello"}');
    expect(r.stdout).not.toContain("ping");
  });

  test("task create resolves a profile by name and prints the session id", async () => {
    const r = await runCli(["task", "create", "--profile", "backend", "--prompt", "fix it"]);
    expect(r.code).toBe(0);
    expect(r.stdout.trim()).toBe("sess-for-prof-1");
  });

  test("task create rejects an ambiguous profile name", async () => {
    const r = await runCli(["task", "create", "--profile", "dupe"]);
    expect(r.code).toBe(1);
    expect(r.stderr).toContain("ambiguous");
  });

  test("auth login: device flow → CreateCliKey exchange → hosts.json (0600)", async () => {
    const configDir = mkdtempSync(join(tmpdir(), "engrams-login-"));
    const r = await runCli(["auth", "login"], {
      env: { ENGRAMS_API_KEY: "", XDG_CONFIG_HOME: configDir, BROWSER: "true" },
      stdin: "\n", // "Press Enter to open the browser…"
    });
    expect(r.code).toBe(0);
    expect(r.stderr).toContain("copy your one-time code: ABCD1234");
    expect(r.stderr).toContain("Logged in to");
    const hosts = JSON.parse(
      readFileSync(join(configDir, "engrams", "hosts.json"), "utf8"),
    ) as Record<string, { apiKey: string; keyId: string; user?: string }>;
    const cred = hosts[stub.url]!;
    expect(cred.apiKey).toBe("engk_minted-plaintext");
    expect(cred.keyId).toBe("key-1");
    expect(cred.user).toBe("alice@example.com");
    // The exchange leg authenticated with the device session bearer.
    const exchange = seen.find((s) => s.path.includes("CreateCliKey"));
    expect(exchange?.authorization).toBe("Bearer session-token-xyz");

    // The stored key now authenticates normally + logout revokes and forgets.
    const status = await runCli(["auth", "status"], {
      env: { ENGRAMS_API_KEY: "", XDG_CONFIG_HOME: configDir },
    });
    expect(status.code).toBe(0);
    expect(status.stdout).toContain("alice@example.com");
    const logout = await runCli(["auth", "logout"], {
      env: { ENGRAMS_API_KEY: "", XDG_CONFIG_HOME: configDir },
    });
    expect(logout.code).toBe(0);
    const after = JSON.parse(
      readFileSync(join(configDir, "engrams", "hosts.json"), "utf8"),
    ) as Record<string, unknown>;
    expect(after[stub.url]).toBeUndefined();
    const revoke = seen.find((s) => s.path.includes("RevokeCliKey"));
    expect(revoke?.apiKey).toBe("engk_minted-plaintext");
  });

  test("auth status --json reports the env-var source", async () => {
    const r = await runCli(["--json", "auth", "status"]);
    const body = JSON.parse(r.stdout) as { loggedIn: boolean; user: string; source: string };
    expect(body.loggedIn).toBe(true);
    expect(body.user).toBe("alice@example.com");
    expect(body.source).toBe("ENGRAMS_API_KEY");
  });
});
