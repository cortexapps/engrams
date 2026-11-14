import { createClient, createRouterTransport } from "@connectrpc/connect";
import { expect, test } from "bun:test";
import { createSecureServer } from "node:http2";
import { execFileSync } from "node:child_process";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { X509Certificate } from "node:crypto";
import { connectNodeAdapter } from "@connectrpc/connect-node";
import { CredentialControlService } from "../gen/engram/mcp/v1/control_pb.ts";
import {
  makeCredentialControlClient,
  unwrapCredentialControlClient,
  makeSessionPolicyControlClient,
} from "../mcp/control-client.ts";

import { SessionPolicyService } from "../gen/engram/mcp/v1/session_policy_pb.ts";

function certificates() {
  const dir = mkdtempSync(join(tmpdir(), "mcp-control-test-"));
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
        join(dir, "key"),
        "-out",
        join(dir, "cert"),
        "-days",
        "1",
        "-subj",
        "/CN=localhost",
        "-addext",
        "subjectAltName=DNS:localhost,IP:127.0.0.1",
        "-addext",
        "extendedKeyUsage=serverAuth,clientAuth",
      ],
      { stdio: "ignore" },
    );
    const cert = readFileSync(join(dir, "cert"), "utf8");
    return {
      cert,
      key: readFileSync(join(dir, "key"), "utf8"),
      ca: cert,
      serverFingerprint: new X509Certificate(cert).fingerprint256,
    };
  } finally {
    rmSync(dir, { recursive: true });
  }
}
test("control client requires pinned mTLS and preserves int64 metadata", async () => {
  const tls = certificates();
  let calls = 0;
  const server = createSecureServer(
    { key: tls.key, cert: tls.cert, ca: tls.ca, requestCert: true, rejectUnauthorized: true },
    connectNodeAdapter({
      routes(router) {
        router.service(SessionPolicyService, {
          async readWritePolicy(req) {
            return {
              state: {
                policy: {
                  ref: {
                    sessionId: req.sessionId,
                    policyRevision: 9007199254740993n,
                    bindingEpoch: 9007199254740995n,
                  },
                  writeEligibility: "read_only",
                },
              },
            };
          },
        });
        router.service(CredentialControlService, {
          async getStatus(req) {
            calls++;
            expect(req.protocol).toBe(1);
            return { status: { protocol: 1, version: 9007199254740993n, status: "connected" } };
          },
        });
      },
    }),
  );
  const sessions = new Set<import("node:http2").ServerHttp2Session>();
  server.on("session", (s) => {
    sessions.add(s);
    s.on("close", () => sessions.delete(s));
  });
  await new Promise<void>((r) => server.listen(0, "127.0.0.1", r));
  const address = server.address();
  if (!address || typeof address === "string") throw new Error("address");
  const url = `https://127.0.0.1:${address.port}`;
  try {
    const good = makeCredentialControlClient({ url, ...tls });
    expect((await good.getStatus({ protocol: 1, connectionId: "c" })).version).toBe(
      9007199254740993n,
    );
    const policy = makeSessionPolicyControlClient({ url, ...tls });
    expect(
      (await policy.readWritePolicy({ protocol: 1, sessionId: "s" })).state?.policy?.ref
        ?.policyRevision,
    ).toBe(9007199254740993n);
    const badPolicy = makeSessionPolicyControlClient({
      url,
      ...tls,
      serverFingerprint: "00:".repeat(31) + "00",
    });
    await expect(badPolicy.readWritePolicy({ protocol: 1 })).rejects.toThrow("control unavailable");
    const bad = makeCredentialControlClient({
      url,
      ...tls,
      serverFingerprint: "00:".repeat(31) + "00",
    });
    await expect(bad.getStatus({ protocol: 1 })).rejects.toThrow("credential control unavailable");
    const other = certificates();
    const untrusted = makeCredentialControlClient({
      url,
      ...tls,
      key: other.key,
      cert: other.cert,
    });
    await expect(untrusted.getStatus({ protocol: 1 })).rejects.toThrow(
      "credential control unavailable",
    );
    expect(calls).toBe(1);
    expect(() =>
      makeCredentialControlClient({ url: url.replace("https:", "http:"), ...tls }),
    ).toThrow();
    expect(() => makeCredentialControlClient({ url, ...tls, key: "" })).toThrow();
  } finally {
    for (const s of sessions) s.destroy();
    await new Promise<void>((r) => server.close(() => r()));
  }
});

test("credential response wrappers fail closed when status is absent", async () => {
  const client = unwrapCredentialControlClient(
    createClient(
      CredentialControlService,
      createRouterTransport((router) =>
        router.service(CredentialControlService, {
          async putSecret() {
            return {};
          },
          async completeAuthorization() {
            return {};
          },
          async getStatus() {
            return {};
          },
          async revoke() {
            return {};
          },
        }),
      ),
    ),
  );
  await expect(client.putSecret({})).rejects.toThrow("status unavailable");
  await expect(client.completeAuthorization({})).rejects.toThrow("status unavailable");
  await expect(client.getStatus({})).rejects.toThrow("status unavailable");
  await expect(client.revoke({})).rejects.toThrow("status unavailable");
});
