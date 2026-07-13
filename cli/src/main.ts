#!/usr/bin/env bun
/**
 * engrams — the product CLI. Talks ONLY to the orchestrator (Connect RPC on
 * /rpc + the SSE/auth HTTP routes); the coordinator is an internal service
 * the CLI never dials. Auth is `engrams auth login` (device flow → stored
 * user-owned API key) or ENGRAMS_API_KEY (CI). See cli/README.md.
 *
 * Output contract (carried over from the Rust engram-cli so script pipelines
 * port unchanged): default = human tables; --json = pretty JSON on stdout and
 * nothing else; errors on stderr as `engrams: …` + exit 1.
 */

import { Command } from "commander";

import { requireClients } from "./client.ts";
import { resolveHost } from "./config.ts";
import * as auth from "./commands/auth.ts";
import * as session from "./commands/session.ts";
import * as image from "./commands/image.ts";
import * as host from "./commands/host.ts";
import * as task from "./commands/task.ts";
import * as profile from "./commands/profile.ts";
import * as apikey from "./commands/apikey.ts";
import * as admin from "./commands/admin.ts";

const program = new Command("engrams")
  .description("engrams product CLI — sessions, tasks, images, fleet")
  .version("0.1.0")
  .option("--url <url>", "orchestrator base URL (env: ENGRAMS_URL)")
  .option("--json", "print raw JSON instead of the human-readable view", false);

/** Global flags resolved at action time (commander binds them to the root). */
function ctx() {
  const opts = program.opts<{ url?: string; json: boolean }>();
  const hostUrl = resolveHost(opts.url);
  return { hostUrl, json: opts.json, clients: () => requireClients(hostUrl) };
}

// ---- auth -----------------------------------------------------------------

const authCmd = program.command("auth").description("log in / out of an engrams host");
authCmd
  .command("login")
  .description("authenticate via your browser (device flow) and store an API key")
  .action(() => auth.login(ctx().hostUrl));
authCmd
  .command("logout")
  .description("revoke the stored key for this host and forget it")
  .action(() => auth.logout(ctx().hostUrl));
authCmd
  .command("status")
  .description("show who you are logged in as")
  .action(() => {
    const { hostUrl, json } = ctx();
    return auth.status(hostUrl, json);
  });

// ---- task (the product-level create) ---------------------------------------

const taskCmd = program.command("task").description("tasks — profile-based agent runs");
taskCmd
  .command("create")
  .description("start a task from a profile (prints the primary session id)")
  .requiredOption("--profile <name-or-id>", "profile to launch from")
  .option("--prompt <text>", "initial prompt for the agent")
  .option("--title <text>", "task title (defaults from the prompt)")
  .option("--harness <name>", "override the profile's harness")
  .option("--model <name>", "override the profile's model")
  .option("--effort <level>", "override the profile's reasoning effort")
  .action((o: { profile: string; prompt?: string; title?: string; harness?: string; model?: string; effort?: string }) => {
    const { clients, json } = ctx();
    return task.create(clients(), o, json);
  });
taskCmd
  .command("list")
  .description("list your tasks (admins additionally see unattributed sessions)")
  .action(() => {
    const { clients, json } = ctx();
    return task.list(clients(), json);
  });
taskCmd
  .command("get <id>")
  .description("print one task")
  .action((id: string) => {
    const { clients, json } = ctx();
    return task.get(clients(), id, json);
  });
taskCmd
  .command("delete <id>")
  .description("delete a task AND tear down its session(s)")
  .action((id: string) => {
    const { clients } = ctx();
    return task.remove(clients(), id);
  });

// ---- session ----------------------------------------------------------------

const sessionCmd = program.command("session").description("operations on sessions");
sessionCmd
  .command("create")
  .description("admin escape hatch: boot a session from a raw image URI (no profile)")
  .requiredOption("--image <uri>", "image to boot (flat OCI URI; must be enabled)")
  .option("--dev-vm", "pure dev VM — no harness driven; interact via exec", false)
  .option("--harness <name>", "catalog harness for an agent session (e.g. claude)")
  .option("--prompt <text>", "initial prompt (agent mode only)")
  .action((o: { image: string; devVm: boolean; harness?: string; prompt?: string }) => {
    const { clients, json } = ctx();
    return session.create(clients(), o, json);
  });
sessionCmd
  .command("list")
  .description("list sessions")
  .action(() => {
    const { clients, json } = ctx();
    return session.list(clients(), json);
  });
sessionCmd
  .command("get <id>")
  .description("print one session")
  .action((id: string) => {
    const { clients, json } = ctx();
    return session.get(clients(), id, json);
  });
sessionCmd
  .command("exec <id> <cmd>")
  .description("run a shell command in the session's sandbox (mirrors its exit code)")
  .option("--timeout-secs <n>", "wall-clock timeout", (v: string) => parseInt(v, 10))
  .action((id: string, cmd: string, o: { timeoutSecs?: number }) => {
    const { clients, json } = ctx();
    return session.exec(clients(), id, cmd, o.timeoutSecs, json);
  });
sessionCmd
  .command("delete <id>")
  .description("mark the session completed and tear down its sandbox")
  .action((id: string) => {
    const { clients } = ctx();
    return session.remove(clients(), id);
  });
sessionCmd
  .command("logs <id>")
  .description("tail the persistent event log (SSE; Ctrl-C to stop)")
  .option("--since <idx>", "replay strictly-after idx N, then tail", (v: string) =>
    parseInt(v, 10),
  )
  .action((id: string, o: { since?: number }) => {
    const { clients } = ctx();
    return session.logs(clients(), id, o.since);
  });
sessionCmd
  .command("log <id>")
  .description("show the conversation timeline (newest rows by default)")
  .option("--limit <n>", "cap on rows returned", (v: string) => parseInt(v, 10))
  .option("--from-start", "oldest rows instead of the tail")
  .action((id: string, o: { limit?: number; fromStart?: boolean }) => {
    const { clients, json } = ctx();
    return session.log(clients(), id, o.limit, !o.fromStart, json);
  });
sessionCmd
  .command("resume <id>")
  .description("resume an Idle session via its hot snapshot")
  .action((id: string) => {
    const { clients, json } = ctx();
    return session.resume(clients(), id, json);
  });
sessionCmd
  .command("prompt <id> <text>")
  .description("push a prompt to a running session (auto-resumes Idle)")
  .action((id: string, text: string) => {
    const { clients, json } = ctx();
    return session.prompt(clients(), id, text, json);
  });

// ---- image + registry --------------------------------------------------------

const imageCmd = program.command("image").description("operations on enabled images");
imageCmd
  .command("list")
  .description("list the enabled images sessions may reference")
  .action(() => {
    const { clients, json } = ctx();
    return image.list(clients(), json);
  });
imageCmd
  .command("enable")
  .description("enable an image (captures its base snapshot; polls to ready)")
  .requiredOption("--uri <uri>", "full OCI URI: <host>[:port]/<repo>:<tag>")
  .option("--config <path>", "image-config TOML (REQUIRED on first enable)")
  .option("--no-wait", "print the job id and return without polling")
  .action((o: { uri: string; config?: string; wait: boolean }) => {
    const { clients, json } = ctx();
    return image.enable(clients(), o.uri, o.config, !o.wait, json);
  });
imageCmd
  .command("update")
  .description("edit an enabled image's config (full replace)")
  .requiredOption("--uri <uri>", "full OCI URI of an already-enabled image")
  .requiredOption("--config <path>", "the complete new image-config TOML")
  .option("--allow-recapture", "consent to a recapture when the diff needs one", false)
  .option("--no-wait", "don't poll a recapture job to completion")
  .action((o: { uri: string; config: string; allowRecapture: boolean; wait: boolean }) => {
    const { clients, json } = ctx();
    return image.update(clients(), o.uri, o.config, o.allowRecapture, !o.wait, json);
  });
imageCmd
  .command("disable")
  .description("disable an image (registry artifact untouched)")
  .requiredOption("--uri <uri>")
  .action((o: { uri: string }) => {
    const { clients } = ctx();
    return image.disable(clients(), o.uri);
  });
imageCmd
  .command("refresh")
  .description("re-fetch a moved tag (and optionally force a recapture)")
  .requiredOption("--uri <uri>")
  .option("--recapture", "force a new base-snapshot capture", false)
  .action((o: { uri: string; recapture: boolean }) => {
    const { clients, json } = ctx();
    return image.refresh(clients(), o.uri, o.recapture, json);
  });
imageCmd
  .command("jobs")
  .description("list recent enable/refresh jobs")
  .action(() => {
    const { clients, json } = ctx();
    return image.jobs(clients(), json);
  });
imageCmd
  .command("job <id>")
  .description("print one enable/refresh job")
  .action((id: string) => {
    const { clients, json } = ctx();
    return image.job(clients(), id, json);
  });
imageCmd
  .command("poll-job <id>")
  .description("poll a job until ready/failed")
  .action((id: string) => {
    const { clients, json } = ctx();
    return image.pollJob(clients(), id, json);
  });
imageCmd
  .command("retry-job <id>")
  .description("re-queue a failed enable/refresh job")
  .option("--no-wait", "don't poll the retried job to completion")
  .action((id: string, o: { wait: boolean }) => {
    const { clients, json } = ctx();
    return image.retryJob(clients(), id, !o.wait, json);
  });

const registryCmd = program
  .command("registry")
  .description("Docker registry credentials (envelope-encrypted server-side)");
registryCmd
  .command("add")
  .description("add or update a registry credential")
  .requiredOption("--host <host>", "registry host, e.g. ghcr.io")
  .option("--auth-kind <kind>", "static | gcp-workload-identity", "static")
  .option("--username <name>", "static: registry username")
  .option("--password-file <path>", "static: file containing the password/PAT")
  .option("--password-stdin", "static: read the password from stdin", false)
  .option("--impersonate-sa <email>", "gcp-workload-identity: impersonate this SA")
  .action(
    (o: {
      host: string;
      authKind: string;
      username?: string;
      passwordFile?: string;
      passwordStdin: boolean;
      impersonateSa?: string;
    }) => {
      const { clients, json } = ctx();
      return image.registryAdd(clients(), o, json);
    },
  );
registryCmd
  .command("list")
  .description("list registry credentials (never returns passwords)")
  .action(() => {
    const { clients, json } = ctx();
    return image.registryList(clients(), json);
  });
registryCmd
  .command("rm <host>")
  .description("remove the credential for a registry host")
  .action((h: string) => {
    const { clients } = ctx();
    return image.registryRm(clients(), h);
  });

// ---- host / fleet -------------------------------------------------------------

const hostCmd = program
  .command("host")
  .alias("hosts")
  .description("operations on fleet hosts");
hostCmd
  .command("list")
  .description("list hosts")
  .action(() => {
    const { clients, json } = ctx();
    return host.list(clients(), json);
  });
hostCmd
  .command("get <id>")
  .description("print one host")
  .action((id: string) => {
    const { clients, json } = ctx();
    return host.get(clients(), id, json);
  });
hostCmd
  .command("drain <id>")
  .description("flip a host to draining (new sessions avoid it)")
  .action((id: string) => {
    const { clients } = ctx();
    return host.drain(clients(), id);
  });
hostCmd
  .command("delete <id>")
  .description("deregister a host row (refused while sessions are bound — drain / reap first)")
  .action((id: string) => {
    const { clients } = ctx();
    return host.remove(clients(), id);
  });
hostCmd
  .command("uncordon <id>")
  .description("clear an admin cordon; host returns to ready")
  .action((id: string) => {
    const { clients, json } = ctx();
    return host.uncordon(clients(), id, json);
  });
hostCmd
  .command("evacuate <session-id>")
  .description("evacuate one session off its host (scanner resumes it on a peer)")
  .action((id: string) => {
    const { clients, json } = ctx();
    return host.evacuate(clients(), id, json);
  });

// ---- profile -------------------------------------------------------------------

const profileCmd = program.command("profile").description("session profiles (read-only)");
profileCmd
  .command("list")
  .description("list launchable profiles")
  .action(() => {
    const { clients, json } = ctx();
    return profile.list(clients(), json);
  });
profileCmd
  .command("get <id>")
  .description("print one profile")
  .action((id: string) => {
    const { clients, json } = ctx();
    return profile.get(clients(), id, json);
  });

// ---- apikey (admin) --------------------------------------------------------------

const apikeyCmd = program
  .command("apikey")
  .description("global service-account API keys (admin; your login key is `auth`)");
apikeyCmd
  .command("create")
  .description("mint a global key (plaintext printed once, alone, on stdout)")
  .requiredOption("--name <name>", 'display name, e.g. "ci-bot"')
  .option("--role <role>", "admin | user", "user")
  .option("--expires-at <iso>", "ISO-8601 expiry; empty = never", "")
  .action((o: { name: string; role: string; expiresAt: string }) => {
    const { clients, json } = ctx();
    return apikey.create(clients(), o.name, o.role, o.expiresAt, json);
  });
apikeyCmd
  .command("list")
  .description("list all keys (masked preview only)")
  .action(() => {
    const { clients, json } = ctx();
    return apikey.list(clients(), json);
  });
apikeyCmd
  .command("revoke <id>")
  .description("revoke a key (idempotent)")
  .action((id: string) => {
    const { clients, json } = ctx();
    return apikey.revoke(clients(), id, json);
  });

// ---- admin ------------------------------------------------------------------------

const adminCmd = program
  .command("admin")
  .description("explicit triggers for implicitly-driven primitives");
adminCmd
  .command("flush <session-id>")
  .description("force an immediate chunked-disk flush of one session")
  .action((id: string) => {
    const { clients, json } = ctx();
    return admin.flush(clients(), id, json);
  });
adminCmd
  .command("evict-idle <session-id>")
  .description("run the idle-eviction pipeline now (synchronous; session ends Idle)")
  .action((id: string) => {
    const { clients, json } = ctx();
    return admin.evictIdle(clients(), id, json);
  });
adminCmd
  .command("gc")
  .description("blob GC sweeps (bundle/snapshot-blob/chunk); dry-run unless --apply")
  .option("--apply", "mark candidates + delete promoted blobs (default: report only)")
  .option("--grace-secs <n>", "override the candidate grace window in seconds (0 = delete same sweep)")
  .action((opts: { apply?: boolean; graceSecs?: string }) => {
    const { clients, json } = ctx();
    const grace = opts.graceSecs !== undefined ? BigInt(opts.graceSecs) : undefined;
    return admin.gc(clients(), Boolean(opts.apply), grace, json);
  });

program.parseAsync().catch((e: unknown) => {
  console.error(`engrams: ${e instanceof Error ? e.message : e}`);
  process.exit(1);
});
