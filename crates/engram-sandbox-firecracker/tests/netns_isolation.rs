//! ADR 0014 M1.16 — per-VM netns + SNAT isolation.
//!
//! Pins the contract of `engram_sandbox_firecracker::net::provision_netns`
//! and `teardown_netns` without spinning up Firecracker. Two warm
//! slots from one canonical snapshot share the same bake-time TAP
//! name (`tap-engr-<bake-id>`) and CIDR (`10.200.0.0/30`), so the
//! M1.16 invariant is: those names must live inside two independent
//! netns'es with distinct host-side SNAT IPs.
//!
//! What this test exercises end-to-end:
//!   1. Two `provision_netns` calls with the same fake `bake_cidr` +
//!      `tap_name` (simulating two warm slots from one snapshot)
//!      succeed independently — TAP-name collisions don't happen
//!      because TAP names are netns-local.
//!   2. Each netns has the bake's TAP inside it (`ip -n <ns> link
//!      show <tap>`), and the TAP carries the bake's gateway IP.
//!   3. Each netns gets a distinct host-side `snat_cidr.guest()`
//!      from `NetworkAllocator` — these are the host-reachable IPs
//!      the egress proxy registry indexes against and the dashboard
//!      shell tab dials for ttyd.
//!   4. The bake's TAP name does NOT exist in the host root netns
//!      after provisioning (proves the netns isolation).
//!   5. `teardown_netns` removes the netns + veth-A and frees the
//!      SNAT slot back to the allocator, so a third provision can
//!      reuse the IP.
//!
//! Doesn't exercise: FC spawn-in-netns (covered by `snapshot_net`
//! once it lands a netns'd restore) and actual outbound packet
//! flow through the SNAT'd source (covered by `proxy_e2e` once it
//! migrates to the netns shape).
//!
//! Runs on Linux + root only. The CI `test-firecracker` job already
//! satisfies both prereqs (it `sudo`'s in to run the other ignored
//! tests in this directory). Run locally via:
//!
//! ```sh
//! sudo -E env "PATH=$PATH" cargo test \
//!     -p engram-sandbox-firecracker --test netns_isolation -- --ignored
//! ```

#![cfg(target_os = "linux")]

use std::net::Ipv4Addr;
use std::str::FromStr;

use engram_core::types::ids::SandboxId;
use engram_sandbox_firecracker::net::{
    netns_name_for, netns_path_for, provision_netns, tap_name_for, teardown_netns, veth_names_for,
    NetworkAllocator, VmCidr,
};

/// `/proc/self/status`-based root check matching the other ignored
/// FC integration tests. Returns `false` (after printing SKIP) when
/// not root.
fn require_root() -> bool {
    let status = std::fs::read_to_string("/proc/self/status").expect("read /proc/self/status");
    let euid: u32 = status
        .lines()
        .find(|l| l.starts_with("Uid:"))
        .and_then(|l| l.split_whitespace().nth(2).and_then(|s| s.parse().ok()))
        .expect("parse euid");
    if euid != 0 {
        eprintln!(
            "SKIP: netns_isolation requires root (CAP_NET_ADMIN for \
             `ip netns add` + veth pair + `ip tuntap add`). \
             Re-run via `sudo -E ...`."
        );
        return false;
    }
    true
}

/// Best-effort wipe of any `engr-vm-*` leftovers from a prior failed
/// run. The kernel rejects `ip netns add <name>` when `<name>`
/// already exists, so a stale netns from a crashed test would block
/// re-runs. Same shape as `delete_stale_taps` in snapshot_net.rs.
fn delete_stale_netns() {
    let saved = std::process::Command::new("ip")
        .args(["netns", "list"])
        .output();
    let Ok(o) = saved else {
        return;
    };
    for line in String::from_utf8_lossy(&o.stdout).lines() {
        // `ip netns list` prints "<name>" or "<name> (id: <n>)". Take
        // the first whitespace-separated token.
        let name = line.split_whitespace().next().unwrap_or("");
        if !name.starts_with("engr-vm-") {
            continue;
        }
        let _ = std::process::Command::new("ip")
            .args(["netns", "delete", name])
            .output();
    }
    // veth-A's left over by a crashed test also block re-runs (Linux
    // refuses to add a link with a name already in use). Best-effort
    // delete every `vh-engr-*` interface in the host root.
    let saved = std::process::Command::new("sh")
        .arg("-c")
        .arg("ip -o link show | awk -F': ' '/vh-engr-/ {print $2}' | awk '{print $1}'")
        .output();
    let Ok(o) = saved else {
        return;
    };
    for line in String::from_utf8_lossy(&o.stdout).lines() {
        let name = line.trim();
        if name.is_empty() {
            continue;
        }
        let _ = std::process::Command::new("ip")
            .args(["link", "delete", name])
            .output();
    }
}

/// `ip -n <ns> link show <name>` succeeds (exit 0) iff the device
/// is present inside the netns.
fn tap_in_netns(ns: &str, name: &str) -> bool {
    std::process::Command::new("ip")
        .args(["-n", ns, "link", "show", name])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// `ip link show <name>` succeeds iff the device is present in host
/// root. Used to assert M1.16 isolation: the bake's TAP name should
/// NOT appear in host root after `provision_netns`.
fn tap_in_host_root(name: &str) -> bool {
    std::process::Command::new("ip")
        .args(["link", "show", name])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// `ip -n <ns> addr show dev <iface>` — returns true if the listed
/// addresses contain `expected`. Pins the address-assignment step
/// of `provision_netns`.
fn iface_has_addr_in_netns(ns: &str, iface: &str, expected: Ipv4Addr) -> bool {
    let out = match std::process::Command::new("ip")
        .args(["-n", ns, "-4", "addr", "show", "dev", iface])
        .output()
    {
        Ok(o) if o.status.success() => o,
        _ => return false,
    };
    let text = String::from_utf8_lossy(&out.stdout);
    text.contains(&format!("inet {expected}/"))
}

/// `ip netns exec <ns> iptables -t nat -S POSTROUTING` lists the
/// netns's NAT POSTROUTING rules. We assert that a SNAT rule
/// targeting the expected source IP is present — this is the
/// load-bearing transformation that makes warm slots distinguishable
/// to the host-root egress proxy.
fn snat_rule_present(ns: &str, snat_to: Ipv4Addr) -> bool {
    let out = match std::process::Command::new("ip")
        .args([
            "netns",
            "exec",
            ns,
            "iptables",
            "-t",
            "nat",
            "-S",
            "POSTROUTING",
        ])
        .output()
    {
        Ok(o) if o.status.success() => o,
        _ => return false,
    };
    let text = String::from_utf8_lossy(&out.stdout);
    text.contains(&format!("--to-source {snat_to}"))
}

#[tokio::test]
#[ignore = "requires Linux + root (CAP_NET_ADMIN for netns + veth + iptables)"]
async fn netns_provision_isolates_two_warm_slots_from_one_snapshot() {
    if !require_root() {
        return;
    }
    delete_stale_netns();

    let allocator = parking_lot::Mutex::new(NetworkAllocator::new(
        Ipv4Addr::from_str("10.200.0.0").unwrap(),
    ));

    // Both slots share the bake's CIDR + TAP name (one snapshot →
    // two restores). The M1.16 invariant: they coexist by living in
    // separate netns'es with separate SNAT slots.
    let bake_cidr = VmCidr::new(Ipv4Addr::from_str("10.200.0.0").unwrap());
    let bake_tap = "tap-engr-baked0"; // 14 chars, IFNAMSIZ-safe

    let slot1_id = SandboxId::new();
    let slot2_id = SandboxId::new();
    let ns1_name = netns_name_for(slot1_id);
    let ns2_name = netns_name_for(slot2_id);
    let (vh1, _vg1) = veth_names_for(slot1_id);
    let (vh2, _vg2) = veth_names_for(slot2_id);

    // 1. Provision both warm slots from the same bake.
    let setup1 = provision_netns(slot1_id, bake_cidr, bake_tap, &allocator)
        .await
        .expect("provision slot 1");
    let setup2 = provision_netns(slot2_id, bake_cidr, bake_tap, &allocator)
        .await
        .expect("provision slot 2");

    // 2. Each netns exists at its kernel-published path.
    assert!(
        netns_path_for(slot1_id).exists(),
        "netns {ns1_name} must publish at /var/run/netns",
    );
    assert!(
        netns_path_for(slot2_id).exists(),
        "netns {ns2_name} must publish at /var/run/netns",
    );

    // 3. Bake's TAP name exists IN EACH netns and only there.
    assert!(
        tap_in_netns(&ns1_name, bake_tap),
        "bake TAP {bake_tap} must be present inside slot 1's netns",
    );
    assert!(
        tap_in_netns(&ns2_name, bake_tap),
        "bake TAP {bake_tap} must be present inside slot 2's netns",
    );
    assert!(
        !tap_in_host_root(bake_tap),
        "bake TAP {bake_tap} must NOT leak into host root (the whole \
         point of the netns isolation is that the bake's name can be \
         reused per-slot without colliding globally)",
    );

    // 4. TAP carries the bake's gateway IP inside each netns. Without
    //    this, the VM's default route (via `bake_cidr.host()`) would
    //    ARP into the void.
    assert!(
        iface_has_addr_in_netns(&ns1_name, bake_tap, bake_cidr.host()),
        "bake TAP in slot 1's netns must hold gateway IP {}",
        bake_cidr.host(),
    );
    assert!(
        iface_has_addr_in_netns(&ns2_name, bake_tap, bake_cidr.host()),
        "bake TAP in slot 2's netns must hold gateway IP {}",
        bake_cidr.host(),
    );

    // 5. Distinct host-side SNAT IPs. This is what the egress proxy
    //    registry indexes by and what `guest_ip()` returns for the
    //    dashboard shell tab — collide them and N>1 warm slots all
    //    look like one session to the host stack.
    assert_ne!(
        setup1.snat_cidr.guest(),
        setup2.snat_cidr.guest(),
        "warm slots must get distinct SNAT'd source IPs",
    );
    assert_eq!(
        setup1.host_reachable_ip(),
        setup1.snat_cidr.guest(),
        "host_reachable_ip is the SNAT'd guest octet",
    );

    // 6. Each netns has a SNAT rule pointing at its own slot.
    assert!(
        snat_rule_present(&ns1_name, setup1.snat_cidr.guest()),
        "slot 1's netns must SNAT to its own pool slot {}",
        setup1.snat_cidr.guest(),
    );
    assert!(
        snat_rule_present(&ns2_name, setup2.snat_cidr.guest()),
        "slot 2's netns must SNAT to its own pool slot {}",
        setup2.snat_cidr.guest(),
    );

    // 7. veth host-side endpoints are in host root (they bridge
    //    netns → host's iptables). These ARE expected in host root —
    //    only the bake's TAP isn't.
    assert!(
        tap_in_host_root(&vh1),
        "veth host-end {vh1} must live in host root",
    );
    assert!(
        tap_in_host_root(&vh2),
        "veth host-end {vh2} must live in host root",
    );

    // 8. Teardown reclaims both. After this every device the test
    //    created should be gone, and the SNAT slots should be back
    //    in the allocator so a third provision can reuse them.
    let snat1 = setup1.snat_cidr;
    let snat2 = setup2.snat_cidr;
    teardown_netns(&setup1, &allocator).await;
    teardown_netns(&setup2, &allocator).await;

    assert!(
        !netns_path_for(slot1_id).exists(),
        "slot 1 netns must be deleted post-teardown",
    );
    assert!(
        !netns_path_for(slot2_id).exists(),
        "slot 2 netns must be deleted post-teardown",
    );
    assert!(
        !tap_in_host_root(&vh1),
        "veth {vh1} must be auto-cleaned by netns deletion",
    );
    assert!(
        !tap_in_host_root(&vh2),
        "veth {vh2} must be auto-cleaned by netns deletion",
    );

    // 9. Reservation round-trips: post-teardown the allocator should
    //    hand the same slots back to a new caller. This pins the
    //    "destroy releases the slot" half of the contract.
    let mut a = allocator.lock();
    a.reserve(snat1)
        .expect("snat1 must be reservable post-free");
    a.reserve(snat2)
        .expect("snat2 must be reservable post-free");
    drop(a);

    // tap_name_for is exercised here only to keep the import set
    // honest if the function ever moves; same module pin pattern as
    // snapshot_net.rs uses for `netns_name_for`.
    let _ = tap_name_for(slot1_id);
}

/// Issue #536 (S1): the idempotency pre-clean in `provision_netns_inner`
/// went from unconditional `ip netns delete`/`ip link delete` spawns to
/// existence/`link_index`-gated ones. Pins that a partial-failure
/// leftover — a netns bind-mount plus a host-side veth from a prior
/// crashed provision — is still cleaned up correctly before the retry
/// proceeds, and that the end-state topology after the retry is
/// identical to a from-scratch provision.
#[tokio::test]
#[ignore = "requires Linux + root (CAP_NET_ADMIN for netns + veth + iptables)"]
async fn netns_provision_retries_past_a_stale_leftover() {
    if !require_root() {
        return;
    }
    delete_stale_netns();

    let allocator = parking_lot::Mutex::new(NetworkAllocator::new(
        Ipv4Addr::from_str("10.200.0.0").unwrap(),
    ));

    let bake_cidr = VmCidr::new(Ipv4Addr::from_str("10.200.0.0").unwrap());
    let bake_tap = "tap-engr-baked1"; // 15 chars, IFNAMSIZ-safe; distinct from the other test's

    let sandbox_id = SandboxId::new();
    let ns_name = netns_name_for(sandbox_id);
    let (veth_host, _veth_ns) = veth_names_for(sandbox_id);

    // Pre-seed exactly the leftover state a crashed prior provision
    // for this sandbox_id would strand: the netns bind-mount and the
    // host-root veth. Neither `ip netns add` nor the veth add below
    // would succeed a second time without cleanup — that's the
    // regression this test guards against.
    let status = std::process::Command::new("ip")
        .args(["netns", "add", &ns_name])
        .status()
        .expect("spawn ip netns add");
    assert!(status.success(), "pre-seed netns add must succeed");
    let status = std::process::Command::new("ip")
        .args([
            "link",
            "add",
            &veth_host,
            "type",
            "veth",
            "peer",
            "name",
            "vh-stale-peer0",
        ])
        .status()
        .expect("spawn ip link add");
    assert!(status.success(), "pre-seed veth add must succeed");

    // Retry: provision_netns must succeed despite the leftover state,
    // via the existence/link_index-gated pre-clean rather than
    // tripping over an "already exists" error from `ip netns add` or
    // the veth add.
    let setup = provision_netns(sandbox_id, bake_cidr, bake_tap, &allocator)
        .await
        .expect("provision must succeed past a stale leftover");

    // End-state topology matches a from-scratch provision: the bake
    // TAP lives inside the (freshly re-created) netns and nowhere in
    // host root, and the netns republishes at the expected path.
    assert!(
        netns_path_for(sandbox_id).exists(),
        "netns must be (re-)published after the retry",
    );
    assert!(
        tap_in_netns(&ns_name, bake_tap),
        "bake TAP must be present inside the netns after the retry",
    );
    assert!(
        !tap_in_host_root(bake_tap),
        "bake TAP must not leak into host root after the retry",
    );
    assert!(
        tap_in_host_root(&veth_host),
        "veth host-end must live in host root after the retry",
    );

    teardown_netns(&setup, &allocator).await;
    assert!(
        !netns_path_for(sandbox_id).exists(),
        "netns must be deleted post-teardown",
    );
}
