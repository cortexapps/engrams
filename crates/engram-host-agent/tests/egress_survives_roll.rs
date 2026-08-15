//! ADR 0111: a surviving VM keeps its egress across a host-agent roll,
//! against a REAL Firecracker microVM (2026-08-03 incident, session
//! 51fc6af7).
//!
//! What this pins: the egress proxy's guest registry is process RAM
//! and dies with the host-agent pod. Generation B rebuilds it from the
//! policy generation A persisted beside its binding record — the real
//! startup sequence (`reattach_pass` → `rebuild_egress_from_policies`)
//! — with zero coordinator involvement. Before ADR 0111, this exact
//! shape stranded a healthy VM with every request refused
//! (`UnknownGuest`) until an evict/resume cycle.
//!
//! Sized to the property, not to realism: one guest, one policy, no
//! checkpoints, no sleeps.
//!
//! Gating mirrors `chain_rehydrate.rs`. Run on the dev-vm:
//!
//! ```sh
//! cargo build -p engram-agentd --target x86_64-unknown-linux-musl --release
//! eval "$(bash crates/engram-sandbox-firecracker/scripts/fetch-fc-test-artifacts.sh)"
//! cargo test -p engram-host-agent --test egress_survives_roll -- --ignored --nocapture
//! ```
// tests drive a live system; wall clock/OS entropy here is input, not a decision source (ADR 0098 D1)
#![allow(clippy::disallowed_methods)]
#![cfg(target_os = "linux")]

mod common;

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use engram_core::traits::sandbox::SandboxBackend;
use engram_core::types::sandbox::{CpuLimit, DiskLimit, MemoryLimit, SandboxSpec};
use engram_host_agent::bindings::BindingStore;
use engram_host_agent::egress::HostEgress;
use engram_host_agent::pooled_backend::PooledBackend;
use engram_rootfs_materializer::{InitInjection, Transport};
use engram_sandbox_firecracker::{
    FirecrackerBackend, FirecrackerConfig, RestoreMode, ENGRAM_AGENTD_PORT,
};

async fn spawn_egress(dir: &Path) -> HostEgress {
    let source: Arc<dyn engram_egress_proxy::CaSource> = Arc::new(
        engram_egress_proxy::LocalDiskCaSource::new(dir.join("egress-ca")),
    );
    let bind: std::net::SocketAddr = "127.0.0.1:0".parse().unwrap();
    HostEgress::spawn(
        source,
        bind,
        None,
        None,
        None,
        None,
        Arc::new(engram_egress_proxy::GuestGatewayRegistry::default()),
        // ADR 0118: no guest-port dialer — this test exercises policy replay
        // across a roll, not the app-to-app short circuit.
        None,
    )
    .await
    .expect("spawn egress")
}

#[tokio::test]
#[ignore = "requires Linux + KVM + firecracker; bakes a rootfs and boots a microVM"]
async fn survivor_keeps_egress_across_a_host_agent_roll() {
    // ---- gating (same as chain_rehydrate.rs) ----
    let kernel = match std::env::var("FC_TEST_KERNEL") {
        Ok(p) => std::path::PathBuf::from(p),
        Err(_) => {
            eprintln!("SKIP: FC_TEST_KERNEL not set");
            return;
        }
    };
    if !Path::new("/dev/kvm").exists() {
        eprintln!("SKIP: /dev/kvm not present");
        return;
    }
    for bin in ["firecracker", "mksquashfs"] {
        if std::env::var_os("PATH")
            .map(|p| !std::env::split_paths(&p).any(|d| d.join(bin).is_file()))
            .unwrap_or(true)
        {
            eprintln!("SKIP: {bin} not on PATH");
            return;
        }
    }
    let Some(busybox) = common::find_busybox() else {
        eprintln!("SKIP: no static busybox (apt install busybox-static or set BUSYBOX_STATIC)");
        return;
    };
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let agent = Path::new(&manifest_dir)
        .join("../../target/x86_64-unknown-linux-musl/release/engram-agentd");
    if !agent.exists() {
        eprintln!(
            "SKIP: musl engram-agentd not built at {} — \
             cargo build -p engram-agentd --target x86_64-unknown-linux-musl --release",
            agent.display(),
        );
        return;
    }

    // ---- 1. Bake an agentd-injected rootfs (docker-free, ADR 0080 §D) ----
    let images = tempfile::tempdir().expect("images dir");
    let work = tempfile::tempdir().expect("work dir");
    let blob: Arc<dyn engram_core::traits::BlobStorage> = Arc::new(
        engram_storage_local::LocalBlobStorage::new(work.path().join("blob")),
    );
    let chunk_store = engram_chunk_store::ChunkStore::new(blob);
    let outcome = common::bake_fixture_ext4(
        &images.path().join("rootfs.ext4"),
        &chunk_store,
        &busybox,
        Some(InitInjection {
            vsock_port: ENGRAM_AGENTD_PORT,
            transport: Transport::Vsock,
            init_script: None,
        }),
        |_tree| Ok(()),
    )
    .await;

    // ---- 2. Generation A: boot, apply + persist the policy ----
    let mut cfg = FirecrackerConfig::with_kernel(kernel);
    let staged = common::stage_agentd_bundle(&work.path().join("bundles"), &agent);
    cfg.bundle_dir = staged.bundle_dir.clone();
    cfg.net_pool = None;
    cfg.restore_mode = RestoreMode::File;
    cfg.default_boot_args = "console=ttyS0 reboot=k panic=1 pci=off init=/sbin/engram-init".into();

    let egress_a = Arc::new(spawn_egress(work.path()).await);
    let pooled_a = Arc::new(
        PooledBackend::new(
            Arc::new(FirecrackerBackend::new(work.path(), cfg.clone())) as Arc<dyn SandboxBackend>
        )
        .with_egress(egress_a.clone()),
    );
    pooled_a.set_self_ref(&pooled_a);

    let spec = SandboxSpec {
        image: "engram-egress-roll-test".into(),
        rootfs_source: Some(outcome.rootfs_path),
        image_uri: None,
        rootfs_manifest: None,
        cpu: CpuLimit { vcpus: 1 },
        memory: MemoryLimit { max_mib: 256 },
        disk: DiskLimit { max_gib: 1 },
        ttl: None,
        env: HashMap::new(),
        workdir: None,
        network: Default::default(),
        aux_ro_drives: vec![staged.agentd_slot()],
        swap_mib: None,
    };
    let sandbox = pooled_a.create(spec).await.expect("create");

    let session_id = engram_core::SessionId::new();
    let guest_ip = std::net::Ipv4Addr::new(10, 200, 0, 2);
    let policy = engram_core::types::egress::SessionEgressPolicy {
        session_id,
        sandbox_id: sandbox,
        guest_ip,
        network_allow_hosts: vec!["api.anthropic.com".into()],
        network_allow_host_patterns: Vec::new(),
        allow_all: false,
        secrets: Vec::new(),
        injects: Vec::new(),
        observes: Vec::new(),
        guest_services: Vec::new(),
        tunnels: Vec::new(),
        apps: Vec::new(),
        secret_mode: engram_core::types::image::SecretMode::Broker,
    };
    pooled_a
        .notify_session_policy(policy.clone())
        .await
        .expect("gen A applies the policy");
    assert!(egress_a.registry.lookup(guest_ip).is_some());
    // The start_agent path's persist (ADR 0111): policy on disk before
    // the ack.
    let bindings_dir = work.path().join("bindings");
    BindingStore::open(&bindings_dir)
        .expect("open bindings")
        .store_policy(&policy)
        .expect("persist policy");

    // ---- 3. The roll: generation A is "dead" (no destructors — a
    // real pod death). Generation B reattaches the survivor and
    // rebuilds egress from the persisted policy: the real startup
    // sequence. ----
    let _gen_a_dead = pooled_a;
    let inner_b = Arc::new(FirecrackerBackend::new(work.path(), cfg.clone()));
    let report = engram_host_agent::live_attach::reattach_pass(work.path(), &inner_b)
        .await
        .expect("reattach pass");
    assert!(
        report.reattached.iter().any(|o| matches!(
            o,
            engram_host_agent::live_attach::ReattachOutcome::Reattached { sandbox_id, .. }
                if *sandbox_id == sandbox.to_string()
        )),
        "generation B must reattach the still-live VM",
    );
    let egress_b = Arc::new(spawn_egress(work.path()).await);
    assert!(
        egress_b.registry.lookup(guest_ip).is_none(),
        "pre-condition: the restarted registry is empty (the incident state)",
    );
    let pooled_b = Arc::new(
        PooledBackend::new(inner_b as Arc<dyn SandboxBackend>).with_egress(egress_b.clone()),
    );
    pooled_b.set_self_ref(&pooled_b);
    let policies = BindingStore::open(&bindings_dir)
        .expect("reopen bindings")
        .list_policies()
        .expect("list policies");
    pooled_b.rebuild_egress_from_policies(policies).await;

    // ---- 4. The survivor's guest resolves with the same allow-list ----
    let state = egress_b
        .registry
        .lookup(guest_ip)
        .expect("survivor guest must resolve after the rebuild");
    assert_eq!(state.session_id, session_id);
    assert!(state.network_allow.matches("api.anthropic.com"));
    assert!(!state.network_allow.matches("evil.example.com"));

    pooled_b.destroy(sandbox).await.expect("destroy");
}
