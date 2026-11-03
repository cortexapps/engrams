import { open, lstat } from "node:fs/promises";
import { constants, type Stats } from "node:fs";
import { createHash } from "node:crypto";
import { join } from "node:path";
import { z } from "zod";
import { realOwnedPath, requireThat, sha256, type Context } from "./context.ts";
import { sourceKeySchema, type DatabaseObserver } from "./observer.ts";
import { canonicalJson } from "./guest.ts";

const hex = z.string().regex(/^[a-f0-9]{64}$/);
const digest = z.string().regex(/^sha256:[a-f0-9]{64}$/);
const bytes = z
  .number()
  .int()
  .positive()
  .max(16 * 1024 ** 3);
const manifest = z.strictObject({
  manifest_id: z.uuid(),
  version: z.number().int().positive().safe(),
});
const object = z.record(z.string(), z.unknown());
const auxDrives = z
  .array(
    z.strictObject({
      drive_id: z.string(),
      guest_mount: z.string(),
      fs_type: z.literal("squashfs"),
      sha256: hex,
    }),
  )
  .min(3)
  .max(12)
  .refine((items) =>
    items.every(
      (v, i) =>
        v.drive_id === `dyn-${i}` && v.guest_mount === `/opt/engram/dyn/${i}`,
    ),
  );
const captureRequest = z.strictObject({
  key: sourceKeySchema,
  capture_id: z.uuid(),
});
const captureAuthority = z.strictObject({
  version: z.literal(1),
  request: captureRequest,
  host_incarnation: z.string().min(1).max(128),
  disk_manifest: manifest,
  disk_sha256: hex,
  disk_size_bytes: bytes,
  image: z.string().min(1).max(256),
  kernel_sha256: hex,
  aux_ro_drives: auxDrives,
  consistency: z.literal("crash_consistent_disk_cut_v1"),
  source_state: z.literal("destroyed"),
  record_digest: digest,
});
const cold = z.strictObject({
  version: z.literal(1),
  session_id: z.uuid(),
  sandbox_id: z.uuid(),
  boot_attempt: z.string().min(1).max(128),
  image: z.string().min(1).max(256),
  disk: z.strictObject({
    manifest,
    sha256: hex,
    size_bytes: bytes,
    source: z.discriminatedUnion("kind", [
      z.strictObject({
        kind: z.literal("fresh_image"),
        base_manifest: manifest,
      }),
      z.strictObject({
        kind: z.literal("capture"),
        capture_id: z.uuid(),
        session_id: z.uuid(),
        sandbox_id: z.uuid(),
        binding_epoch: z.number().int().positive().safe(),
        record_digest: digest,
      }),
    ]),
  }),
  kernel_sha256: hex,
  aux_ro_drives: auxDrives,
});
const request = z.strictObject({
  key: sourceKeySchema,
  cold,
  bootstrap_policy: object,
  capture_authority: captureAuthority.nullable(),
  sandbox_spec_json: z.string().max(1024 * 1024),
});
const recordSchema = z.strictObject({
  version: z.literal(1),
  request,
  incarnation: z.string().min(1).max(128),
  state: object,
  allocation: object.nullable(),
  prepared: object.nullable(),
  applied: object.nullable(),
  apply_digest: digest.nullable(),
  finalization: digest.nullable(),
  capture: z
    .strictObject({
      request: captureRequest,
      manifest,
      artifact: z.tuple([hex, bytes]).nullable(),
      authority: captureAuthority.nullable(),
    })
    .nullable(),
  retired: z.tuple([hex, bytes]).nullable(),
});
export type HostRecord = z.infer<typeof recordSchema>;
const startSchema = z.strictObject({
  receipt: z.strictObject({
    key: sourceKeySchema,
    prepared_receipt_id: digest,
    finalization_digest: digest,
    host_incarnation: z.string().min(1).max(128),
    network_identity: digest,
    receipt_id: digest,
  }),
  start_id: digest,
});
export function verifyPhysicalBoot(
  ctx: Pick<Context, "backend" | "hostId" | "imageDigest"> & {
    artifacts: { kernel: { sha256: string } };
  },
  record: HostRecord,
  current: Awaited<ReturnType<DatabaseObserver["started"]>>,
  observed: Awaited<ReturnType<DatabaseObserver["physicalBoot"]>>,
) {
  const assets = z
    .strictObject({
      request: z.strictObject({
        version: z.literal(1),
        host_id: z.uuid(),
        session_id: z.uuid(),
        intent_id: z.string(),
        image: z.string(),
        image_digest: digest,
        disk_manifest: manifest,
        aux_ro_drives: auxDrives,
      }),
      host_incarnation: z.string(),
      backend: z.enum(["firecracker", "vz"]),
      kernel_sha256: hex,
      disk_sha256: hex,
      disk_size_bytes: bytes,
    })
    .parse(boundedHostJson(observed.assets, 65536));
  const start = startSchema.parse(boundedHostJson(observed.start, 65536));
  const hostState = z
    .strictObject({ state: z.literal("started"), intent: startSchema })
    .parse(record.state);
  const prepared = z
    .object({
      request,
      host_incarnation: z.string(),
      network_identity: digest,
      receipt_id: digest,
    })
    .parse(record.prepared);
  requireThat(
    current.phase === "started" &&
      canonicalJson(record.request.key) === canonicalJson(current.source_key) &&
      canonicalJson(start) === canonicalJson(hostState.intent) &&
      canonicalJson(start.receipt.key) === canonicalJson(current.source_key) &&
      start.receipt.host_incarnation === record.incarnation &&
      start.receipt.finalization_digest === record.finalization &&
      canonicalJson(prepared.request) === canonicalJson(record.request) &&
      prepared.host_incarnation === record.incarnation &&
      prepared.receipt_id === start.receipt.prepared_receipt_id &&
      prepared.network_identity === start.receipt.network_identity &&
      assets.host_incarnation === record.incarnation &&
      assets.backend === ctx.backend &&
      assets.request.host_id === ctx.hostId &&
      assets.request.session_id === current.source_key.creation.session_id &&
      assets.request.intent_id === current.boot_attempt &&
      assets.request.image_digest === ctx.imageDigest &&
      assets.request.image === record.request.cold.image &&
      canonicalJson(assets.request.disk_manifest) ===
        canonicalJson(record.request.cold.disk.manifest) &&
      assets.kernel_sha256 === record.request.cold.kernel_sha256 &&
      assets.kernel_sha256 === ctx.artifacts.kernel.sha256 &&
      assets.disk_sha256 === record.request.cold.disk.sha256 &&
      assets.disk_size_bytes === record.request.cold.disk.size_bytes &&
      canonicalJson(assets.request.aux_ro_drives) ===
        canonicalJson(record.request.cold.aux_ro_drives),
    "physical_actual_backend_chain",
  );
  const combined = Buffer.from(
    z
      .array(z.number().int().min(0).max(255))
      .min(1)
      .max(1048576)
      .parse(boundedHostJson(observed.combined_manifest, 8388608)),
  );
  requireThat(
    observed.native_digest === current.native_digest &&
      observed.native_digest === `sha256:${sha256(observed.native_bytes)}` &&
      observed.combined_manifest_digest === current.combined_manifest_digest &&
      observed.combined_manifest_digest === `sha256:${sha256(combined)}`,
    "physical_manifest_integrity",
  );
  const entry = z.object({
    name: z.string().min(1),
    description: z.string(),
    inputSchema: object,
  });
  const native = z
    .array(entry.passthrough())
    .max(128)
    .parse(boundedHostJson(observed.native_bytes.toString("utf8"), 1048576));
  const all = z
    .array(entry.passthrough())
    .max(128)
    .parse(boundedHostJson(combined.toString("utf8"), 1048576));
  requireThat(
    native.length <= all.length &&
      new Set(all.map((v) => v.name)).size === all.length &&
      canonicalJson(all.slice(0, native.length)) === canonicalJson(native),
    "physical_native_manifest_prefix",
  );
  return {
    backend: assets.backend,
    nativeSha256: sha256(observed.native_bytes),
    combinedSha256: sha256(combined),
    nativeCount: native.length,
    combinedCount: all.length,
    assetsSha256: sha256(observed.assets),
    startSha256: sha256(observed.start),
  };
}
export function verifyColdChain(old: HostRecord, next: HostRecord) {
  const authority = old.capture?.authority;
  requireThat(
    authority &&
      old.state.state === "captured" &&
      canonicalJson(old.state.authority) === canonicalJson(authority),
    "physical_old_capture_destroyed",
  );
  const source = next.request.cold.disk.source;
  requireThat(
    source.kind === "capture" &&
      authority.image === old.request.cold.image &&
      authority.kernel_sha256 === old.request.cold.kernel_sha256 &&
      canonicalJson(authority.aux_ro_drives) ===
        canonicalJson(old.request.cold.aux_ro_drives) &&
      canonicalJson(next.request.capture_authority) ===
        canonicalJson(authority) &&
      source.capture_id === authority.request.capture_id &&
      source.session_id === old.request.key.creation.session_id &&
      source.sandbox_id === old.request.key.context.target.sandbox_id &&
      source.binding_epoch === old.request.key.context.target.binding_epoch &&
      source.record_digest === authority.record_digest &&
      canonicalJson(next.request.key.creation) ===
        canonicalJson(old.request.key.creation) &&
      next.request.key.native_digest === old.request.key.native_digest &&
      next.request.key.context.target.binding_epoch ===
        source.binding_epoch + 1 &&
      next.request.key.context.target.sandbox_id !== source.sandbox_id &&
      next.request.key.boot_attempt !== old.request.key.boot_attempt &&
      next.request.cold.disk.sha256 === authority.disk_sha256 &&
      next.request.cold.disk.size_bytes === authority.disk_size_bytes &&
      canonicalJson(next.request.cold.disk.manifest) ===
        canonicalJson(authority.disk_manifest) &&
      next.request.cold.image === authority.image &&
      next.request.cold.kernel_sha256 === authority.kernel_sha256 &&
      canonicalJson(next.request.cold.aux_ro_drives) ===
        canonicalJson(authority.aux_ro_drives),
    "physical_exact_capture_to_cold_chain",
  );
  return {
    captureSha256: authority.disk_sha256,
    authoritySha256: sha256(canonicalJson(authority)),
  };
}
export function boundedHostJson(text: string, limit: number) {
  requireThat(Buffer.byteLength(text) <= limit, "host_json_bytes");
  const value: unknown = JSON.parse(text);
  const stack = [{ value, depth: 0 }];
  while (stack.length) {
    const current = stack.pop()!;
    requireThat(current.depth <= 64, "host_json_depth");
    if (current.value !== null && typeof current.value === "object")
      for (const child of Object.values(current.value))
        stack.push({ value: child, depth: current.depth + 1 });
  }
  return value;
}
export function parseHostRecord(encoded: Buffer, sandboxId: string) {
  requireThat(z.uuid().safeParse(sandboxId).success, "host_sandbox_identity");
  const envelope = z
    .strictObject({
      schema: z.literal(1),
      content_hash: hex,
      id: z.uuid(),
      body: z.string(),
    })
    .parse(
      boundedHostJson(
        new TextDecoder("utf-8", { fatal: true }).decode(encoded),
        16 * 1024 ** 2,
      ),
    );
  requireThat(
    envelope.id === sandboxId &&
      envelope.content_hash === sha256(envelope.body),
    "host_envelope_integrity",
  );
  const raw = boundedHostJson(envelope.body, 8 * 1024 ** 2);
  const record = recordSchema.parse(raw);
  requireThat(
    record.request.key.context.target.sandbox_id === sandboxId &&
      record.request.cold.sandbox_id === sandboxId &&
      record.request.cold.session_id ===
        record.request.key.creation.session_id &&
      record.request.cold.boot_attempt === record.request.key.boot_attempt,
    "host_record_target",
  );
  const spec = z
    .object({ env: object })
    .parse(boundedHostJson(record.request.sandbox_spec_json, 1024 ** 2));
  requireThat(
    Object.keys(spec.env).length === 0,
    "host_spec_has_no_environment",
  );
  // The production digest uses Rust struct field order. Verify the raw serialized object order.
  const rawRecord = object.parse(raw);
  if (record.capture?.authority) {
    const authority = object.parse(object.parse(rawRecord.capture).authority);
    requireThat(
      authority.record_digest ===
        `sha256:${sha256(JSON.stringify({ ...authority, record_digest: "" }))}`,
      "host_capture_record_digest",
    );
    requireThat(
      canonicalJson(record.capture.request.key) ===
        canonicalJson(record.request.key) &&
        record.capture.authority.host_incarnation === record.incarnation &&
        canonicalJson(record.capture.request) ===
          canonicalJson(record.capture.authority.request) &&
        canonicalJson(record.capture.manifest) ===
          canonicalJson(record.capture.authority.disk_manifest) &&
        record.capture.manifest.manifest_id ===
          record.capture.request.capture_id &&
        record.capture.manifest.version === 1 &&
        canonicalJson(record.capture.artifact) ===
          canonicalJson([
            record.capture.authority.disk_sha256,
            record.capture.authority.disk_size_bytes,
          ]),
      "host_capture_exact_authority",
    );
  }
  return { record, sha256: sha256(encoded) };
}
function privateStat(stat: Stats) {
  requireThat(
    stat.isFile() &&
      stat.uid === process.getuid?.() &&
      (stat.mode & 0o777) === 0o600 &&
      stat.nlink === 1,
    "host_private_file",
  );
}
function unchanged(before: Stats, after: Stats) {
  requireThat(
    before.ino === after.ino &&
      before.dev === after.dev &&
      before.size === after.size &&
      before.mtimeMs === after.mtimeMs &&
      before.ctimeMs === after.ctimeMs,
    "host_file_changed_during_read",
  );
}
export class HostObserver {
  constructor(private readonly ctx: Context) {}
  async boot(sandboxId: string) {
    requireThat(z.uuid().safeParse(sandboxId).success, "host_sandbox_identity");
    const path = join(this.ctx.observer.hostManagedRoot, `${sandboxId}.boot`);
    await realOwnedPath(this.ctx.runRoot, path);
    const file = await open(path, constants.O_RDONLY | constants.O_NOFOLLOW);
    try {
      const before = await file.stat();
      privateStat(before);
      requireThat(
        before.size > 0 && before.size <= 16 * 1024 ** 2,
        "host_record_bytes",
      );
      const data = Buffer.alloc(before.size);
      let offset = 0;
      const deadline = performance.now() + this.ctx.deadlines.stepMs;
      while (offset < data.length) {
        requireThat(performance.now() < deadline, "host_record_read_deadline");
        const read = await file.read(
          data,
          offset,
          data.length - offset,
          offset,
        );
        requireThat(read.bytesRead > 0, "host_record_complete");
        offset += read.bytesRead;
      }
      unchanged(before, await file.stat());
      unchanged(before, await lstat(path));
      return parseHostRecord(data, sandboxId);
    } finally {
      await file.close();
    }
  }
  async captureFile(record: HostRecord) {
    requireThat(
      record.capture?.authority && record.capture.artifact,
      "host_capture_available",
    );
    const authority = record.capture.authority;
    const path = join(
      this.ctx.observer.hostManagedRoot,
      `${authority.request.capture_id}.disk`,
    );
    await realOwnedPath(this.ctx.runRoot, path);
    const file = await open(path, constants.O_RDONLY | constants.O_NOFOLLOW);
    try {
      const before = await file.stat();
      privateStat(before);
      requireThat(
        before.size === authority.disk_size_bytes &&
          before.size <= 16 * 1024 ** 3,
        "host_capture_size",
      );
      const hash = createHash("sha256");
      const buffer = Buffer.alloc(65536);
      const deadline = performance.now() + this.ctx.deadlines.stepMs;
      let offset = 0;
      while (offset < before.size) {
        requireThat(performance.now() < deadline, "host_capture_read_deadline");
        const read = await file.read(
          buffer,
          0,
          Math.min(buffer.length, before.size - offset),
          offset,
        );
        requireThat(read.bytesRead > 0, "host_capture_complete");
        hash.update(buffer.subarray(0, read.bytesRead));
        offset += read.bytesRead;
      }
      unchanged(before, await file.stat());
      unchanged(before, await lstat(path));
      const actual = hash.digest("hex");
      requireThat(actual === authority.disk_sha256, "host_capture_actual_hash");
      return { sha256: actual, bytes: offset };
    } finally {
      await file.close();
    }
  }
  async logBaseline() {
    await realOwnedPath(this.ctx.runRoot, this.ctx.observer.hostLogFile);
    const file = await open(
      this.ctx.observer.hostLogFile,
      constants.O_RDONLY | constants.O_NOFOLLOW,
    );
    try {
      const stat = await file.stat();
      privateStat(stat);
      const last = Buffer.alloc(1);
      if (stat.size) await file.read(last, 0, 1, stat.size - 1);
      return {
        inode: stat.ino,
        device: stat.dev,
        offset: stat.size,
        partial: stat.size > 0 && last[0] !== 10,
      };
    } finally {
      await file.close();
    }
  }
  async proxyDenials(
    baseline: Awaited<ReturnType<HostObserver["logBaseline"]>>,
    sessionId: string,
    sni: string,
  ) {
    requireThat(
      z.uuid().safeParse(sessionId).success &&
        /^[a-z0-9.-]+\.fixture\.example$/.test(sni),
      "proxy_observer_identity",
    );
    await realOwnedPath(this.ctx.runRoot, this.ctx.observer.hostLogFile);
    const file = await open(
      this.ctx.observer.hostLogFile,
      constants.O_RDONLY | constants.O_NOFOLLOW,
    );
    try {
      const before = await file.stat();
      privateStat(before);
      requireThat(
        before.ino === baseline.inode &&
          before.dev === baseline.device &&
          before.size >= baseline.offset &&
          before.size - baseline.offset <= 8 * 1024 ** 2,
        "host_log_window",
      );
      const data = Buffer.alloc(before.size - baseline.offset);
      let offset = 0;
      const deadline = performance.now() + this.ctx.deadlines.stepMs;
      while (offset < data.length) {
        requireThat(performance.now() < deadline, "host_log_read_deadline");
        const read = await file.read(
          data,
          offset,
          data.length - offset,
          baseline.offset + offset,
        );
        requireThat(read.bytesRead > 0, "host_log_complete");
        offset += read.bytesRead;
      }
      const after = await file.stat();
      const current = await lstat(this.ctx.observer.hostLogFile);
      requireThat(
        after.ino === before.ino &&
          current.ino === before.ino &&
          current.dev === before.dev &&
          after.size >= before.size &&
          current.size >= before.size,
        "host_log_rotation_or_truncation",
      );
      return parseProxyDenials(data, baseline.partial, sessionId, sni);
    } finally {
      await file.close();
    }
  }
}
export function parseProxyDenials(
  data: Buffer,
  partial: boolean,
  sessionId: string,
  sni: string,
) {
  requireThat(data.length <= 8 * 1024 ** 2, "host_log_window");
  const lines = data.toString("utf8").split("\n");
  requireThat(
    lines.every((line) => Buffer.byteLength(line) <= 65536),
    "host_log_line_bound",
  );
  lines.pop();
  if (partial) lines.shift();
  return lines.flatMap((line) => {
    requireThat(Buffer.byteLength(line) <= 65536, "host_log_line_bound");
    const entry = z
      .object({ target: z.string().optional(), fields: object })
      .parse(boundedHostJson(line, 65536));
    return entry.target === "engram_egress_proxy::proxy" &&
      entry.fields.message ===
        "egress rejected — destination not in any allow_hosts" &&
      entry.fields.session_id === sessionId &&
      entry.fields.sni === sni
      ? [{ sha256: sha256(line + "\n"), bytes: Buffer.byteLength(line) }]
      : [];
  });
}
