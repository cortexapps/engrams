//! A resume whose memory image the host refuses must recover the
//! session from its disk, never declare it dead.
//!
//! The production case: a memory snapshot captured before the swap
//! drive was a chunked disk carries no swap manifest, and every host
//! refuses to restore it. The disk is intact, so the resume verb must
//! fall back to a disk-only cold boot on the newest disk lineage (the
//! live manifest when it is ahead of the snapshot's disk). In-RAM
//! context is lost; the session is not.

use engram_core::traits::BlobStorage as _;
use engram_core::types::manifest::ManifestRef;
use engram_core::types::session::SessionState;
use engram_dst::{Profile, Sim, Step};

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("current-thread runtime")
}

#[test]
fn refused_memory_image_resumes_by_disk_only_cold_boot() {
    rt().block_on(async {
        tokio::time::pause();
        let mut sim = Sim::new(23, Profile::Calm).with_faithful_hosts();

        sim.execute(Step::HostHeartbeats).await;
        sim.execute(Step::CreateSession).await;
        let sid = sim
            .world
            .meta
            .with_db(|db| {
                db.sessions
                    .values()
                    .find(|r| r.session.status == SessionState::Active)
                    .map(|r| r.session.id)
            })
            .expect("one Active session after create");
        for h in 0..sim.world.host_ids.len() {
            sim.execute(Step::HostCheckpoint(h)).await;
        }
        // The sim checkpoint records a manifest-less row. Give the newest
        // one the shape of a production memory snapshot, backed by real
        // blobs, so the resume's artifact checks pass and the host's
        // refusal is the only obstacle.
        let snapshot_disk = ManifestRef {
            manifest_id: uuid::Uuid::from_u128(0xd15c),
            version: 7,
        };
        let memory = ManifestRef {
            manifest_id: uuid::Uuid::from_u128(0x3e3),
            version: 1,
        };
        let snapshot_id = sim
            .world
            .meta
            .with_db_mut(|db| {
                let snap = db
                    .snapshots
                    .values_mut()
                    .filter(|s| s.session_id == Some(sid))
                    .max_by_key(|s| s.created_at)?;
                snap.disk_manifest = Some(snapshot_disk);
                snap.memory_manifest = Some(memory);
                Some(snap.id)
            })
            .expect("the checkpoint recorded a snapshot row");
        let blob = sim.world.host_world.blob();
        for key in [
            snapshot_disk.storage_key(),
            memory.storage_key(),
            engram_chunk_store::snapshot_blob::state_blob_key(snapshot_id),
            engram_chunk_store::snapshot_blob::sidecar_blob_key(snapshot_id),
        ] {
            blob.put(&key, bytes::Bytes::from_static(b"sim"))
                .await
                .expect("blob put");
        }

        // Rest to Idle the surgical way, with the continuously flushed
        // disk one version ahead of the snapshot's disk.
        let mut live = snapshot_disk;
        live.version += 1;
        sim.world.meta.with_db_mut(|db| {
            let row = db.sessions.get_mut(&sid).expect("session row");
            row.session.status = SessionState::Idle;
            row.session.sandbox_id = None;
            row.session.host_id = None;
            row.session.live_disk_manifest = Some(live);
        });

        // Every host refuses the memory image, as a real fleet does.
        {
            let mut hosts = sim.world.host_world.hosts.lock();
            for h in hosts.values_mut() {
                h.refuse_memory_images = true;
                h.created_rootfs.clear();
            }
        }

        sim.execute(Step::ResumeSession).await;

        let status = sim
            .world
            .meta
            .with_db(|db| db.sessions.get(&sid).map(|r| r.session.status))
            .expect("session row");
        assert_eq!(
            status,
            SessionState::Active,
            "a refused memory image must recover from disk, not kill the session",
        );
        let created: Vec<_> = sim
            .world
            .host_world
            .hosts
            .lock()
            .values()
            .flat_map(|h| h.created_rootfs.clone())
            .collect();
        assert_eq!(
            created,
            vec![Some(live)],
            "exactly one cold boot, on the live disk (newer than the snapshot's)",
        );
    });
}
