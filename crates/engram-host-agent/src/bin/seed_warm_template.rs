//! ADR 0014 e2e validation helper — produces a real template
//! snapshot in the host-agent's BlobStorage so warm-pool refill
//! can actually succeed.
//!
//! Drop-in for the missing "bake → templates row" glue: spin up
//! FirecrackerBackend + PooledBackend pointing at the same
//! BlobStorage dir + chunk store the running host-agent uses,
//! snapshot a microVM through the production code path, and
//! print the resulting snapshot metadata to stdout for hand-
//! injection into `templates`.
//!
//! Run on the dev VM (after sourcing fetch-fc-test-artifacts.sh):
//!
//! ```sh
//! sudo -E \
//!   ENGRAM_BLOB_DIR=/tmp/engram-host-blob/blobs \
//!   ENGRAM_KERNEL=$HOME/.cache/engram-fc-test/vmlinux-5.10.223 \
//!   ENGRAM_ROOTFS=$HOME/.cache/engram-fc-test/ubuntu-22.04.ext4 \
//!   ./target/release/seed_warm_template
//! ```

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use engram_chunk_store::ChunkStore;
use engram_core::traits::storage::BlobStorage;
use engram_core::traits::SandboxBackend;
use engram_core::types::sandbox::{CpuLimit, DiskLimit, MemoryLimit, SandboxSpec};
use engram_host_agent::pooled_backend::PooledBackend;
use engram_sandbox_firecracker::{FirecrackerBackend, FirecrackerConfig};
use engram_storage_local::LocalBlobStorage;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let blob_dir =
        PathBuf::from(std::env::var("ENGRAM_BLOB_DIR").expect("ENGRAM_BLOB_DIR must be set"));
    let kernel = PathBuf::from(std::env::var("ENGRAM_KERNEL").expect("ENGRAM_KERNEL must be set"));
    let rootfs_src =
        PathBuf::from(std::env::var("ENGRAM_ROOTFS").expect("ENGRAM_ROOTFS must be set"));
    // The host-agent's work_dir. Critical that this matches what
    // the running host-agent uses, because FC's state.bin embeds
    // the rootfs path (under work_dir) verbatim and the receiver
    // resolves it on its own filesystem. ADR 0014's canonical
    // path contract: same work_dir fleet-wide.
    let work_dir =
        PathBuf::from(std::env::var("ENGRAM_WORK_DIR").expect("ENGRAM_WORK_DIR must be set"));
    // Stable rootfs path that survives this binary's exit AND is
    // present on the host-agent's filesystem. The host-agent
    // restores the canonical symlink at
    // `<work_dir>/rootfs/<source_sandbox_id>.dev → <rootfs_target>`,
    // so this target must exist when FC reopens it.
    let template_rootfs = PathBuf::from(
        std::env::var("ENGRAM_TEMPLATE_ROOTFS")
            .unwrap_or_else(|_| "/tmp/engram-templates/warm-pool-seed.ext4".into()),
    );
    eprintln!("template_rootfs target: {}", template_rootfs.display());

    // Use the running host-agent's BlobStorage so the snapshot lands
    // where its WarmPool will look on refill.
    let blob: Arc<dyn BlobStorage> = Arc::new(LocalBlobStorage::new(&blob_dir));
    let chunk_store = ChunkStore::new(blob);

    // Set up FC + PooledBackend identically to host-agent's wiring.
    // Use the host-agent's work_dir directly — state.bin will embed
    // paths under it, and those paths must resolve on the
    // host-agent's filesystem at restore time.
    tokio::fs::create_dir_all(&work_dir).await?;
    tokio::fs::create_dir_all(template_rootfs.parent().unwrap()).await?;
    if !template_rootfs.exists() {
        let bytes = tokio::fs::copy(&rootfs_src, &template_rootfs).await?;
        eprintln!("copied {} bytes to {}", bytes, template_rootfs.display());
    } else {
        eprintln!(
            "template_rootfs already exists at {}",
            template_rootfs.display()
        );
    }
    // Verify it's actually there before we hand the path to FC.
    let meta = tokio::fs::metadata(&template_rootfs).await?;
    eprintln!("template_rootfs size: {} bytes", meta.len());

    let mut cfg = FirecrackerConfig::with_kernel(kernel);
    cfg.net_pool = None;
    cfg.default_boot_args = "console=ttyS0 reboot=k panic=1 pci=off init=/bin/bash".into();
    let fc = Arc::new(FirecrackerBackend::new(&work_dir, cfg));
    let pooled = PooledBackend::new(fc.clone() as Arc<dyn SandboxBackend>)
        .with_chunk_store(chunk_store, work_dir.join("materialized"));

    let spec = SandboxSpec {
        image: "warm-pool-seed".into(),
        rootfs_source: Some(template_rootfs.clone()),
        image_uri: None,
        harness_pack_uri: None,
        cpu: CpuLimit { vcpus: 1 },
        memory: MemoryLimit { max_mib: 128 },
        disk: DiskLimit { max_gib: 1 },
        ttl: None,
        env: Default::default(),
        workdir: None,
        harness_substrate: None,
        network: Default::default(),
        canonical_memory_manifest: None,
    };

    eprintln!("creating sandbox...");
    let sandbox_id = pooled.create(spec).await?;
    eprintln!("sandbox created: {sandbox_id}; settling for 3s before snapshot");
    tokio::time::sleep(Duration::from_secs(3)).await;

    eprintln!("snapshotting (uploads state.bin + sidecar + memory chunks)...");
    let metadata = pooled.snapshot(sandbox_id).await?;
    pooled.destroy(sandbox_id).await?;

    // Print the metadata as SQL-friendly tuples so the operator can
    // copy-paste them into psql.
    println!();
    println!("=== seed result ===");
    println!("snapshot_id:        {}", metadata.id);
    println!("source_sandbox_id:  {:?}", metadata.source_sandbox_id);
    println!("state_blob_key:     {:?}", metadata.state_blob_key);
    println!("sidecar_blob_key:   {:?}", metadata.sidecar_blob_key);
    println!("memory_manifest:    {:?}", metadata.memory_manifest);
    println!("size_bytes:         {}", metadata.size_bytes);
    println!();
    println!("=== SQL to register as a template ===");
    println!(
        "INSERT INTO sessions (id, status, harness, image_uri, last_active_at) VALUES \
         ('aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa', 'completed', '{{\"kind\":\"none\"}}'::jsonb, \
         'warm-pool-seed', NOW()) ON CONFLICT DO NOTHING;"
    );
    println!(
        "INSERT INTO snapshots (id, session_id, image_version, size_bytes, created_at, \
         last_accessed_at, recoverable) VALUES \
         ('{}', 'aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa', 'warm-pool-seed', {}, NOW(), NOW(), true) \
         ON CONFLICT DO NOTHING;",
        metadata.id, metadata.size_bytes,
    );
    println!(
        "INSERT INTO templates (template_ref, image_repo, image_tag, harness_pack_uri, \
         snapshot_id, vcpus, memory_mib) VALUES \
         ('cccccccc-cccc-cccc-cccc-cccccccccccc', 'warm-pool-seed', 'v1', 'none', \
         '{}', 1, 128) ON CONFLICT DO NOTHING;",
        metadata.id,
    );

    Ok(())
}
