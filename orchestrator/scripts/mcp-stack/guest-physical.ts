import { X509Certificate } from "node:crypto";
import { readFile } from "node:fs/promises";
import { z } from "zod";
import {
  requireThat,
  sha256,
  verifyArtifact,
  type Context,
} from "./context.ts";
import { guestExec, guestHarnessPath } from "./guest.ts";
import type { PublicClient } from "./transport.ts";

/** Parse actual OpenSSL output. A timeout, close or generic TLS failure is not a denial receipt. */
export function parseWorkerTlsDenial(
  output: Buffer,
  status: number,
  fingerprint: string,
) {
  requireThat(
    output.length <= 65536 &&
      status > 0 &&
      ![124, 126, 127, 137].includes(status),
    "worker_tls_actual_failure",
  );
  const text = output.toString("utf8");
  const first = text.match(
    /-----BEGIN CERTIFICATE-----\r?\n[A-Za-z0-9+/=\r\n]+-----END CERTIFICATE-----/,
  );
  requireThat(
    first &&
      /^OpenSSL 3\./m.test(text) &&
      /New, TLSv1\.3, Cipher is \S+/.test(text) &&
      /Verify return code: 0 \(ok\)/.test(text) &&
      /tlsv13 alert certificate required/.test(text) &&
      /SSL alert number 116\b/.test(text),
    "worker_tls_certificate_required",
  );
  const certificate = new X509Certificate(first[0]);
  requireThat(
    certificate.fingerprint256 === fingerprint,
    "worker_tls_actual_leaf_pin",
  );
  return {
    classification: "mtls_authentication_denial" as const,
    alert: 116 as const,
    transcriptSha256: sha256(output),
    leafSha256: sha256(certificate.raw),
  };
}
export async function probeWorkerControl(
  ctx: Context,
  client: PublicClient,
  sessionId: string,
) {
  requireThat(ctx.deadlines.requestMs >= 6000, "worker_tls_request_deadline");
  const endpoint = ctx.endpoints.workerControl;
  await verifyArtifact(ctx.runRoot, endpoint.ca);
  const ca = await readFile(endpoint.ca.path);
  requireThat(
    ca.length <= 65536 &&
      sha256(ca) === endpoint.ca.sha256 &&
      !ca.toString("utf8").includes("PRIVATE KEY"),
    "worker_public_ca_only",
  );
  const url = new URL(endpoint.origin);
  const result = await guestExec(ctx, client, sessionId, [
    "/usr/bin/env",
    "-i",
    "PATH=/usr/bin:/bin",
    "/bin/sh",
    "-c",
    `set -eu
command -v openssl >/dev/null
command -v timeout >/dev/null
openssl version
cd /tmp/mcp-stack-fixture
[ ! -L worker-control-ca.pem ] && [ ! -e worker-control-ca.pem ]
umask 077
set -C
printf '%s' "$1" | base64 -d > worker-control-ca.pem
set +e
timeout 5s openssl s_client -connect "$2" -servername "$3" -verify_hostname "$3" -verify_return_error -CAfile worker-control-ca.pem -tls1_3 -showcerts -ign_eof < /dev/null 2>&1
status=$?
rm worker-control-ca.pem
exit "$status"
`,
    "fixture-worker-tls",
    ca.toString("base64"),
    `${url.hostname}:${url.port || "443"}`,
    url.hostname,
  ]);
  return parseWorkerTlsDenial(
    result.stdout,
    result.status,
    endpoint.serverFingerprint,
  );
}

export async function probeManagedAuthority(
  ctx: Context,
  client: PublicClient,
  sessionId: string,
) {
  const url = new URL(ctx.protocolFixtures.modern.resourceOrigin);
  const response = await guestExec(ctx, client, sessionId, [
    "/usr/bin/env",
    "-i",
    "PATH=/usr/bin:/bin",
    "/bin/sh",
    "-c",
    `command -v openssl >/dev/null && command -v timeout >/dev/null || exit 127
timeout 5s openssl s_client -connect "$1" -servername "$2" -tls1_3 < /dev/null >/dev/null 2>&1
`,
    "fixture-managed-destination",
    `${url.hostname}:${url.port || "443"}`,
    url.hostname,
  ]);
  requireThat(
    response.status !== 0 && response.status !== 127,
    "managed_destination_probe_denied",
  );
  return { sni: url.hostname, sha256: sha256(String(response.status)) };
}

/** Fixed sentinels test loss of the old tmpfs state, not removal of every old RAM byte. */
export async function guestColdObservation(
  ctx: Context,
  client: PublicClient,
  sessionId: string,
  prepare: boolean,
) {
  const sentinel = sha256(`${ctx.runId}:${sessionId}:cold-sentinel`);
  const script = `set -eu
cd /tmp/mcp-stack-fixture
awk '$2 == "/dev/shm" && $3 == "tmpfs" {found=1} END {exit !found}' /proc/mounts
if [ "$1" = prepare ]; then
  [ ! -e /dev/shm/mcp-stack-sentinel ] && [ ! -L /dev/shm/mcp-stack-sentinel ]
  [ ! -e cold-disk-sentinel ] && [ ! -L cold-disk-sentinel ]
  umask 077
  set -C
  printf '%s' "$2" > /dev/shm/mcp-stack-sentinel
  printf '%s' "$2" > cold-disk-sentinel
  sync -f cold-disk-sentinel
else
  [ ! -e /dev/shm/mcp-stack-sentinel ] && [ ! -L /dev/shm/mcp-stack-sentinel ]
fi
[ ! -L cold-disk-sentinel ] && [ "$(cat cold-disk-sentinel)" = "$2" ]
boot=$(cat /proc/sys/kernel/random/boot_id)
count=0
manifest_hash=''
for path in /proc/[0-9]*/cmdline; do
  [ -r "$path" ] || continue
  first=$(tr '\\000' '\\n' < "$path" | head -n 1)
  [ "$first" = "$3" ] || continue
  second=$(tr '\\000' '\\n' < "$path" | sed -n '2p')
  case "$second" in --connect|--vsock-host) ;; *) continue;; esac
  count=$((count+1))
  [ "$count" -le 1 ] || exit 12
  env_path=$(dirname "$path")/environ
  values=$(head -c 1048577 "$env_path" | tr '\\000' '\\n')
  [ "$(printf '%s' "$values" | wc -c)" -le 1048576 ] || exit 13
  manifest=$(printf '%s\\n' "$values" | sed -n 's/^ENGRAM_TOOLS=//p')
  [ -n "$manifest" ] || exit 14
  manifest_hash=$(printf '%s' "$manifest" | sha256sum | cut -d' ' -f1)
done
[ "$count" -eq 1 ]
printf '{"bootId":"%s","manifestSha256":"%s","tmpfsVerified":true,"diskRetained":true}\\n' "$boot" "$manifest_hash"
`;
  const response = await guestExec(ctx, client, sessionId, [
    "/bin/sh",
    "-c",
    script,
    "fixture-cold-observe",
    prepare ? "prepare" : "verify",
    sentinel,
    guestHarnessPath(ctx),
  ]);
  requireThat(response.status === 0, "guest_cold_actual_observation");
  const observed = z
    .strictObject({
      bootId: z.uuid(),
      manifestSha256: z.string().regex(/^[a-f0-9]{64}$/),
      tmpfsVerified: z.literal(true),
      diskRetained: z.literal(true),
    })
    .parse(JSON.parse(response.stdout.toString("utf8")));
  return { ...observed, sha256: sha256(response.stdout) };
}
