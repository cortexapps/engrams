//! A private namespace owns its TAP, rules, listeners and conntrack table.
use serde::{Deserialize, Serialize};
use std::{io, net::Ipv4Addr};

#[cfg(any(target_os = "linux", test))]
pub(super) const TAP: &str = "managed0";
pub(super) const GUEST_MAC: &str = "02:00:00:00:00:02";
pub(super) const GATEWAY_MAC: &str = "02:00:00:00:00:01";
#[cfg(target_os = "linux")]
pub(super) const PROXY_PORT: u16 = 15000;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetworkShape {
    pub guest: Ipv4Addr,
    pub gateway: Ipv4Addr,
    pub guest_mac: String,
    pub gateway_mac: String,
    pub mtu: u32,
}
impl NetworkShape {
    pub(super) fn new(guest: Ipv4Addr, gateway: Ipv4Addr) -> io::Result<Self> {
        let valid = |ip: Ipv4Addr| {
            !ip.is_loopback()
                && !ip.is_unspecified()
                && !ip.is_multicast()
                && !ip.is_link_local()
                && ip.octets()[0] != 0
                && ip.octets()[0] < 224
                && !matches!(ip.octets()[3], 0 | 255)
        };
        if guest == gateway
            || !valid(guest)
            || !valid(gateway)
            || guest.octets()[..3] != gateway.octets()[..3]
        {
            return Err(io::Error::other("invalid managed subnet"));
        }
        Ok(Self {
            guest,
            gateway,
            guest_mac: GUEST_MAC.into(),
            gateway_mac: GATEWAY_MAC.into(),
            mtu: 1500,
        })
    }
}

#[cfg(target_os = "linux")]
pub(super) fn rules(shape: &NetworkShape) -> String {
    let guest = shape.guest;
    let gateway = shape.gateway;
    // CLOSED precedes even ARP and tracked packets. The second netdev table
    // retains the source/destination checks after a later exact release.
    format!(
        r#"table netdev closed {{
 chain ingress {{ type filter hook ingress device "{TAP}" priority -500; policy drop; }}
 chain egress {{ type filter hook egress device "{TAP}" priority -500; policy drop; }}
}}
table netdev identity {{
 chain ingress {{ type filter hook ingress device "{TAP}" priority -450; policy drop;
  ether saddr {GUEST_MAC} ether type arp arp operation request arp saddr ether {GUEST_MAC} arp saddr ip {guest} arp daddr ip {gateway} accept
  ether saddr {GUEST_MAC} ether daddr {GATEWAY_MAC} ether type arp arp operation reply arp saddr ether {GUEST_MAC} arp saddr ip {guest} arp daddr ether {GATEWAY_MAC} arp daddr ip {gateway} accept
  ether saddr {GUEST_MAC} ether daddr {GATEWAY_MAC} ether type ip ip saddr {guest} accept
 }}
 chain egress {{ type filter hook egress device "{TAP}" priority -450; policy drop;
  ether saddr {GATEWAY_MAC} ether daddr ff:ff:ff:ff:ff:ff ether type arp arp operation request arp saddr ether {GATEWAY_MAC} arp saddr ip {gateway} arp daddr ip {guest} accept
  ether saddr {GATEWAY_MAC} ether daddr {GUEST_MAC} ether type arp arp operation reply arp saddr ether {GATEWAY_MAC} arp saddr ip {gateway} arp daddr ip {guest} accept
  ether saddr {GATEWAY_MAC} ether daddr {GUEST_MAC} ether type ip ip daddr {guest} accept
 }}
}}
table inet closed {{
 chain input {{ type filter hook input priority -200; policy drop; }}
 chain output {{ type filter hook output priority -200; policy drop; }}
 chain forward {{ type filter hook forward priority -200; policy drop; }}
}}
table inet strict {{
 chain classify {{ type filter hook prerouting priority -150; policy drop;
  iifname "{TAP}" ip saddr {guest} ip daddr {gateway} udp dport 53 accept
  iifname "{TAP}" ip saddr {guest} ip daddr != {gateway} tcp dport 53 drop
  iifname "{TAP}" ip saddr {guest} meta l4proto tcp accept
 }}
 chain redirect {{ type nat hook prerouting priority -110; policy accept;
  iifname "{TAP}" ip saddr {guest} meta l4proto tcp redirect to :{PROXY_PORT}
 }}
 chain input {{ type filter hook input priority -100; policy drop;
  iifname "{TAP}" ip saddr {guest} ct status dnat tcp dport {PROXY_PORT} accept
  iifname "{TAP}" ip saddr {guest} ip daddr {gateway} udp dport 53 accept
 }}
 chain output {{ type filter hook output priority -100; policy drop;
  oifname "{TAP}" ip daddr {guest} ct state established tcp sport {PROXY_PORT} accept
  oifname "{TAP}" ip saddr {gateway} ip daddr {guest} udp sport 53 accept
 }}
 chain forward {{ type filter hook forward priority -100; policy drop; }}
}}
"#
    )
}

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub(super) use linux::Namespace;

#[cfg(any(target_os = "linux", test))]
use serde_json::Value;
#[cfg(any(target_os = "linux", test))]
fn verify_links(links: &Value, shape: &NetworkShape) -> io::Result<u32> {
    let links = links
        .as_array()
        .ok_or_else(|| io::Error::other("invalid links"))?;
    if links.len() != 2 || !links.iter().any(|v| v["ifname"] == "lo") {
        return Err(io::Error::other("unexpected namespace links"));
    }
    let tap = links
        .iter()
        .find(|v| v["ifname"] == TAP)
        .ok_or_else(|| io::Error::other("missing TAP"))?;
    if tap["address"] != shape.gateway_mac
        || tap["mtu"] != shape.mtu
        || tap["linkinfo"]["info_kind"] != "tun"
        || tap["linkinfo"]["info_data"]["type"] != "tap"
    {
        return Err(io::Error::other("wrong TAP shape"));
    }
    tap["ifindex"]
        .as_u64()
        .and_then(|n| u32::try_from(n).ok())
        .filter(|n| *n > 0)
        .ok_or_else(|| io::Error::other("invalid TAP index"))
}
#[cfg(any(target_os = "linux", test))]
fn verify_closed_tables(rules: &Value) -> io::Result<()> {
    let values = rules["nftables"]
        .as_array()
        .ok_or_else(|| io::Error::other("invalid nft rules"))?;
    if values.iter().any(|v| v["rule"]["table"] == "closed") {
        return Err(io::Error::other("closed fence contains rules"));
    }
    for (family, hook, priority) in [
        ("netdev", "ingress", -500),
        ("netdev", "egress", -500),
        ("inet", "input", -200),
        ("inet", "output", -200),
        ("inet", "forward", -200),
    ] {
        if !values.iter().any(|v| {
            v["chain"]["family"] == family
                && v["chain"]["table"] == "closed"
                && v["chain"]["hook"] == hook
                && v["chain"]["policy"] == "drop"
                && v["chain"]["prio"] == priority
                && (family != "netdev" || v["chain"]["dev"] == TAP)
        }) {
            return Err(io::Error::other("closed fence absent"));
        }
    }
    Ok(())
}

#[cfg(any(target_os = "linux", test))]
fn stable_link_shape(value: Value) -> io::Result<Value> {
    let links = value
        .as_array()
        .ok_or_else(|| io::Error::other("invalid links"))?;
    Ok(Value::Array(links.iter().map(|v| serde_json::json!({
        "ifindex": v["ifindex"], "ifname": v["ifname"], "address": v["address"], "mtu": v["mtu"],
        "link_type": v["link_type"], "master": v["master"],
        "up": v["flags"].as_array().is_some_and(|flags| flags.iter().any(|f| f == "UP")),
        "linkinfo": {"info_kind": v["linkinfo"]["info_kind"], "info_data": {"type": v["linkinfo"]["info_data"]["type"]}}
    })).collect()))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn readback_rejects_accept_rules_or_wrong_hook_priority_in_closed_fence() {
        let mut objects: Vec<Value> = [("netdev", "ingress", -500), ("netdev", "egress", -500), ("inet", "input", -200), ("inet", "output", -200), ("inet", "forward", -200)].into_iter().map(|(family, hook, priority)| serde_json::json!({"chain":{"family":family,"table":"closed","hook":hook,"policy":"drop", "prio": priority,"dev":"managed0"}})).collect();
        let good = serde_json::json!({"nftables":objects});
        verify_closed_tables(&good).unwrap();
        objects[0]["chain"]["prio"] = 0.into();
        assert!(
            verify_closed_tables(&serde_json::json!({"nftables":objects})).is_err(),
            "late ingress fence is not the approved closed boundary"
        );
        let mut changed = good.clone();
        changed["nftables"].as_array_mut().unwrap().push(serde_json::json!({"rule":{"family":"netdev","table":"closed","chain":"ingress","expr":[{"accept":null}]}}));
        assert!(
            verify_closed_tables(&changed).is_err(),
            "an early accept in a default-drop chain is not closed"
        );
    }
    #[test]
    fn live_link_readback_requires_private_tap_type_and_exact_selected_shape() {
        let shape =
            NetworkShape::new("192.0.2.2".parse().unwrap(), "192.0.2.1".parse().unwrap()).unwrap();
        let good = serde_json::json!([{"ifname":"lo","ifindex":1}, {"ifname":"managed0","ifindex":2,"address":"02:00:00:00:00:01","mtu":1500,"linkinfo":{"info_kind":"tun","info_data":{"type":"tap"}}}]);
        assert_eq!(verify_links(&good, &shape).unwrap(), 2);
        for (key, replacement) in [
            ("address", serde_json::json!("02:00:00:00:00:03")),
            ("mtu", serde_json::json!(1400)),
            ("ifindex", serde_json::json!(0)),
        ] {
            let mut bad = good.clone();
            bad[1][key] = replacement;
            assert!(verify_links(&bad, &shape).is_err());
        }
        let mut bad = good.clone();
        bad[1]["linkinfo"]["info_data"]["type"] = "tun".into();
        assert!(verify_links(&bad, &shape).is_err());
        let mut bad = good;
        bad.as_array_mut()
            .unwrap()
            .push(serde_json::json!({"ifname":"escape0","ifindex":3}));
        assert!(verify_links(&bad, &shape).is_err());
    }
    #[test]
    fn opening_same_tap_changes_carrier_and_queue_count_not_attachment_identity() {
        let before = serde_json::json!([{"ifname":"managed0","ifindex":2,"address":"02:00:00:00:00:01","mtu":1500,"flags":["BROADCAST","UP"],"operstate":"DOWN", "linkinfo":{"info_kind":"tun","info_data":{"type":"tap","num_queues":0}}}]);
        let mut after = before.clone();
        after[0]["flags"] = serde_json::json!(["BROADCAST", "UP", "LOWER_UP"]);
        after[0]["operstate"] = "UP".into();
        after[0]["linkinfo"]["info_data"]["num_queues"] = 1.into();
        assert_eq!(
            stable_link_shape(before.clone()).unwrap(),
            stable_link_shape(after.clone()).unwrap()
        );
        after[0]["ifindex"] = 3.into();
        assert_ne!(
            stable_link_shape(before.clone()).unwrap(),
            stable_link_shape(after.clone()).unwrap()
        );
        after[0]["ifindex"] = 2.into();
        after[0]["flags"] = serde_json::json!(["BROADCAST"]);
        assert_ne!(
            stable_link_shape(before).unwrap(),
            stable_link_shape(after).unwrap()
        );
    }
    #[test]
    fn rejects_subnet_escape_and_non_unicast_addresses() {
        for (guest, gateway) in [
            ("192.0.2.2", "198.51.100.1"),
            ("127.0.0.2", "127.0.0.1"),
            ("224.0.0.2", "224.0.0.1"),
            ("192.0.2.0", "192.0.2.1"),
            ("192.0.2.2", "192.0.2.255"),
            ("192.0.2.2", "192.0.2.2"),
        ] {
            assert!(
                NetworkShape::new(guest.parse().unwrap(), gateway.parse().unwrap()).is_err(),
                "{guest} {gateway}"
            );
        }
    }
}
