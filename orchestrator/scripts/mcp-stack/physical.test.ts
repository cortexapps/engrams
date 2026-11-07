import { expect, test } from "bun:test";
import { mkdtemp, realpath, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createServer } from "node:tls";
import {
  TEST_CA,
  TEST_CERT,
  TEST_KEY,
} from "../../../mcp-worker/test/fixtures/tls.ts";
import { X509Certificate } from "node:crypto";
import { sha256 } from "./context.ts";
import {
  boundedHostJson,
  parseHostRecord,
  parseProxyDenials,
  verifyColdChain,
  verifyPhysicalBoot,
} from "./host-observer.ts";
import { probeWorkerControl } from "./guest-physical.ts";
import type { Context } from "./context.ts";

const id = (n: number) =>
  `00000000-0000-4000-8000-${String(n).padStart(12, "0")}`;
const h = "a".repeat(64),
  d = `sha256:${h}`;
const aux = Array.from({ length: 3 }, (_, i) => ({
  drive_id: `dyn_${i}`,
  guest_mount: `/opt/engram/dyn/${i}`,
  fs_type: "squashfs",
  sha256: h,
}));
const manifest = { manifest_id: id(4), version: 1 };
const key = () => ({
  creation: {
    session_id: id(1),
    task_id: "task",
    policy: { actorId: "actor", inputId: "input", eligibility: "interactive" },
    input_digest: d,
  },
  context: {
    target: {
      session_id: id(1),
      sandbox_id: id(2),
      host_id: id(3),
      binding_epoch: 1,
    },
    operator_generation: 1,
    operator_identity: d,
    binding_hash: null,
  },
  boot_attempt: "boot-first",
  native_digest: d,
});
function rawRecord() {
  const request = {
    key: key(),
    cold: {
      version: 1,
      session_id: id(1),
      sandbox_id: id(2),
      boot_attempt: "boot-first",
      image: "fixture-image",
      disk: {
        manifest,
        sha256: h,
        size_bytes: 4,
        source: {
          kind: "fresh_image",
          base_manifest: { manifest_id: id(5), version: 1 },
        },
      },
      kernel_sha256: h,
      aux_ro_drives: aux,
    },
    bootstrap_policy: {},
    capture_authority: null,
    sandbox_spec_json: '{"env":{}}',
  };
  return {
    version: 1,
    request,
    incarnation: "host-first",
    state: {
      state: "started",
      intent: {
        receipt: {
          key: key(),
          prepared_receipt_id: d,
          finalization_digest: d,
          host_incarnation: "host-first",
          network_identity: d,
          receipt_id: d,
        },
        start_id: d,
      },
    },
    allocation: null,
    prepared: {
      request,
      host_incarnation: "host-first",
      network_identity: d,
      receipt_id: d,
      guest_ip: "192.0.2.2",
    },
    applied: null,
    apply_digest: null,
    finalization: d,
    capture: null,
    retired: null,
  };
}
function encoded(value: unknown) {
  const body = JSON.stringify(value);
  return Buffer.from(
    JSON.stringify({ schema: 1, content_hash: sha256(body), id: id(2), body }),
  );
}
test("host envelope accepts production slot IDs in failed boot records", () => {
  const raw = {
    ...rawRecord(),
    state: { state: "failed", key: key() },
  };
  expect(
    parseHostRecord(encoded(raw), id(2)).record.request.cold.aux_ro_drives.map(
      (drive) => drive.drive_id,
    ),
  ).toEqual(["dyn_0", "dyn_1", "dyn_2"]);
});
test("host envelope rejects hyphenated slot IDs", () => {
  const raw = rawRecord();
  raw.request.cold.aux_ro_drives = aux.map((drive, i) => ({
    ...drive,
    drive_id: `dyn-${i}`,
  }));
  expect(() => parseHostRecord(encoded(raw), id(2))).toThrow();
});
test("host envelope rejects corruption, target substitution, environment and unknown fields", () => {
  expect(parseHostRecord(encoded(rawRecord()), id(2)).record.version).toBe(1);
  const corrupt = JSON.parse(encoded(rawRecord()).toString());
  corrupt.content_hash = "b".repeat(64);
  expect(() =>
    parseHostRecord(Buffer.from(JSON.stringify(corrupt)), id(2)),
  ).toThrow();
  expect(() => parseHostRecord(encoded(rawRecord()), id(9))).toThrow();
  const changed = rawRecord();
  changed.request.sandbox_spec_json = '{"env":{"TOKEN":"test"}}';
  expect(() => parseHostRecord(encoded(changed), id(2))).toThrow();
  expect(() =>
    parseHostRecord(encoded({ ...rawRecord(), pass: true }), id(2)),
  ).toThrow();
  expect(() => boundedHostJson(JSON.stringify([[[1]]]), 2)).toThrow();
  expect(() =>
    boundedHostJson("[".repeat(65) + "0" + "]".repeat(65), 1024),
  ).toThrow();
});
function captured() {
  const request = { key: key(), capture_id: id(6) };
  const authority = {
    version: 1,
    request,
    host_incarnation: "host-first",
    disk_manifest: { manifest_id: id(6), version: 1 },
    disk_sha256: h,
    disk_size_bytes: 4,
    image: "fixture-image",
    kernel_sha256: h,
    aux_ro_drives: aux,
    consistency: "crash_consistent_disk_cut_v1",
    source_state: "destroyed",
    record_digest: "",
  };
  authority.record_digest = `sha256:${sha256(JSON.stringify(authority))}`;
  return {
    ...rawRecord(),
    state: { state: "captured", authority },
    capture: {
      request,
      manifest: authority.disk_manifest,
      artifact: [h, 4],
      authority,
    },
  };
}
test("capture digest and cold destination bind exact source, artifact and epoch", () => {
  const raw = captured();
  const old = parseHostRecord(encoded(raw), id(2)).record;
  const next = structuredClone(old);
  next.request.key.context.target.sandbox_id = id(7);
  next.request.key.context.target.binding_epoch = 2;
  next.request.key.boot_attempt = "boot-second";
  next.request.cold.sandbox_id = id(7);
  next.request.cold.boot_attempt = "boot-second";
  next.request.capture_authority = old.capture!.authority;
  next.request.cold.disk = {
    manifest: { manifest_id: id(6), version: 1 },
    sha256: h,
    size_bytes: 4,
    source: {
      kind: "capture",
      capture_id: id(6),
      session_id: id(1),
      sandbox_id: id(2),
      binding_epoch: 1,
      record_digest: old.capture!.authority!.record_digest,
    },
  };
  expect(verifyColdChain(old, next).captureSha256).toBe(h);
  if (next.request.cold.disk.source.kind !== "capture")
    throw new Error("expected capture");
  next.request.cold.disk.source.record_digest = `sha256:${"b".repeat(64)}`;
  expect(() => verifyColdChain(old, next)).toThrow();
  raw.capture.authority.disk_size_bytes = 5;
  expect(() => parseHostRecord(encoded(raw), id(2))).toThrow();
});
test("actual backend chain rejects backend substitution and changed native extension fields", () => {
  const native = Buffer.from(
    JSON.stringify([
      {
        name: "native",
        description: "test",
        inputSchema: {},
        extension: "original",
      },
    ]),
  );
  const raw = rawRecord();
  raw.request.key.native_digest = `sha256:${sha256(native)}`;
  raw.state.intent.receipt.key = raw.request.key;
  const host = parseHostRecord(encoded(raw), id(2)).record;
  const assets = {
    request: {
      version: 1,
      host_id: id(3),
      session_id: id(1),
      intent_id: "boot-first",
      image: "fixture-image",
      image_digest: d,
      disk_manifest: manifest,
      aux_ro_drives: aux,
    },
    host_incarnation: "host-first",
    backend: "vz",
    kernel_sha256: h,
    disk_sha256: h,
    disk_size_bytes: 4,
  };
  const current = {
    phase: "started",
    host_id: id(3),
    binding_epoch: "1",
    boot_attempt: "boot-first",
    native_digest: raw.request.key.native_digest,
    source_key: host.request.key,
    combined_manifest_digest: `sha256:${sha256(native)}`,
    binding_hash: d,
  };
  const observed = {
    completed: true as const,
    native_version: 1 as const,
    native_bytes: native,
    native_digest: current.native_digest,
    assets: JSON.stringify(assets),
    start: JSON.stringify(raw.state.intent),
    combined_manifest: JSON.stringify([...native]),
    combined_manifest_digest: current.combined_manifest_digest,
  };
  const ctx = {
    backend: "vz" as const,
    hostId: id(3),
    imageDigest: d,
    artifacts: { kernel: { sha256: h } },
  };
  expect(verifyPhysicalBoot(ctx, host, current, observed).backend).toBe("vz");
  expect(() =>
    verifyPhysicalBoot(
      { ...ctx, backend: "firecracker" },
      host,
      current,
      observed,
    ),
  ).toThrow();
  const wrong = Buffer.from(native.toString().replace("original", "changed"));
  const wrongHash = `sha256:${sha256(wrong)}`;
  expect(() =>
    verifyPhysicalBoot(
      ctx,
      host,
      { ...current, combined_manifest_digest: wrongHash },
      {
        ...observed,
        combined_manifest: JSON.stringify([...wrong]),
        combined_manifest_digest: wrongHash,
      },
    ),
  ).toThrow();
});
test("proxy denial requires one complete exact daemon event", () => {
  const line = `2026-11-07T00:00:00.000000Z  INFO engram_egress_proxy::proxy: egress rejected — destination not in any allow_hosts session_id=${id(1)} sni=resource.fixture.example original_dst=Some((10.88.2.30, 443))`;
  const parse = (
    text: string,
    partial = false,
    session = id(1),
    sni = "resource.fixture.example",
  ) => parseProxyDenials(Buffer.from(text), partial, session, sni);
  expect(parse(line + "\n")).toHaveLength(1);
  expect(parse(line)).toHaveLength(0);
  expect(parse(line + "\n", true)).toHaveLength(0);
  expect(parse(line + "\n", false, id(9))).toHaveLength(0);
  expect(
    parse(line + "\n", false, id(1), "other.fixture.example"),
  ).toHaveLength(0);
  expect(parse(line.replace(" INFO ", " WARN ") + "\n")).toHaveLength(0);
  expect(
    parse(line.replace("engram_egress_proxy::proxy", "other::proxy") + "\n"),
  ).toHaveLength(0);
  expect(parse("prefix " + line + "\n")).toHaveLength(0);
  expect(parse(line + " session_id=" + id(1) + "\n")).toHaveLength(0);
  expect(parse(line + "\n" + line + "\n")).toHaveLength(0);
  expect(parse(line + "\n" + "prefix " + line + "\n")).toHaveLength(0);
  expect(
    parse(line.replace("sni=resource", "sni=bad\nsni=resource") + "\n"),
  ).toHaveLength(0);
  expect(() => parse("x".repeat(65537))).toThrow();
});
test("worker TLS requires a verified server and certificate-required alert", async () => {
  const root = await realpath(
    await mkdtemp(join(tmpdir(), "worker-tls-test-")),
  );
  const caPath = join(root, "ca.pem");
  await writeFile(caPath, TEST_CA, { mode: 0o600 });
  const server = createServer({
    key: TEST_KEY,
    cert: TEST_CERT,
    ca: TEST_CA,
    requestCert: true,
    rejectUnauthorized: true,
    minVersion: "TLSv1.3",
    maxVersion: "TLSv1.3",
  });
  await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
  const port = (server.address() as { port: number }).port;
  const endpoint = {
    origin: `https://localhost:${port}`,
    ca: { path: caPath, sha256: sha256(Buffer.from(TEST_CA)) },
    serverFingerprint: new X509Certificate(TEST_CERT).fingerprint256,
  };
  const ctx = {
    runRoot: root,
    deadlines: { requestMs: 6000 },
    endpoints: { workerControl: endpoint },
  } as Context;
  try {
    expect((await probeWorkerControl(ctx)).classification).toBe(
      "mtls_authentication_denial",
    );
    endpoint.serverFingerprint = Array(32).fill("AA").join(":");
    await expect(probeWorkerControl(ctx)).rejects.toThrow();
    endpoint.serverFingerprint = new X509Certificate(TEST_CERT).fingerprint256;
    await new Promise<void>((resolve) => server.close(() => resolve()));
    const permissive = createServer(
      {
        key: TEST_KEY,
        cert: TEST_CERT,
        minVersion: "TLSv1.3",
        maxVersion: "TLSv1.3",
      },
      (socket) => socket.destroy(),
    );
    await new Promise<void>((resolve) =>
      permissive.listen(port, "127.0.0.1", resolve),
    );
    try {
      await expect(probeWorkerControl(ctx)).rejects.toThrow();
    } finally {
      await new Promise<void>((resolve) => permissive.close(() => resolve()));
    }
  } finally {
    if (server.listening)
      await new Promise<void>((resolve) => server.close(() => resolve()));
    await rm(root, { recursive: true, force: true });
  }
});
