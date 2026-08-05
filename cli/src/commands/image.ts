/**
 * engrams image … / engrams registry … — ImageService passthrough verbs.
 *
 * `enable --config` parses the image-config TOML client-side into the proto
 * shape; STRICT validation stays server-side (the coordinator rejects typos /
 * retired sections with the serde message) — the retired Rust CLI duplicated
 * engram-core's validate(), which the one-server-validator model replaces.
 */

import { readFileSync } from "node:fs";
import { parse as parseToml, stringify as stringifyToml } from "smol-toml";

import type { Clients } from "../client.ts";
import { detail, fail, failWith, printJson, table, truncate } from "../output.ts";
import type { EnableJob, ImageConfig } from "../gen/engram/app/v1/image_pb.ts";
import type { MessageInitShape } from "@bufbuild/protobuf";
import type { ImageConfigSchema } from "../gen/engram/app/v1/image_pb.ts";

type ImageConfigInit = MessageInitShape<typeof ImageConfigSchema>;

// ---- image-config TOML → proto ------------------------------------------

/** The TOML shape (engram_core::types::image::ImageConfig, snake_case). */
interface ConfigToml {
  name?: string;
  description?: string;
  env?: Record<string, string>;
  workdir?: string;
  resources?: {
    suggested_memory_mib?: number;
    suggested_vcpus?: number;
    suggested_disk_gib?: number;
  };
  warm?: {
    command?: string[];
    timeout_secs?: number;
    workdir?: string;
    env?: Array<{ name?: string; value?: string; secret_ref?: string }>;
    network?: {
      default?: string;
      allow_hosts?: string[];
      allow_host_patterns?: string[];
    };
  };
}

export function loadImageConfig(path: string): ImageConfigInit {
  let text: string;
  try {
    text = readFileSync(path, "utf8");
  } catch (e) {
    fail(`read ${path}: ${e instanceof Error ? e.message : e}`);
  }
  let c: ConfigToml;
  try {
    c = parseToml(text) as ConfigToml;
  } catch (e) {
    fail(`${path} is not valid TOML: ${e instanceof Error ? e.message : e}`);
  }
  if (!c.name) fail(`${path}: image config requires a non-empty \`name\``);
  return {
    name: c.name,
    description: c.description,
    env: c.env ?? {},
    workdir: c.workdir,
    resources: {
      suggestedMemoryMib: c.resources?.suggested_memory_mib,
      suggestedVcpus: c.resources?.suggested_vcpus,
      suggestedDiskGib: c.resources?.suggested_disk_gib,
    },
    warm: c.warm
      ? {
          command: c.warm.command ?? [],
          timeoutSecs:
            c.warm.timeout_secs !== undefined ? BigInt(c.warm.timeout_secs) : undefined,
          workdir: c.warm.workdir,
          env: (c.warm.env ?? []).map((e) => {
            if (!e.name) fail(`${path}: [[warm.env]] entry missing \`name\``);
            if ((e.value === undefined) === (e.secret_ref === undefined)) {
              fail(`${path}: [[warm.env]] ${e.name}: exactly one of value/secret_ref`);
            }
            return {
              name: e.name,
              value:
                e.value !== undefined
                  ? { case: "literal" as const, value: e.value }
                  : { case: "secretRef" as const, value: e.secret_ref! },
            };
          }),
          network: c.warm.network
            ? {
                default: c.warm.network.default ?? "deny",
                allowHosts: c.warm.network.allow_hosts ?? [],
                allowHostPatterns: c.warm.network.allow_host_patterns ?? [],
              }
            : undefined,
        }
      : undefined,
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

/** Drop keys whose value is undefined so smol-toml can serialize the object. */
function compact<T extends Record<string, unknown>>(o: T): Record<string, unknown> {
  return Object.fromEntries(Object.entries(o).filter(([, v]) => v !== undefined));
}

/**
 * Print an enabled image's LIVE stored config (the enabled_images row — the
 * source of truth the UI edits) as image-config TOML. The output round-trips
 * through `enable --config` / `update --config`, so this is the backup and
 * disaster-recovery path now that repos no longer keep a config file.
 */
export async function config(c: Clients, uri: string, json: boolean): Promise<void> {
  const resp = await c.image.listEnabledImages({}).catch(failWith);
  const img = resp.images.find((i) => i.imageUri === uri);
  if (!img) fail(`no enabled image with uri ${uri}`);
  const cfg = img.config;
  if (!cfg) fail(`enabled image ${uri} has no stored config`);
  const out = compact({
    name: cfg.name,
    description: cfg.description,
    env: Object.keys(cfg.env).length > 0 ? cfg.env : undefined,
    workdir: cfg.workdir,
    resources: cfg.resources
      ? compact({
          suggested_memory_mib: cfg.resources.suggestedMemoryMib,
          suggested_vcpus: cfg.resources.suggestedVcpus,
          suggested_disk_gib: cfg.resources.suggestedDiskGib,
        })
      : undefined,
    warm: cfg.warm
      ? compact({
          command: cfg.warm.command,
          timeout_secs:
            cfg.warm.timeoutSecs !== undefined ? Number(cfg.warm.timeoutSecs) : undefined,
          workdir: cfg.warm.workdir,
          env: cfg.warm.env.length > 0
            ? cfg.warm.env.map((e) =>
                compact({
                  name: e.name,
                  value: e.value.case === "literal" ? e.value.value : undefined,
                  secret_ref: e.value.case === "secretRef" ? e.value.value : undefined,
                }),
              )
            : undefined,
          network: cfg.warm.network
            ? compact({
                default: cfg.warm.network.default,
                allow_hosts:
                  cfg.warm.network.allowHosts.length > 0
                    ? cfg.warm.network.allowHosts
                    : undefined,
                allow_host_patterns:
                  cfg.warm.network.allowHostPatterns.length > 0
                    ? cfg.warm.network.allowHostPatterns
                    : undefined,
              })
            : undefined,
        })
      : undefined,
  });
  if (json) {
    printJson(out);
    return;
  }
  console.log(stringifyToml(out));
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
    console.log("(no images enabled — `engrams image enable --uri <uri>` to add one)");
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
  configPath: string | undefined,
  noWait: boolean,
  json: boolean,
): Promise<void> {
  const config = configPath ? loadImageConfig(configPath) : undefined;
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
  configPath: string,
  allowRecapture: boolean,
  noWait: boolean,
  json: boolean,
): Promise<void> {
  const config = loadImageConfig(configPath);
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
