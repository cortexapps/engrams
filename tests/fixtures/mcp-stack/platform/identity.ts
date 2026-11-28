import {
  X509Certificate,
  createPrivateKey,
  createPublicKey,
  randomBytes,
  randomUUID,
} from "node:crypto";
import { join } from "node:path";
import {
  check,
  childEnvironment,
  command,
  hash,
  newRoot,
  privateRoot,
  put,
  readOwned,
  verifyPin,
  type Pin,
} from "./owned.ts";
import type { Inputs } from "./preflight.ts";

export interface Leaf {
  certificate: Pin;
  key: Pin;
  fingerprint: string;
  hostPin: string;
}
export interface Identity {
  runId: string;
  hostId: string;
  ca: Pin;
  caKey: Pin;
  leaves: Record<
    | "orchestrator"
    | "coordinator"
    | "operator"
    | "host"
    | "worker"
    | "app"
    | "modern"
    | "legacy",
    Leaf
  >;
  secrets: Record<
    | "supervisor"
    | "kek"
    | "postgres"
    | "coordinator"
    | "appGrpc"
    | "auth"
    | "broker"
    | "router"
    | "bootstrapPassword"
    | "adminPassword"
    | "ownerPassword"
    | "otherPassword",
    Pin
  >;
}
export async function generateIdentity(
  inputs: Inputs,
  root: string,
): Promise<Identity> {
  await privateRoot(root);
  await verifyPin(inputs.binaries.openssl);
  check(
    (
      await command(
        inputs.binaries.openssl.path,
        ["version"],
        root,
        childEnvironment(root),
      )
    )
      .toString()
      .startsWith("OpenSSL 3."),
    "openssl3",
  );
  const tls = join(root, "tls"),
    secretsRoot = join(root, "secrets");
  await newRoot(tls);
  await newRoot(secretsRoot);
  const openssl = (args: string[]) =>
    command(
      inputs.binaries.openssl.path,
      args,
      tls,
      childEnvironment(root),
      undefined,
      10000,
    );
  const caPath = join(tls, "ca.pem"),
    caKeyPath = join(tls, "ca.key");
  await put(caPath, "");
  await put(caKeyPath, "");
  await openssl([
    "req",
    "-x509",
    "-newkey",
    "ec",
    "-pkeyopt",
    "ec_paramgen_curve:P-256",
    "-nodes",
    "-days",
    "2",
    "-subj",
    "/CN=MCP isolated fixture root",
    "-keyout",
    caKeyPath,
    "-out",
    caPath,
    "-addext",
    "basicConstraints=critical,CA:TRUE",
    "-addext",
    "keyUsage=critical,keyCertSign,cRLSign",
  ]);
  const pin = async (path: string): Promise<Pin> => ({
    path,
    sha256: hash(await readOwned(path)),
  });
  const ca = await pin(caPath),
    caKey = await pin(caKeyPath);
  const caCert = new X509Certificate(await readOwned(ca.path));
  const leafNames = [
    "orchestrator",
    "coordinator",
    "operator",
    "host",
    "worker",
    "app",
    "modern",
    "legacy",
  ] as const;
  const leaves: Partial<Identity["leaves"]> = {};
  for (const role of leafNames) {
    const names =
      role === "modern" || role === "legacy"
        ? ["resource", "issuer", "authorization", "token", "observer"].map(
            (name) => `${name}.${role}.fixture.example`,
          )
        : role === "coordinator"
          ? [
              "coordinator.fixture.example",
              "control.fixture.example",
              "operator.fixture.example",
            ]
          : [`${role}.fixture.example`];
    const keyPath = join(tls, `${role}.key`),
      certPath = join(tls, `${role}.pem`),
      csrPath = join(tls, `${role}.csr`);
    const extension = await put(
      join(tls, `${role}.ext`),
      `basicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature\nextendedKeyUsage=serverAuth,clientAuth\nsubjectAltName=${names.map((name) => `DNS:${name}`).join(",")}\n`,
    );
    for (const path of [keyPath, certPath, csrPath]) await put(path, "");
    await openssl([
      "req",
      "-new",
      "-newkey",
      "ec",
      "-pkeyopt",
      "ec_paramgen_curve:P-256",
      "-nodes",
      "-subj",
      `/CN=${names[0]}`,
      "-keyout",
      keyPath,
      "-out",
      csrPath,
    ]);
    await openssl([
      "x509",
      "-req",
      "-in",
      csrPath,
      "-CA",
      ca.path,
      "-CAkey",
      caKey.path,
      "-set_serial",
      `0x${randomBytes(16).toString("hex")}`,
      "-days",
      "1",
      "-extfile",
      extension.path,
      "-out",
      certPath,
    ]);
    const certificate = await pin(certPath),
      key = await pin(keyPath);
    const parsed = new X509Certificate(await readOwned(certPath));
    check(
      parsed.verify(caCert.publicKey) &&
        names.every((name) => parsed.checkHost(name) === name),
      "leaf_identity",
    );
    const publicKey = createPublicKey(
      createPrivateKey(await readOwned(keyPath)),
    ).export({ type: "spki", format: "der" });
    check(
      publicKey.equals(
        parsed.publicKey.export({ type: "spki", format: "der" }),
      ),
      "leaf_key_pair",
    );
    leaves[role] = {
      certificate,
      key,
      fingerprint: parsed.fingerprint256,
      hostPin: hash(parsed.raw),
    };
  }
  check(
    new Set(Object.values(leaves).map((v) => v.fingerprint)).size ===
      leafNames.length,
    "distinct_leaves",
  );
  const secretNames = [
    "supervisor",
    "kek",
    "postgres",
    "coordinator",
    "appGrpc",
    "auth",
    "broker",
    "router",
    "bootstrapPassword",
    "adminPassword",
    "ownerPassword",
    "otherPassword",
  ] as const;
  const secrets: Partial<Identity["secrets"]> = {};
  for (const name of secretNames) {
    const bytes = randomBytes(32);
    const value =
      name === "kek"
        ? bytes.toString("base64")
        : (name === "router" ? "fixture-neutral-" : "") +
          bytes.toString("base64url");
    secrets[name] = await put(join(secretsRoot, name), value);
  }
  // Every named entry was generated by the fixed exhaustive lists above.
  return {
    runId: `mcp-${randomBytes(12).toString("hex")}`,
    hostId: randomUUID(),
    ca,
    caKey,
    leaves: leaves as Identity["leaves"],
    secrets: secrets as Identity["secrets"],
  };
}
