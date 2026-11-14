import { connect } from "node:tls";
import { z } from "zod";
import {
  requireThat,
  privateFile,
  sha256,
  verifyArtifact,
  RunnerFailure,
  type Context,
} from "./context.ts";
import { guestExec, guestHarnessPath, type GuestExecClient } from "./guest.ts";
import { tlsOptions } from "./transport.ts";

/** Verify the real worker rejects a TLS client without a certificate. */
export async function probeWorkerControl(ctx: Context) {
  const endpoint = ctx.endpoints.workerControl;
  await verifyArtifact(ctx.runRoot, endpoint.ca);
  const options = await tlsOptions(ctx, endpoint);
  const url = new URL(endpoint.origin);
  return await new Promise<{
    classification: "mtls_authentication_denial";
    alert: 116;
    leafSha256: string;
  }>((resolve, reject) => {
    const socket = connect({
      ...options,
      host: url.hostname,
      port: Number(url.port || "443"),
      servername: url.hostname,
      minVersion: "TLSv1.3",
      maxVersion: "TLSv1.3",
    });
    let leafSha256 = "";
    let settled = false;
    const finish = (ok: boolean) => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      socket.destroy();
      if (ok)
        resolve({
          classification: "mtls_authentication_denial",
          alert: 116,
          leafSha256,
        });
      else
        reject(
          new RunnerFailure(
            "ASSERTION_FAILED",
            "worker_tls_authentication_denial",
          ),
        );
    };
    const timer = setTimeout(() => finish(false), ctx.deadlines.requestMs);
    socket.on("secureConnect", () => {
      const leaf = socket.getPeerCertificate().raw;
      if (socket.authorized && socket.getProtocol() === "TLSv1.3" && leaf)
        leafSha256 = sha256(leaf);
    });
    socket.on("error", (error: NodeJS.ErrnoException) => {
      finish(
        !!leafSha256 &&
          (error.code === "ERR_SSL_TLSV13_ALERT_CERTIFICATE_REQUIRED" ||
            error.code === "ERR_SSL_TLSV1_ALERT_CERTIFICATE_REQUIRED"),
      );
    });
    socket.on("data", () => finish(false));
    socket.on("close", () => finish(false));
  });
}

export async function probeManagedAuthority(
  ctx: Context,
  client: GuestExecClient,
  sessionId: string,
) {
  const fixture = ctx.protocolFixtures.modern;
  const url = new URL(fixture.resourceOrigin);
  const bytes = await privateFile(fixture.runRoot, fixture.configuration.path);
  requireThat(
    sha256(bytes) === fixture.configuration.sha256,
    "managed_fixture_config_digest",
  );
  const config = z
    .object({
      listen: z.object({
        host: z.ipv4(),
        port: z.number().int().min(1).max(65535),
      }),
      advertised: z.object({ resource: z.string() }),
    })
    .parse(JSON.parse(bytes.toString("utf8")));
  requireThat(
    config.advertised.resource === fixture.resourceOrigin &&
      config.listen.port === Number(url.port || "443"),
    "managed_fixture_config_binding",
  );
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
    // Port 443 follows the enforced proxy path; denial happens before upstream dial.
    `${config.listen.host}:443`,
    url.hostname,
  ]);
  requireThat(
    response.status !== 0 && ![124, 126, 127, 137].includes(response.status),
    "managed_destination_probe_denied",
  );
  return { sni: url.hostname, sha256: sha256(String(response.status)) };
}

/** Fixed sentinels test loss of the old tmpfs state, not removal of every old RAM byte. */
export async function guestColdObservation(
  ctx: Context,
  client: GuestExecClient,
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
