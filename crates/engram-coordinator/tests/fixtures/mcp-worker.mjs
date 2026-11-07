// Actual worker service, with only fixture DNS resolution replaced.
import { createWorker } from "../../../../mcp-worker/src/service.ts";
import { ZonePolicy } from "../../../../mcp-worker/src/network.ts";
import { mkdtempSync, readFileSync, writeFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { execFileSync } from "node:child_process";
import { X509Certificate } from "node:crypto";
const dir = mkdtempSync(join(tmpdir(), "mcp-coordinator-test-"));
const run = (args) => execFileSync("openssl", args, { stdio: "ignore" });
run([
  "req",
  "-x509",
  "-newkey",
  "rsa:2048",
  "-nodes",
  "-keyout",
  join(dir, "ca-key"),
  "-out",
  join(dir, "ca"),
  "-days",
  "1",
  "-subj",
  "/CN=fixture-ca",
  "-addext",
  "basicConstraints=critical,CA:TRUE",
]);
run([
  "req",
  "-new",
  "-newkey",
  "rsa:2048",
  "-nodes",
  "-keyout",
  join(dir, "key"),
  "-out",
  join(dir, "csr"),
  "-subj",
  "/CN=localhost",
]);
writeFileSync(
  join(dir, "ext"),
  "basicConstraints=critical,CA:FALSE\nsubjectAltName=DNS:localhost\nextendedKeyUsage=serverAuth,clientAuth\n",
);
run([
  "x509",
  "-req",
  "-in",
  join(dir, "csr"),
  "-CA",
  join(dir, "ca"),
  "-CAkey",
  join(dir, "ca-key"),
  "-CAcreateserial",
  "-out",
  join(dir, "cert"),
  "-days",
  "1",
  "-extfile",
  join(dir, "ext"),
]);
const key = readFileSync(join(dir, "key"), "utf8"),
  cert = readFileSync(join(dir, "cert"), "utf8"),
  ca = readFileSync(join(dir, "ca"), "utf8");
rmSync(dir, { recursive: true });
const zone = new ZonePolicy({
  id: "public",
  revision: "1",
  mode: "public",
  authorities: ["auth.example"],
  aliases: [],
  cidrs: [],
  denyCidrs: [],
});
const worker = createWorker({
  zone,
  tls: { key, cert, ca },
  callers: [
    {
      fingerprint: new X509Certificate(cert).fingerprint256,
      role: "coordinator",
    },
  ],
  enabled: true,
  network: {
    resolve: async () => ["8.8.8.8"],
    request: () => {
      throw Error("no_provider_dial");
    },
  },
});
const port = await worker.listen(0, "127.0.0.1");
console.log(
  JSON.stringify({
    port,
    key,
    cert,
    ca,
    fingerprint: new X509Certificate(cert).fingerprint256,
    identity: worker.identity,
  }),
);
process.stdin.resume();
process.stdin.on("end", async () => {
  await worker.close();
  process.exit(0);
});
