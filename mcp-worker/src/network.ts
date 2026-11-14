import { resolve4, resolve6 } from "node:dns/promises";
import { request as httpsRequest } from "node:https";
import type { RequestOptions } from "node:https";
import type { ClientRequest } from "node:http";
import type { Transform } from "node:stream";
import { createGunzip, createInflate, createBrotliDecompress } from "node:zlib";
import ipaddr from "ipaddr.js";
import { digest } from "./encoding.ts";
import type { ZoneConfig } from "./contracts.ts";

export const PERMANENT_DENY_CIDRS = [
  "168.63.129.16/32",
  "fd00:ec2::254/128",
  "fd20:ce::254/128",
  "3fff::/20",
];
export const RESPONSE_BYTES = 2 * 1024 * 1024;
const allowedHeaders = new Set([
  "accept",
  "content-type",
  "authorization",
  "x-api-key",
  "api-key",
  "mcp-session-id",
  "mcp-protocol-version",
  "mcp-method",
  "mcp-name",
  "last-event-id",
]);
export type NetworkDependencies = {
  resolve: (hostname: string) => Promise<string[]>;
  request: (url: URL, options: RequestOptions) => ClientRequest;
};
const defaults: NetworkDependencies = {
  async resolve(hostname) {
    const results = await Promise.all(
      [resolve4(hostname), resolve6(hostname)].map(async (p) => {
        try {
          return await p;
        } catch (e) {
          if (
            e instanceof Error &&
            "code" in e &&
            ["ENODATA", "ENOTFOUND"].includes(String(e.code))
          )
            return [];
          throw Error("dns_failed");
        }
      }),
    );
    return results.flat();
  },
  request: (url, options) => httpsRequest(url, options),
};
export class ZonePolicy {
  readonly config: ZoneConfig;
  readonly digest: string;
  constructor(config: ZoneConfig) {
    this.config = structuredClone(config);
    if (
      !config.id ||
      !config.revision ||
      !["public", "internal"].includes(config.mode) ||
      !config.authorities.length
    )
      throw Error("invalid_zone");
    for (const authority of config.authorities) {
      const u = new URL(`https://${authority}/`);
      if (
        u.host !== authority ||
        !validHost(u.hostname) ||
        u.username ||
        u.password
      )
        throw Error("invalid_authority");
    }
    for (const alias of config.aliases)
      if (!validHost(alias)) throw Error("invalid_alias");
    for (const cidr of [...config.cidrs, ...config.denyCidrs])
      ipaddr.parseCIDR(cidr);
    if (config.mode === "internal")
      for (const cidr of config.cidrs) {
        const [address, prefix] = ipaddr.parseCIDR(cidr);
        if (
          !["10.0.0.0/8", "172.16.0.0/12", "192.168.0.0/16", "fc00::/7"].some(
            (base) => {
              const [network, bits] = ipaddr.parseCIDR(base);
              return (
                address.kind() === network.kind() &&
                prefix >= bits &&
                address.match(network, bits)
              );
            },
          )
        )
          throw Error("internal_cidr_denied");
      }
    this.digest = digest({
      version: 1,
      policy: config,
      permanentDenyCidrs: PERMANENT_DENY_CIDRS,
      caIdentity: config.ca ?? "system-roots",
      limits: {
        execute: 32,
        discover: 2,
        oauth: 4,
        schema: 4,
        toolMs: 60000,
        ...config.limits,
      },
    });
    for (const list of [
      this.config.authorities,
      this.config.aliases,
      this.config.cidrs,
      this.config.denyCidrs,
    ])
      Object.freeze(list);
    if (this.config.limits) Object.freeze(this.config.limits);
    Object.freeze(this.config);
  }
  endpoint(input: string): URL {
    const u = new URL(input);
    if (
      input !== u.href ||
      u.protocol !== "https:" ||
      u.username ||
      u.password ||
      u.search ||
      u.hash ||
      !validHost(u.hostname) ||
      !this.config.authorities.includes(u.host)
    )
      throw Error("destination_denied");
    return u;
  }
  checkAddresses(addresses: string[]) {
    if (!addresses.length || addresses.length > 64) throw Error("dns_denied");
    for (const raw of addresses) {
      const address = ipaddr.parse(raw);
      const range = address.range();
      // IPv4-mapped, translation, transition and reserved IPv6 ranges fail closed.
      if (!["unicast", "private", "uniqueLocal"].includes(range))
        throw Error("address_denied");
      if (
        address.kind() === "ipv6" &&
        range === "unicast" &&
        !matches(address, "2000::/3")
      )
        throw Error("address_denied");
      if (
        [...PERMANENT_DENY_CIDRS, ...this.config.denyCidrs].some((cidr) =>
          matches(address, cidr),
        )
      )
        throw Error("address_denied");
      if (this.config.mode === "public" && range !== "unicast")
        throw Error("address_denied");
      if (
        this.config.mode === "internal" &&
        !this.config.cidrs.some((cidr) => matches(address, cidr))
      )
        throw Error("address_denied");
    }
  }
}
function validHost(host: string) {
  return (
    !ipaddr.isValid(host.replace(/^\[|\]$/g, "")) &&
    host.length <= 253 &&
    host.includes(".") &&
    host
      .split(".")
      .every((s) => /^[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?$/.test(s))
  );
}
function matches(address: ipaddr.IPv4 | ipaddr.IPv6, cidr: string) {
  const [network, prefix] = ipaddr.parseCIDR(cidr);
  return address.kind() === network.kind() && address.match(network, prefix);
}
// Dependencies are a code-level transport seam, never an operator configuration
// bypass. Production uses only the defaults below.
export function restrictedFetch(
  zone: ZonePolicy,
  dependencies: NetworkDependencies = defaults,
): (
  input: Parameters<typeof fetch>[0],
  init?: Parameters<typeof fetch>[1],
) => ReturnType<typeof fetch> {
  return async (input, init) => {
    const raw = input instanceof Request ? input.url : String(input);
    const url = zone.endpoint(raw);
    const source = input instanceof Request ? input : undefined;
    const signal = init?.signal ?? source?.signal;
    signal?.throwIfAborted();
    const headers = new Headers(init?.headers ?? source?.headers);
    for (const key of headers.keys())
      if (!allowedHeaders.has(key)) throw Error("header_denied");
    headers.set("accept-encoding", "gzip, deflate, br");
    const method = init?.method ?? source?.method ?? "GET";
    if (!["GET", "POST", "DELETE"].includes(method))
      throw Error("method_denied");
    const body = init?.body ?? (source?.body ? await source.text() : undefined);
    if (body !== undefined && body !== null && typeof body !== "string")
      throw Error("body_denied");
    if (typeof body === "string" && Buffer.byteLength(body) > 1024 * 1024)
      throw Error("request_limit");
    const connectionDeadline = performance.now() + 5000;
    const connectionSignal = AbortSignal.any([
      ...(signal ? [signal] : []),
      AbortSignal.timeout(5000),
    ]);
    const addresses = await abortable(
      dependencies.resolve(url.hostname),
      connectionSignal,
    );
    zone.checkAddresses(addresses);
    connectionSignal.throwIfAborted();
    const address = addresses[0]!;
    return new Promise<Response>((resolve, reject) => {
      const req = dependencies.request(url, {
        method,
        headers: { ...Object.fromEntries(headers), host: url.host },
        servername: url.hostname,
        family: ipaddr.parse(address).kind() === "ipv4" ? 4 : 6,
        rejectUnauthorized: true,
        ...(zone.config.ca ? { ca: zone.config.ca } : {}),
        agent: false,
        // This callback returns the already checked address; it never repeats DNS.
        lookup: (_host, _options, callback) =>
          callback(
            null,
            address,
            ipaddr.parse(address).kind() === "ipv4" ? 4 : 6,
          ),
        ...(signal ? { signal } : {}),
        maxHeaderSize: 16384,
      });
      const timer = setTimeout(
        () => req.destroy(Error("connect_deadline")),
        Math.max(1, connectionDeadline - performance.now()),
      );
      req.once("socket", (socket) =>
        socket.once("secureConnect", () => clearTimeout(timer)),
      );
      req.once("error", () => {
        clearTimeout(timer);
        reject(Error("transport_failed"));
      });
      req.once("response", (res) => {
        clearTimeout(timer);
        let decoder: Transform | undefined;
        try {
          const status = res.statusCode;
          if (
            status === undefined ||
            !Number.isInteger(status) ||
            status < 200 ||
            status > 599
          )
            throw Error("response_invalid");
          if (status >= 300 && status < 400) {
            res.destroy();
            reject(Error("redirect_denied"));
            return;
          }
          const encoding = res.headers["content-encoding"];
          if (
            encoding &&
            !["identity", "gzip", "deflate", "br"].includes(encoding)
          ) {
            res.destroy();
            reject(Error("encoding_denied"));
            return;
          }
          decoder =
            encoding === "gzip"
              ? createGunzip()
              : encoding === "deflate"
                ? createInflate()
                : encoding === "br"
                  ? createBrotliDecompress()
                  : undefined;
          const stream = decoder ? res.pipe(decoder) : res;
          let received = 0;
          const responseHeaders = new Headers();
          for (const [name, value] of Object.entries(res.headers))
            if (
              value !== undefined &&
              !["content-encoding", "content-length", "set-cookie"].includes(
                name,
              )
            )
              responseHeaders.set(
                name,
                Array.isArray(value) ? value.join(",") : value,
              );
          const bodyStream = new ReadableStream<Uint8Array>({
            start(controller) {
              stream.on("data", (chunk: Buffer) => {
                received += chunk.length;
                if (received > RESPONSE_BYTES) {
                  stream.destroy(Error("response_limit"));
                  res.destroy();
                  return;
                }
                controller.enqueue(chunk);
              });
              stream.once("end", () => controller.close());
              stream.once("error", () =>
                controller.error(Error("response_failed")),
              );
              res.once("error", () => {
                if (decoder) decoder.destroy(Error("response_failed"));
              });
            },
            cancel() {
              stream.destroy();
              res.destroy();
              req.destroy();
            },
          });
          resolve(
            new Response([204, 205, 304].includes(status) ? null : bodyStream, {
              status,
              headers: responseHeaders,
            }),
          );
        } catch {
          decoder?.destroy();
          res.destroy();
          req.destroy();
          reject(Error("response_invalid"));
        }
      });
      req.end(body);
    });
  };
}
export async function abortable<T>(
  promise: Promise<T>,
  signal: AbortSignal,
): Promise<T> {
  signal.throwIfAborted();
  return new Promise<T>((resolve, reject) => {
    const abort = () => reject(Error("operation_aborted"));
    signal.addEventListener("abort", abort, { once: true });
    promise
      .then(resolve, reject)
      .finally(() => signal.removeEventListener("abort", abort));
  });
}
