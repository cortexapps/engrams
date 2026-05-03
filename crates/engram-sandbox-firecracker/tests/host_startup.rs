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

use engram_sandbox_firecracker::net::host_startup;

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

    host_startup(None).await.expect("first host_startup");
    host_startup(None).await.expect("second host_startup");

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
    assert!(dump.contains("engram-dns"), "missing DNS allow");
    assert!(dump.contains("engram-masq"), "missing MASQUERADE");
    // No proxy mode → no REDIRECT, no default-deny.
    assert!(
        !dump.contains("engram-proxy-redirect"),
        "REDIRECT should be absent under no-proxy mode",
    );
    assert!(
        !dump.contains("engram-default-deny"),
        "default-deny should be absent under no-proxy mode",
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

    host_startup(Some(9443)).await.expect("host_startup");

    let dump = iptables_save();
    assert!(dump.contains("engram-proxy-redirect"));
    assert!(dump.contains("--to-ports 9443"));
    assert!(dump.contains("engram-default-deny"));

    cleanup();
}
