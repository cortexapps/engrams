import { check, hash, record } from "./owned.ts";
import type { Inputs } from "./preflight.ts";

export const ADDRESS = {
  postgres: 10,
  blob: 11,
  registry: 12,
  dns: 13,
  orchestrator: 20,
  coordinator: 21,
  worker: 22,
  modern: 30,
  legacy: 31,
  denied: 40,
} as const;
export type GuardRole = "orchestrator" | "coordinator" | "worker";
export const GUARDS: GuardRole[] = ["orchestrator", "coordinator", "worker"];
export function address(
  inputs: Pick<Inputs, "subnet">,
  role: keyof typeof ADDRESS,
): string {
  return inputs.subnet.slice(0, -4) + ADDRESS[role];
}
function hostRpcAddress(
  inputs: Pick<Inputs, "backend" | "subnet" | "nativeHostIp">,
): string {
  return inputs.backend === "firecracker"
    ? address(inputs, "coordinator")
    : inputs.nativeHostIp;
}
export function dnsRecords(
  inputs: Pick<Inputs, "backend" | "subnet" | "nativeHostIp">,
): Record<string, string> {
  const records: Record<string, string> = {
    "postgres.fixture.example": address(inputs, "postgres"),
    "app.fixture.example": address(inputs, "orchestrator"),
    "coordinator.fixture.example": address(inputs, "coordinator"),
    "operator.fixture.example": address(inputs, "coordinator"),
    "control.fixture.example": address(inputs, "coordinator"),
    "worker.fixture.example": address(inputs, "worker"),
    "host.fixture.example": hostRpcAddress(inputs),
  };
  for (const protocol of ["modern", "legacy"] as const)
    for (const name of [
      "resource",
      "issuer",
      "authorization",
      "token",
      "observer",
    ])
      records[`${name}.${protocol}.fixture.example`] = address(
        inputs,
        protocol,
      );
  return records;
}
export function dnsHosts(
  inputs: Pick<Inputs, "backend" | "subnet" | "nativeHostIp">,
): string {
  return (
    Object.entries(dnsRecords(inputs))
      .map(([name, ip]) => `${ip} ${name}`)
      .join("\n") + "\n"
  );
}

// Install these exact nft objects before an app joins its guard namespace.
// No forwarding chain, default route allowance, or external DNS server is installed.
export function rules(
  inputs: Pick<Inputs, "backend" | "subnet" | "nativeHostIp">,
  role: GuardRole,
): Record<string, unknown>[] {
  const result: Record<string, unknown>[] = [
    { table: { family: "inet", name: "mcp" } },
    {
      chain: {
        family: "inet",
        table: "mcp",
        name: "output",
        type: "filter",
        hook: "output",
        prio: 0,
        policy: "drop",
      },
    },
  ];
  const rule = (expr: unknown[]) =>
    result.push({
      rule: { family: "inet", table: "mcp", chain: "output", expr },
    });
  const match = (left: unknown, right: unknown) => ({
    match: { op: "==", left, right },
  });
  const dest = (ip: string) =>
    match({ payload: { protocol: "ip", field: "daddr" } }, ip);
  // Docker's embedded resolver must never bypass the fixture DNS server.
  rule([dest("127.0.0.11"), { drop: null }]);
  rule([
    match({ ct: { key: "state" } }, { set: ["established", "related"] }),
    { accept: null },
  ]);
  rule([match({ meta: { key: "oifname" } }, "lo"), { accept: null }]);
  const allow = (ip: string, protocol: "tcp" | "udp", port: number) =>
    rule([
      dest(ip),
      match({ payload: { protocol, field: "dport" } }, port),
      { accept: null },
    ]);
  allow(address(inputs, "dns"), "udp", 53);
  allow(address(inputs, "dns"), "tcp", 53);
  for (const fixture of ["modern", "legacy"] as const)
    allow(address(inputs, fixture), "tcp", 8443);
  if (role !== "worker") {
    allow(address(inputs, "postgres"), "tcp", 5432);
    allow(address(inputs, "worker"), "tcp", 8443);
    allow(hostRpcAddress(inputs), "tcp", 9101);
  }
  if (role === "orchestrator") {
    for (const port of [443, 50061, 8444])
      allow(address(inputs, "coordinator"), "tcp", port);
  }
  if (role === "coordinator") {
    allow(address(inputs, "blob"), "tcp", 4443);
    allow(address(inputs, "registry"), "tcp", 5000);
    allow(address(inputs, "orchestrator"), "tcp", 443);
  }
  return result;
}
function canonical(value: unknown): string {
  if (Array.isArray(value)) return "[" + value.map(canonical).join(",") + "]";
  if (value !== null && typeof value === "object")
    return (
      "{" +
      Object.entries(value)
        .sort(([a], [b]) => a.localeCompare(b))
        .map(([k, v]) => JSON.stringify(k) + ":" + canonical(v))
        .join(",") +
      "}"
    );
  return JSON.stringify(value);
}
export function effectiveRules(
  value: unknown,
  expected: Record<string, unknown>[],
): string {
  const o = record(value, ["nftables"]);
  check(Array.isArray(o.nftables), "nft_shape");
  const actual = o.nftables.flatMap((item: unknown) => {
    check(
      item !== null && typeof item === "object" && !Array.isArray(item),
      "nft_object",
    );
    const object = item as Record<string, unknown>;
    if (Object.keys(object).length === 1 && object.metainfo) return [];
    check(Object.keys(object).length === 1, "nft_object");
    const [kind, body] = Object.entries(object)[0]!;
    check(
      ["table", "chain", "rule"].includes(kind) &&
        body !== null &&
        typeof body === "object" &&
        !Array.isArray(body),
      "nft_object",
    );
    const copy = { ...body };
    delete (copy as Record<string, unknown>).handle;
    return [{ [kind]: copy }];
  });
  check(canonical(actual) === canonical(expected), "network_rules_changed");
  return hash(canonical(actual));
}
