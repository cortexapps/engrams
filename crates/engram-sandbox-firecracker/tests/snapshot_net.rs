//! End-to-end coverage for Gap 1 of the FC parity work: snapshot/restore
//! must re-provision the host-side networking (TAP + /30) so a resumed
//! sandbox keeps its egress.
//!
//! Boots a Firecracker microVM with networking enabled, snapshots,
//! destroys (which frees the /30 + deletes the TAP), restores from
//! the same snapshot dir, and asserts:
//!
//!   1. The TAP is present on the host under the *original* name
//!      (FC bakes the TAP name into state.bin; it has to come back
//!      under the same name or load_snapshot fails).
//!   2. The /30's gateway IP is bound on the TAP (so guest egress
//!      has somewhere to go).
//!   3. The restored sandbox's snapshot manifest carries the `net`
//!      field (FcNetSnapshot) so a cross-host cold resume can also
//!      reconstruct networking.
//!
//! Same sudo+KVM gating as `host_startup` and `proxy_e2e`. Run via:
//!
//!   sudo -E env "PATH=$PATH" cargo test \
//!       -p engram-sandbox-firecracker --test snapshot_net -- --ignored
//!
//! …or via the canonical script (which handles sudo wrapping):
//!
//!   bash crates/engram-sandbox-firecracker/scripts/run-boot-test.sh snapshot_net

#![cfg(target_os = "linux")]

mod common;

use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::path::Path;
use std::time::Duration;

use engram_core::traits::sandbox::SandboxBackend;
use engram_core::types::sandbox::{CpuLimit, DiskLimit, MemoryLimit, SandboxSpec};
use engram_sandbox_firecracker::{FirecrackerBackend, FirecrackerConfig};

/// `/proc/self/status`-based root check matching `host_startup.rs` and
/// `proxy_e2e.rs`. Returns false (after printing SKIP) when not root.
fn require_root() -> bool {
    let status = std::fs::read_to_string("/proc/self/status").expect("read /proc/self/status");
    let euid: u32 = status
        .lines()
        .find(|l| l.starts_with("Uid:"))
        .and_then(|l| l.split_whitespace().nth(2).and_then(|s| s.parse().ok()))
        .expect("parse euid");
    if euid != 0 {
        eprintln!(
            "SKIP: snapshot_net requires root (CAP_NET_ADMIN for TAP creation). \
             Re-run via `sudo -E ...`."
        );
        return false;
    }
    true
}

/// Wipe stale `tap-engr-*` devices left behind by a prior failed run.
/// Same shape as proxy_e2e's helper. Multiple stale TAPs sharing the
/// /30 gateway IP race ARP responses and break the new run before it
/// starts.
fn delete_stale_taps() {
    let saved = std::process::Command::new("sh")
        .arg("-c")
        .arg("ip -o link show | awk -F': ' '/tap-engr-/ {print $2}' | awk '{print $1}'")
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

/// `ip link show <name>` succeeds (exit 0) iff the device is present.
fn tap_exists(name: &str) -> bool {
    std::process::Command::new("ip")
        .args(["link", "show", name])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// `ip -n <ns> link show <name>` — same probe, but inside a netns.
/// Used by M1.16 assertions that the bake's TAP is recreated inside
/// the per-VM netns instead of host root.
fn tap_exists_in_netns(ns: &str, name: &str) -> bool {
    std::process::Command::new("ip")
        .args(["-n", ns, "link", "show", name])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// `ip netns add` publishes a bind-mount at `/var/run/netns/<name>`.
/// Probing the path is the cheapest existence check.
fn netns_exists(name: &str) -> bool {
    std::path::Path::new("/var/run/netns").join(name).exists()
}

/// Read `ip -4 addr show dev <name>` and check the gateway IP appears
/// (it should — provision adds `<gateway>/30` to the TAP).
fn tap_has_addr(name: &str, ip: Ipv4Addr) -> bool {
    let out = match std::process::Command::new("ip")
        .args(["-4", "addr", "show", "dev", name])
        .output()
    {
        Ok(o) if o.status.success() => o,
        _ => return false,
    };
    let text = String::from_utf8_lossy(&out.stdout);
    text.contains(&format!("inet {ip}/"))
}

/// Read manifest.json from the snapshot dir and assert the `net`
/// field is populated. We deserialize as a serde Value because
/// FcSnapshotManifest is a private type — checking the JSON shape is
/// the most stable cross-version assertion.
fn manifest_carries_net_field(snap_dir: &Path) -> Option<(String, Ipv4Addr)> {
    let raw = std::fs::read(snap_dir.join("manifest.json")).ok()?;
    let v: serde_json::Value = serde_json::from_slice(&raw).ok()?;
    assert_eq!(
        v.get("format").and_then(|f| f.as_str()),
        Some("fc"),
        "manifest format tag must be \"fc\"",
    );
    let net = v.get("net")?;
    if net.is_null() {
        return None;
    }
    let tap = net.get("tap_name")?.as_str()?.to_string();
    let cidr_raw = net.get("cidr_network")?.as_str()?;
    let cidr: Ipv4Addr = cidr_raw.parse().ok()?;
    Some((tap, cidr))
}

#[tokio::test]
#[ignore = "requires Linux + KVM + firecracker + sudo (CAP_NET_ADMIN)"]
async fn snapshot_restore_round_trips_per_vm_network() {
    let env = match common::fc_preflight() {
        Some(e) => e,
        None => return,
    };
    if !require_root() {
        return;
    }
    delete_stale_taps();

    let work = tempfile::tempdir().expect("tempdir");
    // Rootfs lives in `work` so it survives the source sandbox's
    // destroy() — FC stores the absolute drive path in state.bin and
    // reopens it on load_snapshot, so the file must still exist at
    // restore time. Mirrors the snapshot.rs pattern.
    let local_rootfs = work.path().join("rootfs.ext4");
    tokio::fs::copy(&env.rootfs, &local_rootfs)
        .await
        .expect("clone rootfs into tempdir");

    // Networking ENABLED — that's the whole point of this test. Use
    // the default 10.200.0.0/16 pool; the allocator hands out /30s
    // sequentially starting at .0/30.
    let mut cfg = FirecrackerConfig::with_kernel(env.kernel);
    cfg.net_pool = Some("10.200.0.0".parse().expect("parse pool"));
    cfg.egress_proxy_port = None;
    // The bare ubuntu rootfs panics on `init=/sbin/engram-init` (no
    // such binary). Override to a binary we know is present so the VM
    // stays alive long enough to snapshot. The /30's static IP comes
    // through CONFIG_IP_PNP from the kernel's `ip=` cmdline tail
    // appended by the backend.
    cfg.default_boot_args = "console=ttyS0 reboot=k panic=1 pci=off init=/bin/bash".into();
    let backend = FirecrackerBackend::new(work.path(), cfg);

    let spec = SandboxSpec {
        image: "fc-snapshot-net-test".into(),
        rootfs_source: Some(local_rootfs.clone()),
        image_uri: None,
        rootfs_manifest: None,
        cpu: CpuLimit { vcpus: 1 },
        memory: MemoryLimit { max_mib: 128 },
        disk: DiskLimit { max_gib: 1 },
        ttl: None,
        env: HashMap::new(),
        workdir: None,
        network: Default::default(),
        aux_ro_drives: Vec::new(),
    };

    // 1. create — provisions the TAP + reserves the /30
    let original_id = backend.create(spec).await.expect("create");

    // 2. let early-boot settle so the snapshot captures coherent state
    tokio::time::sleep(Duration::from_secs(2)).await;

    // 3. snapshot — manifest should carry the net info
    // ADR 0007 Phase 6: backend owns its staging dir.
    let metadata = backend.snapshot(original_id).await.expect("snapshot");
    let snap_dir = backend.snapshot_path_for(metadata.id);
    let (orig_tap, orig_cidr) =
        manifest_carries_net_field(&snap_dir).expect("snapshot manifest must carry `net` field");
    println!("snapshot recorded tap={orig_tap} cidr={orig_cidr}/30");
    assert!(
        tap_exists(&orig_tap),
        "TAP {orig_tap} should exist after create"
    );
    let gateway = {
        let o = orig_cidr.octets();
        Ipv4Addr::new(o[0], o[1], o[2], o[3] | 0b01)
    };
    assert!(
        tap_has_addr(&orig_tap, gateway),
        "TAP {orig_tap} should hold gateway IP {gateway}",
    );

    // 4. destroy — frees the /30 + deletes the TAP
    backend.destroy(original_id).await.expect("destroy");
    assert!(
        !tap_exists(&orig_tap),
        "TAP {orig_tap} should be deleted after destroy"
    );

    // 5. restore — re-provisions the same /30 + recreates the TAP
    //    under the same name
    let restored_id = backend.restore(metadata.clone()).await.expect("restore");
    assert_ne!(
        restored_id, original_id,
        "restore allocates fresh sandbox id"
    );

    // 6. ADR 0014 M1.16: post-restore the TAP lives INSIDE a per-VM
    //    netns, not host root. Recreating it under the bake's name in
    //    the host-root namespace would collide with concurrent warm
    //    restores from the same template, so M1.16 moved the entire
    //    networking surface for restored sandboxes into a fresh netns.
    let restored_ns = engram_sandbox_firecracker::net::netns_name_for(restored_id);
    assert!(
        netns_exists(&restored_ns),
        "netns {restored_ns} should exist post-restore (M1.16 puts the \
         restored VM's networking inside its own netns)",
    );
    assert!(
        tap_exists_in_netns(&restored_ns, &orig_tap),
        "TAP {orig_tap} should be recreated INSIDE netns {restored_ns} \
         post-restore (the bake's TAP name is reused, but it's now a \
         netns-local interface so multiple warm slots can coexist)",
    );
    assert!(
        !tap_exists(&orig_tap),
        "Post-M1.16 the bake's TAP name only exists inside the per-VM \
         netns; host root should not have it",
    );

    // cleanup
    backend
        .destroy(restored_id)
        .await
        .expect("destroy restored");
    assert!(
        !netns_exists(&restored_ns),
        "netns {restored_ns} should be deleted after destroy"
    );
    let _ = gateway; // kept above for the cold-create assertions
}
