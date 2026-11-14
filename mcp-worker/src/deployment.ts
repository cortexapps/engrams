import ipaddr from "ipaddr.js";
import { PERMANENT_DENY_CIDRS } from "./network.ts";
export interface EgressRule {
  cidr: string;
  except: string[];
}
export const RESERVED_CIDRS = [
  "0.0.0.0/8",
  "100.64.0.0/10",
  "127.0.0.0/8",
  "169.254.0.0/16",
  "192.0.0.0/24",
  "192.0.2.0/24",
  "192.88.99.0/24",
  "198.18.0.0/15",
  "198.51.100.0/24",
  "203.0.113.0/24",
  "224.0.0.0/4",
  "240.0.0.0/4",
  "2001::/23",
  "2001:db8::/32",
  "2002::/16",
  ...PERMANENT_DENY_CIDRS,
];
function contains(outer: string, inner: string) {
  const [a, ap] = ipaddr.parseCIDR(outer),
    [b, bp] = ipaddr.parseCIDR(inner);
  return a.kind() === b.kind() && ap <= bp && b.match(a, ap);
}
export function validateEgressRules(
  rules: EgressRule[],
  denies: string[],
  allowedCidrs?: string[],
  mode: "public" | "internal" = "internal",
): void {
  if (!rules.length || rules.length > 128) throw Error("egress_rules_required");
  const blocked = [
    ...RESERVED_CIDRS,
    ...denies,
    ...(mode === "public"
      ? ["10.0.0.0/8", "172.16.0.0/12", "192.168.0.0/16", "fc00::/7"]
      : []),
  ];
  for (const rule of rules) {
    if (
      mode === "internal" &&
      allowedCidrs &&
      !allowedCidrs.some((cidr) => contains(cidr, rule.cidr))
    )
      throw Error("egress_exceeds_zone");
    const [address] = ipaddr.parseCIDR(rule.cidr);
    if (
      address.kind() === "ipv6" &&
      !["2000::/3", "fc00::/7"].some((base) => contains(base, rule.cidr))
    )
      throw Error("ipv6_route_denied");
    for (const exception of rule.except)
      if (!contains(rule.cidr, exception))
        throw Error("invalid_egress_exception");
    for (const deny of blocked) {
      if (contains(deny, rule.cidr)) throw Error("egress_route_denied");
      if (
        contains(rule.cidr, deny) &&
        !rule.except.some((exception) => contains(exception, deny))
      )
        throw Error("egress_exception_required");
    }
  }
}
