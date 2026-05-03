//! Live test of the per-VM networking provisioning path.
//!
//! Unlike the rest of the FC integration suite, this one exercises the
//! Linux runtime layer directly: it creates a TAP, applies the per-VM
//! iptables chain, and tears them down. No microVM is booted — this is
//! about confirming our `net::provision` / `net::teardown` actually
//! talk to the kernel correctly.
//!
//! Run with sudo on the dev VM:
//!
//!   sudo -E env "PATH=$PATH" cargo test \
//!       -p engram-sandbox-firecracker --test network_provision -- --ignored
//!
//! The `-E` is critical: cargo's environment (CARGO_HOME, RUSTUP_HOME,
//! PATH) needs to survive the privilege escalation or it can't find
//! the build output.

#![cfg(target_os = "linux")]

use std::net::Ipv4Addr;

use engram_core::types::{NetworkDefault, NetworkPolicy};
use engram_core::SandboxId;
use engram_sandbox_firecracker::net::{
    host_startup, provision, tap_name_for, teardown, NetPolicy, NetworkAllocator,
};
use parking_lot::Mutex;

fn require_root() -> bool {
    // /proc/self/status's `Uid:` line is `Uid: <real> <eff> <saved>
    // <fs>` — all four are zero under sudo. No unsafe / extern needed.
    let status =
        std::fs::read_to_string("/proc/self/status").expect("read /proc/self/status");
    let line = status
        .lines()
        .find(|l| l.starts_with("Uid:"))
        .expect("Uid line in /proc/self/status");
    let euid: u32 = line
        .split_whitespace()
        .nth(2)
        .and_then(|s| s.parse().ok())
        .expect("parse euid");
    if euid != 0 {
        eprintln!(
            "SKIP: network_provision tests require root (effective uid={euid}). \
             Re-run via `sudo -E ...`."
        );
        return false;
    }
    true
}

/// Mirror of net::chain_name_for (private). Matches its rule: take
/// the first 12 hex chars of the UUID-stringified SandboxId.
fn expected_chain_name(id: SandboxId) -> String {
    let s = id.to_string();
    let prefix: String = s
        .chars()
        .filter(|c| c.is_ascii_hexdigit())
        .take(12)
        .collect();
    format!("engram-sb-{prefix}")
}

#[tokio::test]
#[ignore = "requires Linux + root (CAP_NET_ADMIN); run with sudo on the dev VM"]
async fn host_startup_is_idempotent() {
    if !require_root() {
        return;
    }
    // First call inserts the inter-VM DROP rule; second call should
    // detect it via -C and skip without erroring.
    host_startup().await.expect("first host_startup");
    host_startup().await.expect("second host_startup (should be idempotent)");
}

#[tokio::test]
#[ignore = "requires Linux + root (CAP_NET_ADMIN); run with sudo on the dev VM"]
async fn provision_then_teardown_leaves_no_trace() {
    if !require_root() {
        return;
    }
    host_startup().await.expect("host_startup");

    let pool = "10.200.0.0".parse::<Ipv4Addr>().unwrap();
    let allocator = Mutex::new(NetworkAllocator::new(pool));

    let id = SandboxId::new();
    let setup = provision(id, &allocator, NetworkPolicy::default(), NetPolicy::Enforce)
        .await
        .expect("provision");

    // TAP exists.
    let tap = tap_name_for(id);
    assert_eq!(setup.tap_name, tap);
    let ip_show = std::process::Command::new("ip")
        .args(["link", "show", &tap])
        .output()
        .expect("ip link show");
    assert!(
        ip_show.status.success(),
        "TAP {tap} not visible after provision: stderr={}",
        String::from_utf8_lossy(&ip_show.stderr),
    );

    // Per-VM iptables chain has rules.
    let chain = expected_chain_name(id);
    let iptables_save = std::process::Command::new("iptables-save")
        .output()
        .expect("iptables-save");
    let dump = String::from_utf8_lossy(&iptables_save.stdout);
    assert!(dump.contains(&chain), "expected chain {chain} in iptables-save");

    // Teardown.
    teardown(&setup, &allocator).await;

    // TAP gone.
    let ip_show2 = std::process::Command::new("ip")
        .args(["link", "show", &tap])
        .output()
        .expect("ip link show 2");
    assert!(
        !ip_show2.status.success(),
        "TAP {tap} still visible after teardown",
    );

    // Chain gone.
    let iptables_save2 = std::process::Command::new("iptables-save")
        .output()
        .expect("iptables-save 2");
    let dump2 = String::from_utf8_lossy(&iptables_save2.stdout);
    assert!(
        !dump2.contains(&chain),
        "expected chain {chain} gone from iptables-save after teardown",
    );

    // Allocator slot recycled (live_count back to zero).
    assert_eq!(allocator.lock().live_count(), 0);
}

#[tokio::test]
#[ignore = "requires Linux + root (CAP_NET_ADMIN); run with sudo on the dev VM"]
async fn provision_two_concurrent_sandboxes_get_distinct_30s() {
    if !require_root() {
        return;
    }
    host_startup().await.expect("host_startup");

    let pool = "10.200.0.0".parse::<Ipv4Addr>().unwrap();
    let allocator = Mutex::new(NetworkAllocator::new(pool));

    let a_id = SandboxId::new();
    let b_id = SandboxId::new();
    let a = provision(a_id, &allocator, NetworkPolicy::default(), NetPolicy::LogOnly)
        .await
        .expect("provision a");
    let b = provision(b_id, &allocator, NetworkPolicy::default(), NetPolicy::LogOnly)
        .await
        .expect("provision b");

    assert_ne!(
        a.vm_cidr.cidr_str(),
        b.vm_cidr.cidr_str(),
        "two concurrent sandboxes must not share a /30",
    );
    assert_ne!(a.tap_name, b.tap_name);

    teardown(&a, &allocator).await;
    teardown(&b, &allocator).await;
}

#[tokio::test]
#[ignore = "requires Linux + root (CAP_NET_ADMIN); run with sudo on the dev VM"]
async fn provision_resolves_allow_hosts_into_chain() {
    if !require_root() {
        return;
    }
    host_startup().await.expect("host_startup");

    let pool = "10.200.0.0".parse::<Ipv4Addr>().unwrap();
    let allocator = Mutex::new(NetworkAllocator::new(pool));

    let mut policy = NetworkPolicy::default();
    policy.default = NetworkDefault::Deny;
    // 1.1.1.1 resolves to itself (Cloudflare's DNS). Cheap, stable
    // hostname for the round-trip.
    policy.allow_hosts = vec!["one.one.one.one".into()];

    let id = SandboxId::new();
    let setup = provision(id, &allocator, policy, NetPolicy::Enforce)
        .await
        .expect("provision");

    let dump = String::from_utf8(
        std::process::Command::new("iptables-save")
            .output()
            .expect("iptables-save")
            .stdout,
    )
    .expect("utf8");
    // The hostname rides on the `--comment` tag; the IP appears as
    // an `-d` match. Either confirms the resolve happened.
    assert!(
        dump.contains("one.one.one.one"),
        "expected allow-host comment for one.one.one.one in iptables-save",
    );

    teardown(&setup, &allocator).await;
}
