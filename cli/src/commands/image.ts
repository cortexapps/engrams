/**
 * engrams image … / engrams registry … — ImageService passthrough verbs.
 *
 * An image's config is a structured message (name, description, env, workdir,
 * resources, and the warm hook). The dashboard's enable dialog is the full
 * editor; the CLI covers the fields a script needs as flags and leaves the
 * warm hook to the dashboard. STRICT validation stays server-side.
 */

import { readFileSync } from "node:fs";

import type { Clients } from "../client.ts";
import { detail, fail, failWith, printJson, table, truncate } from "../output.ts";
import type { EnableJob, ImageConfig } from "../gen/engram/app/v1/image_pb.ts";
import type { MessageInitShape } from "@bufbuild/protobuf";
import type { ImageConfigSchema } from "../gen/engram/app/v1/image_pb.ts";

type ImageConfigInit = MessageInitShape<typeof ImageConfigSchema>;

// ---- config flags → proto ---------------------------------------------------

/** The config fields the CLI exposes. Everything else (the warm hook) is set in the dashboard. */
export interface ConfigFlags {
  name?: string;
  description?: string;
  workdir?: string;
  vcpus?: string;
  memoryMib?: string;
  diskGib?: string;
  swapMib?: string;
  env?: string[];
}

const CONFIG_FLAG_KEYS: (keyof ConfigFlags)[] = [
  "name",
  "description",
  "workdir",
  "vcpus",
  "memoryMib",
  "diskGib",
  "swapMib",
  "env",
];

export function hasConfigFlags(f: ConfigFlags): boolean {
  return CONFIG_FLAG_KEYS.some((k) => f[k] !== undefined && f[k] !== null);
}

function positive(flag: string, v: string | undefined): number | undefined {
  if (v === undefined) return undefined;
  if (!/^[1-9]\d*$/.test(v)) fail(`--${flag} must be a positive integer, got ${JSON.stringify(v)}`);
  return Number(v);
}

function nonNegative(flag: string, v: string | undefined): number | undefined {
  if (v === undefined) return undefined;
  if (!/^\d+$/.test(v)) fail(`--${flag} must be a non-negative integer, got ${JSON.stringify(v)}`);
  return Number(v);
}

function parseEnv(entries: string[] | undefined): Record<string, string> {
  const out: Record<string, string> = {};
  for (const e of entries ?? []) {
    const i = e.indexOf("=");
    if (i <= 0) fail(`--env expects KEY=VALUE, got ${JSON.stringify(e)}`);
    out[e.slice(0, i)] = e.slice(i + 1);
  }
  return out;
}

/**
 * Build the config to send. With a `base` (the stored config of an enabled
 * image) the flags overlay it: `--env` adds or overrides keys, the warm hook
 * passes through untouched. Without a base this is a first enable, and the
 * server requires a name and a vCPU count.
 */
export function buildImageConfig(flags: ConfigFlags, base?: ImageConfig): ImageConfigInit {
  const env = { ...(base?.env ?? {}), ...parseEnv(flags.env) };
  const name = flags.name ?? base?.name;
  if (!name) fail("--name is required on first enable");
  const vcpus = positive("vcpus", flags.vcpus) ?? base?.resources?.suggestedVcpus;
  if (!vcpus) fail("--vcpus is required on first enable (placement reserves it)");
  return {
    name,
    description: flags.description ?? base?.description,
    env,
    workdir: flags.workdir ?? base?.workdir,
    resources: {
      suggestedVcpus: vcpus,
      suggestedMemoryMib: positive("memory-mib", flags.memoryMib) ?? base?.resources?.suggestedMemoryMib,
      suggestedDiskGib: positive("disk-gib", flags.diskGib) ?? base?.resources?.suggestedDiskGib,
      suggestedSwapMib: nonNegative("swap-mib", flags.swapMib) ?? base?.resources?.suggestedSwapMib,
    },
    warm: base?.warm,
  };
}

// ---- enable-job rendering -------------------------------------------------

function jobJson(job: EnableJob) {
  return {
    id: job.id,
    image_uri: job.imageUri,
    manifest_digest: job.manifestDigest,
    state: job.state,
    chunks_total: job.chunksTotal,
    chunks_done: job.chunksDone,
    attempts: job.attempts,
    error: job.error,
    created_at: job.createdAt,
    updated_at: job.updatedAt,
    capture_phase: job.capturePhase,
    warm_stage: job.warmStage,
    warm_stage_started_at: job.warmStageStartedAt,
    output_tail: job.outputTail,
    prestage_hosts: parseOr(job.prestageHosts),
  };
}

function parseOr(s: string): unknown {
  try {
    return JSON.parse(s);
  } catch {
    return {};
  }
}

function jobProgress(job: EnableJob): string {
  if (job.chunksTotal !== undefined && job.chunksTotal > 0) {
    return `${job.chunksDone}/${job.chunksTotal} chunks`;
  }
  return [job.capturePhase, job.warmStage].filter(Boolean).join(" / ");
}

function printJob(job: EnableJob): void {
  detail([
    ["id", job.id],
    ["state", job.state],
    ["image_uri", job.imageUri],
    ["manifest_digest", job.manifestDigest],
    ["progress", jobProgress(job) || undefined],
    ["attempts", String(job.attempts)],
    ["error", job.error],
    ["capture_phase", job.capturePhase],
    ["warm_stage", job.warmStage],
    ["warm_started_at", job.warmStageStartedAt],
    ["created_at", job.createdAt],
    ["updated_at", job.updatedAt],
  ]);
  if (job.prestageHosts && job.prestageHosts !== "{}") {
    console.log(`prestage_hosts  : ${job.prestageHosts}`);
  }
  if (job.outputTail) {
    console.log(`\noutput_tail:\n${job.outputTail}`);
  }
}

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

/** Poll an enable job until ready/failed, rendering a progress line. */
export async function pollJob(c: Clients, jobId: string, json: boolean): Promise<void> {
  let printedProgress = false;
  for (;;) {
    const resp = await c.image.getEnableJob({ jobId }).catch(failWith);
    const job = resp.job;
    if (!job) failWith(new Error("get-enable-job carried no job"));
    if (job.state === "ready") {
      if (printedProgress) process.stderr.write("\n");
      if (json) printJson(jobJson(job));
      else console.log(`enabled: uri=${job.imageUri} digest=${job.manifestDigest ?? ""}`);
      return;
    }
    if (job.state === "failed") {
      if (printedProgress) process.stderr.write("\n");
      fail(
        `enable job ${jobId} failed: ${job.error ?? "unknown error"} ` +
          `(retry with \`engrams image retry-job ${jobId}\` after fixing the cause)`,
      );
    }
    process.stderr.write(`\r${job.state.padEnd(14)} ${jobProgress(job).padEnd(24)}`);
    printedProgress = true;
    await sleep(2000);
  }
}

// ---- image verbs ----------------------------------------------------------

/** Print an enabled image's stored config, the row the dashboard edits, as JSON. */
export async function config(c: Clients, uri: string): Promise<void> {
  const resp = await c.image.listEnabledImages({}).catch(failWith);
  const img = resp.images.find((i) => i.imageUri === uri);
  if (!img) fail(`no enabled image with uri ${uri}`);
  const cfg = img.config;
  if (!cfg) fail(`enabled image ${uri} has no stored config`);
  printJson({
    name: cfg.name,
    description: cfg.description,
    env: cfg.env,
    workdir: cfg.workdir,
    resources: cfg.resources
      ? {
          suggested_vcpus: cfg.resources.suggestedVcpus,
          suggested_memory_mib: cfg.resources.suggestedMemoryMib,
          suggested_disk_gib: cfg.resources.suggestedDiskGib,
          suggested_swap_mib: cfg.resources.suggestedSwapMib,
        }
      : undefined,
    warm: cfg.warm
      ? {
          command: cfg.warm.command,
          timeout_secs: cfg.warm.timeoutSecs !== undefined ? Number(cfg.warm.timeoutSecs) : undefined,
          workdir: cfg.warm.workdir,
          env: cfg.warm.env.map((e) => ({
            name: e.name,
            kind: e.value.case === "literal" ? "literal" : "secret_ref",
            ...(e.value.case === "literal" ? { value: e.value.value } : { secret_ref: e.value.value }),
          })),
          network: cfg.warm.network
            ? {
                default: cfg.warm.network.default,
                allow_hosts: cfg.warm.network.allowHosts,
                allow_host_patterns: cfg.warm.network.allowHostPatterns,
              }
            : undefined,
        }
      : undefined,
  });
}

export async function list(c: Clients, json: boolean): Promise<void> {
  const resp = await c.image.listEnabledImages({}).catch(failWith);
  if (json) {
    printJson({
      images: resp.images.map((img) => ({
        id: img.id,
        image_uri: img.imageUri,
        manifest_digest: img.manifestDigest,
        name: img.config?.name,
        description: img.config?.description,
        last_refreshed_at: img.lastRefreshedAt,
        created_at: img.createdAt,
      })),
    });
    return;
  }
  if (resp.images.length === 0) {
    console.log("(no images enabled — `engrams image enable --uri <uri> --name <name> --vcpus <n>` to add one)");
    return;
  }
  table(
    ["URI", "NAME", "DIGEST"],
    resp.images.map((img) => [img.imageUri, img.config?.name ?? "?", img.manifestDigest]),
    [48, 22, 71],
  );
}

export async function enable(
  c: Clients,
  uri: string,
  flags: ConfigFlags,
  noWait: boolean,
  json: boolean,
): Promise<void> {
  // No config flags on an already-enabled URI re-uses the stored config.
  const config = hasConfigFlags(flags) ? buildImageConfig(flags) : undefined;
  const resp = await c.image.enableImage({ imageUri: uri, config }).catch(failWith);
  const job = resp.job;
  if (!job) failWith(new Error("enable response carried no job"));
  if (noWait) {
    if (json) printJson(jobJson(job));
    else console.log(`enable job ${job.id} accepted; poll with \`engrams image poll-job ${job.id}\``);
    return;
  }
  await pollJob(c, job.id, json);
}

export async function update(
  c: Clients,
  uri: string,
  flags: ConfigFlags,
  allowRecapture: boolean,
  noWait: boolean,
  json: boolean,
): Promise<void> {
  if (!hasConfigFlags(flags)) fail("nothing to change: pass at least one config flag");
  const listed = await c.image.listEnabledImages({}).catch(failWith);
  const img = listed.images.find((i) => i.imageUri === uri);
  if (!img) fail(`no enabled image with uri ${uri}`);
  const config = buildImageConfig(flags, img.config);
  const resp = await c.image
    .updateImage({ imageUri: uri, config, allowRecapture })
    .catch(failWith);
  if (!resp.job) {
    if (json) printJson({ applied: "immediate" });
    else console.log("config updated (no recapture needed; live immediately)");
    return;
  }
  if (noWait) {
    if (json) printJson(jobJson(resp.job));
    else console.log(`recapture job ${resp.job.id} accepted`);
    return;
  }
  await pollJob(c, resp.job.id, json);
}

export async function disable(c: Clients, uri: string): Promise<void> {
  await c.image.disableImage({ imageUri: uri }).catch(failWith);
  console.log("disabled");
}

export async function refresh(
  c: Clients,
  uri: string,
  recapture: boolean,
  json: boolean,
): Promise<void> {
  const resp = await c.image
    .refreshImage({ imageUri: uri, forceRecapture: recapture })
    .catch(failWith);
  const job = resp.job;
  if (!job) failWith(new Error("refresh response carried no job"));
  await pollJob(c, job.id, json);
}

export async function jobs(c: Clients, json: boolean): Promise<void> {
  const resp = await c.image.listEnableJobs({}).catch(failWith);
  if (json) {
    printJson({ jobs: resp.jobs.map(jobJson) });
    return;
  }
  if (resp.jobs.length === 0) {
    console.log("(no recent enable jobs)");
    return;
  }
  table(
    ["ID", "STATE", "ATTEMPTS", "DETAIL", "URI"],
    resp.jobs.map((job) => [
      job.id,
      job.state,
      String(job.attempts),
      truncate(job.state === "failed" ? (job.error ?? "failed") : jobProgress(job), 22),
      truncate(job.imageUri, 64),
    ]),
    [36, 13, 8, 22, 64],
  );
}

export async function job(c: Clients, id: string, json: boolean): Promise<void> {
  const resp = await c.image.getEnableJob({ jobId: id }).catch(failWith);
  if (!resp.job) failWith(new Error("get-enable-job response carried no job"));
  if (json) printJson(jobJson(resp.job));
  else printJob(resp.job);
}

export async function retryJob(
  c: Clients,
  id: string,
  noWait: boolean,
  json: boolean,
): Promise<void> {
  const resp = await c.image.retryEnableJob({ jobId: id }).catch(failWith);
  const job = resp.job;
  if (!job) failWith(new Error("retry-enable-job response carried no job"));
  if (noWait) {
    if (json) printJson(jobJson(job));
    else console.log(`enable job ${job.id} requeued; poll with \`engrams image poll-job ${job.id}\``);
    return;
  }
  await pollJob(c, job.id, json);
}

// ---- registry verbs -------------------------------------------------------

export interface RegistryAddOpts {
  host: string;
  authKind: string;
  username?: string;
  passwordFile?: string;
  passwordStdin: boolean;
  impersonateSa?: string;
}

export async function registryAdd(c: Clients, opts: RegistryAddOpts, json: boolean): Promise<void> {
  let auth;
  switch (opts.authKind) {
    case "static": {
      if (!opts.username) fail("--auth-kind static requires --username");
      let password: string;
      if (opts.passwordFile) {
        password = readFileSync(opts.passwordFile, "utf8").replace(/\n$/, "");
      } else if (opts.passwordStdin) {
        password = (await Bun.stdin.text()).replace(/\n$/, "");
      } else {
        fail("static auth requires --password-file <path> or --password-stdin");
      }
      if (!password) fail("password is empty");
      auth = {
        case: "static" as const,
        value: { username: opts.username, password },
      };
      break;
    }
    case "gcp-workload-identity":
    case "gcp_workload_identity":
      auth = {
        case: "gcpWorkloadIdentity" as const,
        value: { impersonateSa: opts.impersonateSa },
      };
      break;
    default:
      fail(`unknown --auth-kind \`${opts.authKind}\` (expected: static | gcp-workload-identity)`);
  }
  const resp = await c.image.addRegistry({ host: opts.host, auth }).catch(failWith);
  if (json) {
    printJson({
      id: resp.id,
      host: resp.host,
      auth_kind: resp.authKind,
      auth_principal: resp.authPrincipal,
    });
  } else {
    console.log(
      `added: host=${resp.host} auth_kind=${resp.authKind} principal=${resp.authPrincipal ?? "(none)"}`,
    );
  }
}

export async function registryList(c: Clients, json: boolean): Promise<void> {
  const resp = await c.image.listRegistries({}).catch(failWith);
  if (json) {
    printJson({
      registries: resp.registries.map((r) => ({
        id: r.id,
        registry_host: r.registryHost,
        auth_kind: r.authKind,
        auth_principal: r.authPrincipal,
        created_at: r.createdAt,
        updated_at: r.updatedAt,
      })),
    });
    return;
  }
  if (resp.registries.length === 0) {
    console.log("(no registries configured)");
    return;
  }
  table(
    ["HOST", "AUTH_KIND", "PRINCIPAL"],
    resp.registries.map((r) => [r.registryHost, r.authKind, r.authPrincipal ?? "(none)"]),
    [40, 24, 40],
  );
}

export async function registryRm(c: Clients, host: string): Promise<void> {
  await c.image.deleteRegistry({ host }).catch(failWith);
  console.log("removed");
}
