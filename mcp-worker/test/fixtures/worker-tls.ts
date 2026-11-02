import { mkdtemp, readFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { execFileSync } from "node:child_process";
import { X509Certificate } from "node:crypto";
import { request } from "node:https";
export async function workerTls(hostname = "tools.example") {
  const dir = await mkdtemp(join(tmpdir(), "mcp-worker-tls-"));
  try {
    execFileSync(
      "openssl",
      [
        "req",
        "-x509",
        "-newkey",
        "rsa:2048",
        "-nodes",
        "-keyout",
        join(dir, "key.pem"),
        "-out",
        join(dir, "cert.pem"),
        "-days",
        "1",
        "-subj",
        `/CN=${hostname}`,
        "-addext",
        `subjectAltName=DNS:${hostname}${hostname === "localhost" ? ",IP:127.0.0.1" : ""}`,
        "-addext",
        "extendedKeyUsage=serverAuth,clientAuth",
      ],
      { stdio: "ignore" },
    );
    const key = await readFile(join(dir, "key.pem"), "utf8"),
      cert = await readFile(join(dir, "cert.pem"), "utf8");
    return {
      key,
      cert,
      ca: cert,
      fingerprint: new X509Certificate(cert).fingerprint256,
    };
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
}
export async function localRpc(
  port: number,
  tls: Awaited<ReturnType<typeof workerTls>>,
  path: string,
  body?: unknown,
  withCert = true,
) {
  return new Promise<{ status: number; body: unknown }>((resolve, reject) => {
    const req = request(
      {
        hostname: "127.0.0.1",
        port,
        servername: "tools.example",
        path,
        method: body === undefined ? "GET" : "POST",
        ca: tls.ca,
        ...(withCert ? { key: tls.key, cert: tls.cert } : {}),
        agent: false,
      },
      (res) => {
        let raw = "";
        res.on("data", (chunk) => {
          raw += String(chunk);
        });
        res.on("end", () =>
          resolve({ status: res.statusCode ?? 0, body: JSON.parse(raw) }),
        );
      },
    );
    req.on("error", reject);
    req.end(body === undefined ? undefined : JSON.stringify(body));
  });
}
