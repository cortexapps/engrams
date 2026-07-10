//! Live test of the new (post-proxy) `host_startup` ruleset.
//!
//! Replaces the retired `network_provision.rs` — that one tested the
//! per-VM iptables chain we deleted in favour of proxy enforcement.
//! What we still need to verify on real iptables:
//!
//!   - host_startup is idempotent (re-applying doesn't double rules)
//!   - the ruleset actually appears in iptables-save with the
//!     expected comment tags
//!   - proxy mode (Some(port)) adds the REDIRECT to the right port
//!     and the default-deny FORWARD; bypass mode (None) does NOT
//!     add either
//!
//! Run with sudo on the dev VM:
//!
//!   sudo -E env "PATH=$PATH" cargo test \
//!       -p engram-sandbox-firecracker --test host_startup -- --ignored

#![cfg(target_os = "linux")]

use engram_core::SandboxId;
use engram_sandbox_firecracker::net::{
    host_startup, netns_name_for, provision_netns, tap_name_for, teardown_netns, NetworkAllocator,
    VmCidr,
};
use parking_lot::Mutex;

fn require_root() -> bool {
    let status = std::fs::read_to_string("/proc/self/status").expect("read /proc/self/status");
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
            "SKIP: host_startup tests require root (effective uid={euid}). \
             Re-run via `sudo -E ...`."
        );
        return false;
    }
    true
}

fn iptables_save() -> String {
    String::from_utf8(
        std::process::Command::new("iptables-save")
            .output()
            .expect("iptables-save")
            .stdout,
    )
    .expect("utf8")
}

/// Wipe every rule we manage so each test starts clean. Walk
/// iptables-save: each `*<table>` header sets the current table;
/// each `-A <chain> ...` line that mentions an engram comment tag
/// gets re-issued as `iptables -t <table> -D <chain> ...`.
fn cleanup() {
    let saved = iptables_save();
    let mut current_table = "filter".to_string();
    for line in saved.lines() {
        if let Some(t) = line.strip_prefix('*') {
            current_table = t.trim().to_string();
            continue;
        }
        if !line.contains("engram-") {
            continue;
        }
        // `-A FORWARD -s ... -j DROP ...` → `-D FORWARD -s ... -j DROP ...`
        let Some(rest) = line.strip_prefix("-A ") else {
            continue;
        };
        let mut argv = vec!["-t".to_string(), current_table.clone(), "-D".to_string()];
        argv.extend(rest.split_whitespace().map(str::to_string));
        let _ = std::process::Command::new("iptables").args(&argv).output();
    }
}

#[tokio::test]
#[ignore = "requires Linux + root (CAP_NET_ADMIN); run with sudo on the dev VM"]
async fn host_startup_no_proxy_is_idempotent_and_lacks_redirect() {
    if !require_root() {
        return;
    }
    cleanup();

    host_startup(None, None, None)
        .await
        .expect("first host_startup");
    host_startup(None, None, None)
        .await
        .expect("second host_startup");

    let dump = iptables_save();
    // Hard-isolation rules present.
    assert!(
        dump.contains("engram-isolate-vm-vm"),
        "missing inter-VM block"
    );
    assert!(dump.contains("engram-host-lan"), "missing host-LAN drops");
    assert!(
        dump.contains("engram-host-input"),
        "missing host-INPUT drop"
    );
    assert!(dump.contains("engram-masq"), "missing MASQUERADE");
    // issue #240: the no-proxy lane closed the DNS-exfil hatch. There is
    // no longer a public-resolver ACCEPT (the legacy `engram-dns` rule),
    // and instead the lane applies the default-deny so no guest is ever
    // left with an unfiltered egress route.
    assert!(
        !dump.contains("engram-dns"),
        "no-proxy lane must not install a public-resolver DNS allow (DNS-exfil hatch closed)",
    );
    assert!(
        dump.contains("engram-default-deny"),
        "no-proxy lane must still close egress with the default-deny FORWARD DROP",
    );
    // No proxy mode → no REDIRECT (those belong to proxy mode only).
    assert!(
        !dump.contains("engram-proxy-redirect"),
        "REDIRECT should be absent under no-proxy mode",
    );
    assert!(
        !dump.contains("engram-dns-redirect"),
        "DNS REDIRECT should be absent under no-proxy mode",
    );

    cleanup();
}

#[tokio::test]
#[ignore = "requires Linux + root (CAP_NET_ADMIN); run with sudo on the dev VM"]
async fn host_startup_with_proxy_adds_redirect_and_default_deny() {
    if !require_root() {
        return;
    }
    cleanup();

    host_startup(Some(9443), None, None)
        .await
        .expect("host_startup");

    let dump = iptables_save();
    assert!(dump.contains("engram-proxy-redirect"));
    assert!(dump.contains("--to-ports 9443"));
    assert!(dump.contains("engram-default-deny"));
    // Both interface families must be present: tap-engr-+ for
    // the cold-create path (TAP in root netns) and vh-engr-+ for
    // the warm-restore path (TAP in per-VM netns, packet arrives
    // at root via the host-side veth). Pre-fix only tap-engr-+
    // existed and every warm restore silently bypassed the proxy
    // (prod 2026-05-20).
    assert!(
        dump.contains("-i tap-engr-+"),
        "cold-create REDIRECT must match the TAP; dump=\n{dump}",
    );
    assert!(
        dump.contains("-i vh-engr-+"),
        "warm-restore REDIRECT must match the host-side veth; dump=\n{dump}",
    );

    cleanup();
}

/// ADR 0014 issue #6 follow-up. The new ESTABLISHED,RELATED ACCEPT
/// must land in iptables-save with the conntrack module loaded,
/// matching the pool source, and ordered before the engram-host-input
/// DROP. Without ordering, host-initiated TCP flows (proxy_shell ↔
/// ttyd) lose their return SYN+ACK to the blanket DROP and time out
/// — exactly the prod failure observed on session 379abfec.
///
/// This is the live-iptables counterpart to the
/// `host_startup_accepts_established_input_before_drop` unit test in
/// src/net.rs — that one asserts the rule string is generated; this
/// one asserts the kernel actually installed it.
#[tokio::test]
#[ignore = "requires Linux + root (CAP_NET_ADMIN); run with sudo on the dev VM"]
async fn host_startup_installs_established_accept_before_host_input_drop() {
    if !require_root() {
        return;
    }
    cleanup();

    host_startup(Some(9443), Some(5353), None)
        .await
        .expect("host_startup");

    let dump = iptables_save();
    // The new rule lands with its comment tag and the conntrack match.
    let est_line = dump
        .lines()
        .find(|l| l.contains("engram-host-input-established"))
        .expect("ESTABLISHED ACCEPT rule must be present after host_startup");
    assert!(
        est_line.contains("ESTABLISHED,RELATED") || est_line.contains("RELATED,ESTABLISHED"),
        "rule must use conntrack ESTABLISHED,RELATED state match; got: {est_line}"
    );
    assert!(
        est_line.contains("ACCEPT"),
        "rule must ACCEPT (not DROP); got: {est_line}"
    );

    // Ordering: ESTABLISHED ACCEPT before the blanket
    // engram-host-input DROP in the filter table. iptables-save emits
    // rules in chain order, so find the indices of the two lines and
    // assert the ACCEPT precedes the DROP.
    let est_idx = dump
        .lines()
        .position(|l| l.contains("engram-host-input-established"))
        .expect("established line position");
    let drop_idx = dump
        .lines()
        .position(|l| {
            (l.contains("comment engram-host-input ") || l.ends_with("comment engram-host-input"))
                && l.contains("DROP")
        })
        .expect("host-input DROP line position");
    assert!(
        est_idx < drop_idx,
        "ESTABLISHED,RELATED ACCEPT must precede engram-host-input DROP \
         in iptables-save; ACCEPT idx={est_idx}, DROP idx={drop_idx}",
    );

    cleanup();
}

/// ADR 0019 / #526 phase 2: the guest→collector OTLP pinhole actually
/// installs (live-iptables counterpart to the
/// `host_startup_guest_otlp_pinhole_is_scoped_and_precedes_drop` unit
/// test in src/net.rs). Deliberately minimal per the repo CI-time rule:
/// installs the ruleset once with an otel port and asserts presence,
/// scope, and ordering — no guest OTLP round trip (the collector-side
/// half is deploy-layer, not net.rs's property).
#[tokio::test]
#[ignore = "requires Linux + root (CAP_NET_ADMIN); run with sudo on the dev VM"]
async fn host_startup_installs_guest_otlp_pinhole_before_host_input_drop() {
    if !require_root() {
        return;
    }
    cleanup();

    host_startup(Some(9443), Some(5353), Some(4317))
        .await
        .expect("host_startup");

    let dump = iptables_save();
    let otlp_line = dump
        .lines()
        .find(|l| l.contains("engram-guest-otlp-input"))
        .expect("guest-otlp ACCEPT rule must be present after host_startup");
    assert!(
        otlp_line.contains("ACCEPT")
            && otlp_line.contains("--dport 4317")
            && otlp_line.contains("10.200.0.0/16"),
        "pinhole must ACCEPT exactly tcp/4317 from the VM pool; got: {otlp_line}"
    );

    let otlp_idx = dump
        .lines()
        .position(|l| l.contains("engram-guest-otlp-input"))
        .expect("otlp line position");
    let drop_idx = dump
        .lines()
        .position(|l| {
            (l.contains("comment engram-host-input ") || l.ends_with("comment engram-host-input"))
                && l.contains("DROP")
        })
        .expect("host-input DROP line position");
    assert!(
        otlp_idx < drop_idx,
        "guest-otlp ACCEPT must precede engram-host-input DROP in \
         iptables-save; ACCEPT idx={otlp_idx}, DROP idx={drop_idx}",
    );

    // Idempotency: re-applying must not double the pinhole.
    host_startup(Some(9443), Some(5353), Some(4317))
        .await
        .expect("second host_startup");
    let dump2 = iptables_save();
    assert_eq!(
        dump2
            .lines()
            .filter(|l| l.contains("engram-guest-otlp-input"))
            .count(),
        1,
        "re-applying host_startup must not duplicate the otlp pinhole",
    );

    cleanup();
}

/// Regression for the prod 2026-05-20 blackhole: post-M1.16 the per-VM
/// netns moves the TAP off host root, so REDIRECT must match
/// `vh-engr-+` (the host-side veth end) to catch warm-restored VM
/// traffic. Pre-fix only `tap-engr-+` was matched and the warm-path
/// REDIRECT never fired — packets fell through to the default-deny
/// FORWARD DROP and harness API calls hung silently.
///
/// What this test actually exercises:
///   1. host_startup installs the proxy rules (both interface families).
///   2. Provision a per-VM netns the same way warm-restore does
///      (provision_netns → TAP in netns + veth pair + SNAT POSTROUTING).
///   3. Bind a TCP listener on the proxy port in host root.
///   4. From inside the netns, attempt to connect to a random
///      "internet" IP on tcp/443. The packet must arrive at the
///      listener via REDIRECT — if it doesn't, the test times out
///      (which is exactly the prod failure mode).
#[tokio::test]
#[ignore = "requires Linux + root (CAP_NET_ADMIN + netns); run with sudo on the dev VM"]
async fn host_startup_redirects_warm_path_via_vh_engr() {
    use std::time::Duration;
    use tokio::io::AsyncReadExt;
    use tokio::net::TcpListener;

    if !require_root() {
        return;
    }
    cleanup();

    // Use an unusual proxy port so we can be sure any accept on it
    // came from our REDIRECT, not some other process.
    let proxy_port: u16 = 28443;

    host_startup(Some(proxy_port), Some(5353), None)
        .await
        .expect("host_startup");

    // Listener in host root, accepts the redirected SYN.
    let listener = TcpListener::bind(("0.0.0.0", proxy_port))
        .await
        .expect("bind proxy listener");

    // Provision the netns + veth pair + SNAT (warm-restore shape).
    let sandbox_id = SandboxId::new();
    let bake_cidr = VmCidr::new("10.200.0.0".parse().unwrap());
    let tap_name = tap_name_for(sandbox_id);
    // The allocator's first usable slot is /30 #1 (slot 0 is reserved
    // for the bake CIDR, see net.rs::NetworkAllocator::new).
    let allocator = Mutex::new(NetworkAllocator::new("10.200.0.0".parse().unwrap()));
    let setup = provision_netns(sandbox_id, bake_cidr, &tap_name, &allocator)
        .await
        .expect("provision_netns");

    // From inside the netns, attempt to dial a non-existent
    // "internet" target on tcp/443. The IP doesn't matter — the
    // REDIRECT catches every dport=443. We use a TEST-NET range
    // (RFC 5737) so no real host owns the address.
    //
    // bash's /dev/tcp/<ip>/<port> is the smallest dependency for a
    // TCP SYN we have in nix's coreutils set; busybox-nc isn't
    // guaranteed and tools/nc bring extra footprint.
    let netns = netns_name_for(sandbox_id);
    let dialer = tokio::process::Command::new("ip")
        .args([
            "netns",
            "exec",
            &netns,
            "bash",
            "-c",
            "exec 3<>/dev/tcp/192.0.2.42/443 && head -c 0 <&3",
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn ip netns exec");

    // Wait up to 5s for the listener to accept (REDIRECT is
    // instantaneous; the dial inside the netns races us by tens of
    // ms). Pre-fix this hangs forever.
    let accept_res = tokio::time::timeout(Duration::from_secs(5), listener.accept()).await;

    // Clean up first so the kernel state doesn't leak into other
    // tests, then assert on the result.
    let _ = dialer.id().map(|pid| {
        let _ = std::process::Command::new("kill")
            .arg(pid.to_string())
            .output();
    });
    teardown_netns(&setup, &allocator).await;

    let (mut stream, peer) = accept_res
        .expect(
            "REDIRECT did not deliver the netns dial to the proxy port within 5s \
             — this is the prod 2026-05-20 blackhole. Check that host_startup_lines \
             emits a `-i vh-engr-+` rule and that provision_netns puts the host \
             side of the veth in root.",
        )
        .expect("listener accept");

    // The peer's source IP should be in our SNAT pool (the netns
    // POSTROUTING SNAT rewrote it from 10.200.0.2 → snat_cidr.guest()).
    let peer_ip = match peer {
        std::net::SocketAddr::V4(v4) => *v4.ip(),
        std::net::SocketAddr::V6(_) => panic!("expected IPv4 peer"),
    };
    assert_eq!(
        peer_ip,
        setup.snat_cidr.guest(),
        "REDIRECTed connection's source IP must be the netns SNAT slot, \
         confirming the packet traversed the netns→veth→REDIRECT path",
    );

    // Drain whatever the dialer wrote (probably nothing — bash's
    // `head -c 0` exits immediately after the SYN/ACK handshake).
    let mut _buf = [0u8; 64];
    let _ = tokio::time::timeout(Duration::from_millis(100), stream.read(&mut _buf)).await;

    cleanup();
}
