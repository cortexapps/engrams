import { X509Certificate } from "node:crypto";
import { isIP } from "node:net";
import { lstatSync, readFileSync, realpathSync } from "node:fs";
import { isAbsolute, relative, resolve } from "node:path";
import type { FixtureOptions } from "../../../mcp-worker/test/fixtures/server.ts";

export function object(
  value: unknown,
): asserts value is Record<string, unknown> {
  if (typeof value !== "object" || value === null || Array.isArray(value))
    throw new Error("Expected object");
}
export function keys(
  value: Record<string, unknown>,
  allowed: string[],
  required = allowed,
) {
  if (
    Object.keys(value).some((k) => !allowed.includes(k)) ||
    required.some((k) => !(k in value))
  )
    throw new Error("Unknown or missing field");
}
function text(value: unknown): string {
  if (typeof value !== "string" || value.length === 0 || value.length > 4096)
    throw new Error("Invalid string");
  return value;
}
function port(value: unknown): number {
  if (
    typeof value !== "number" ||
    !Number.isInteger(value) ||
    value < 1 ||
    value > 65535
  )
    throw new Error("Invalid port");
  return value;
}
export function caseOptions(value: unknown): Pick<
  FixtureOptions,
  "reply" | "response" | "holdCalls" | "requireAuth"
> & {
  revision?: number;
} {
  object(value);
  keys(
    value,
    ["reply", "response", "holdCalls", "requireAuth", "revision"],
    [],
  );
  if (
    value.reply !== undefined &&
    (typeof value.reply !== "string" || !["json", "sse"].includes(value.reply))
  )
    throw new Error("Invalid reply");
  if (
    value.response !== undefined &&
    (typeof value.response !== "string" ||
      ![
        "normal",
        "malformed",
        "oversize",
        "binary",
        "image",
        "audio",
        "resource",
        "resource_link",
        "error",
      ].includes(value.response))
  )
    throw new Error("Invalid response");
  for (const k of ["holdCalls", "requireAuth"])
    if (value[k] !== undefined && typeof value[k] !== "boolean")
      throw new Error("Invalid boolean");
  if (
    value.revision !== undefined &&
    (typeof value.revision !== "number" ||
      !Number.isSafeInteger(value.revision) ||
      value.revision < 1 ||
      value.revision > 100)
  )
    throw new Error("Invalid revision");
  // These fields have each been checked above; no unchecked input crosses this boundary.
  return value as ReturnType<typeof caseOptions>;
}
export function loadConfig(path: string) {
  const file = (path: string, max: number) => {
    if (
      !isAbsolute(path) ||
      realpathSync(path) !== path ||
      !lstatSync(path).isFile() ||
      lstatSync(path).size > max
    )
      throw new Error("Invalid file");
    return readFileSync(path, "utf8");
  };
  const raw: unknown = JSON.parse(file(path, 16384));
  object(raw);
  keys(raw, [
    "version",
    "runRoot",
    "listen",
    "advertised",
    "tls",
    "control",
    "protocol",
    "case",
    "apiKeyFile",
  ]);
  if (raw.version !== 1) throw new Error("Invalid config version");
  const root = text(raw.runRoot);
  if (
    !isAbsolute(root) ||
    resolve(root) !== root ||
    realpathSync(root) !== root ||
    !lstatSync(root).isDirectory() ||
    (lstatSync(root).mode & 0o077) !== 0
  )
    throw new Error("Invalid run root");
  const ownedFile = (value: unknown, max: number) => {
    const p = text(value);
    const rel = relative(root, p);
    if (!rel || rel.startsWith("..") || isAbsolute(rel))
      throw new Error("File outside run root");
    return file(p, max);
  };
  const listen = raw.listen;
  object(listen);
  keys(listen, ["host", "port"]);
  const host = text(listen.host);
  if (!isIP(host)) throw new Error("Listener must be an exact IP");
  const dataPort = port(listen.port);
  const advertised = raw.advertised;
  object(advertised);
  keys(advertised, [
    "resource",
    "issuer",
    "authorization",
    "token",
    "redirectUri",
    "authorities",
  ]);
  const origin = (v: unknown, expectedPort?: number) => {
    const s = text(v);
    const u = new URL(s);
    if (
      u.protocol !== "https:" ||
      u.origin !== s ||
      u.username ||
      u.password ||
      isIP(u.hostname) ||
      u.hostname.length > 253 ||
      u.hostname
        .split(".")
        .some(
          (label) => !/^[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?$/.test(label),
        ) ||
      (expectedPort !== undefined && Number(u.port || 443) !== expectedPort)
    )
      throw new Error("Invalid advertised origin");
    return s;
  };
  const resource = origin(advertised.resource, dataPort);
  const redirectUri = text(advertised.redirectUri);
  const redirect = new URL(redirectUri);
  origin(redirect.origin);
  if (
    redirect.href !== redirectUri ||
    redirectUri.includes("?") ||
    redirectUri.includes("#") ||
    redirect.username ||
    redirect.password ||
    redirect.search ||
    redirect.hash
  )
    throw new Error("Invalid redirect URI");
  const oauth = {
    issuer: origin(advertised.issuer, dataPort),
    authorization: origin(advertised.authorization, dataPort),
    token: origin(advertised.token, dataPort),
    redirectUri,
  };
  const authorities = [
    ...new Set(
      [resource, oauth.issuer, oauth.authorization, oauth.token].map(
        (s) => new URL(s).host,
      ),
    ),
  ].sort();
  if (
    !Array.isArray(advertised.authorities) ||
    JSON.stringify([...advertised.authorities].sort()) !==
      JSON.stringify(authorities)
  )
    throw new Error("Authority mismatch");
  const tls = raw.tls;
  object(tls);
  keys(tls, ["certificateFile", "keyFile", "caFile"]);
  const cert = ownedFile(tls.certificateFile, 16384);
  const key = ownedFile(tls.keyFile, 16384);
  const ca = ownedFile(tls.caFile, 16384);
  const leaf = new X509Certificate(cert);
  const trust = new X509Certificate(ca);
  if (!leaf.verify(trust.publicKey))
    throw new Error("Certificate trust mismatch");
  for (const authority of authorities)
    if (!leaf.checkHost(new URL(`https://${authority}`).hostname))
      throw new Error("Certificate name mismatch");
  const control = raw.control;
  object(control);
  keys(
    control,
    ["port", "origin", "tokenFile", "host"],
    ["port", "origin", "tokenFile"],
  );
  const controlHost =
    control.host === undefined ? "127.0.0.1" : text(control.host);
  if (!isIP(controlHost)) throw new Error("Invalid control bind");
  const controlPort = port(control.port);
  if (controlPort === dataPort) throw new Error("Listener collision");
  const controlOrigin = origin(control.origin, controlPort);
  if (!leaf.checkHost(new URL(controlOrigin).hostname))
    throw new Error("Control certificate name mismatch");
  const token = ownedFile(control.tokenFile, 256).trim();
  const apiKey = ownedFile(raw.apiKeyFile, 256).trim();
  if (
    !/^[A-Za-z0-9_-]{32,128}$/.test(token) ||
    !/^[A-Za-z0-9_-]{16,128}$/.test(apiKey) ||
    token === apiKey
  )
    throw new Error("Invalid neutral credential");
  if (
    typeof raw.protocol !== "string" ||
    !["2026-07-28", "2025-11-25", "2025-06-18"].includes(raw.protocol)
  )
    throw new Error("Invalid protocol");
  const protocol = raw.protocol as NonNullable<FixtureOptions["version"]>;
  return {
    root,
    token,
    control: { host: controlHost, port: controlPort, origin: controlOrigin },
    options: {
      ...caseOptions(raw.case),
      version: protocol,
      apiKey,
      network: {
        host,
        port: dataPort,
        key,
        cert,
        ca,
        origin: resource,
        oauth,
        allowedAuthorities: authorities,
      },
    },
  };
}
