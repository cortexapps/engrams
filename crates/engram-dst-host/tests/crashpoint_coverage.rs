//! The derived-from-production crash-schedule meta-test (ADR 0098 P5).
//!
//! P2–P4 kept a hand-maintained table mapping a static `CrashPoint` enum to
//! the ADR 0099 H5 tests — which only proved the table was self-consistent.
//! P5 wires `durable_record::persist` and `spool::write_spool` through the
//! injected `HostFs` seam, so the crash-point list is now DERIVED by running
//! the REAL production bodies through a recording [`CrashFs`] and reading
//! the op trace back. This test pins that derivation:
//!
//! * the recorded trace of each flow equals its production op sequence
//!   (a change to either body shows up here as a trace diff, forcing the
//!   crash schedule to follow reality — never a stale parallel list); and
//! * the crash schedule is exactly `0..=trace.len()` — every boundary
//!   between ops, both ends included (index `len` = "completed, then
//!   died"), with a cut at every index actually refusing the op.
//!
//! Byte-level torn states stay ADR 0099 H5's static tests in the source
//! modules; this seam owns operation-granularity reachability.

use engram_core::types::manifest::ManifestRef;
use engram_core::SandboxId;
use engram_dst_host::{CrashFs, FsOp};
use engram_host_agent::disk_daemon::spool;
use engram_host_agent::durable_record;

#[derive(serde::Serialize, serde::Deserialize)]
struct Rec {
    id: String,
}

fn refv(version: u64) -> ManifestRef {
    ManifestRef {
        manifest_id: uuid::Uuid::from_u128(0xabcd),
        version,
    }
}

/// `durable_record::persist`'s production op sequence, derived by running it.
#[tokio::test]
async fn persist_op_trace_is_the_production_sequence() {
    let tmp = tempfile::tempdir().unwrap();
    let fs = CrashFs::recording();
    let rec = Rec { id: "r".into() };
    durable_record::persist(fs.as_ref(), tmp.path(), &rec.id, &rec, "rec")
        .await
        .unwrap();
    assert_eq!(
        fs.trace(),
        vec![
            FsOp::CreateDir, // create_dir_all(dir)
            FsOp::Write,     // the .json.partial temp
            FsOp::SyncFile,  // fsync the temp
            FsOp::Rename,    // atomic publish
            FsOp::SyncDir,   // fsync the parent dir
        ],
        "durable_record::persist's op sequence changed — update the crash \
         schedule reasoning (and the H5 states) alongside the body",
    );
}

/// `spool::write_spool`'s production op sequence over a two-chunk set,
/// derived by running it. The destructive replace window (`RemoveDir` →
/// `CreateDir`) leads; the completeness marker is written LAST before the
/// dir fsyncs.
#[tokio::test]
async fn write_spool_op_trace_is_the_production_sequence() {
    let tmp = tempfile::tempdir().unwrap();
    let sid = SandboxId::new();
    let chunks = vec![(0usize, vec![1u8; 32]), (3usize, vec![2u8; 32])];
    // Seed a prior spool so the replace window's RemoveDir acts on a real
    // dir (NotFound is tolerated either way; the trace is identical).
    spool::write_spool(
        &engram_host_core::TokioFs,
        tmp.path(),
        sid,
        refv(1),
        &chunks,
    )
    .await
    .unwrap();
    let fs = CrashFs::recording();
    spool::write_spool(fs.as_ref(), tmp.path(), sid, refv(2), &chunks)
        .await
        .unwrap();
    assert_eq!(
        fs.trace(),
        vec![
            FsOp::RemoveDir, // replace-don't-merge: drop the prior spool
            FsOp::CreateDir, // fresh spool dir
            FsOp::Write,     // chunk-0.bin
            FsOp::SyncFile,
            FsOp::Write, // chunk-3.bin
            FsOp::SyncFile,
            FsOp::Write, // meta.json — the completeness marker, LAST
            FsOp::SyncFile,
            FsOp::SyncDir, // the spool dir's entries
            FsOp::SyncDir, // the root's entry for the dir
        ],
        "spool::write_spool's op sequence changed — update the crash \
         schedule reasoning (and the H5 states) alongside the body",
    );
}

/// The crash schedule is exactly `0..=trace.len()`: a cut at every index k
/// refuses op k after genuinely performing ops 0..k (the trace still records
/// the refused op), and a cut past the end completes the flow.
#[tokio::test]
async fn crash_schedule_covers_every_op_boundary() {
    let tmp = tempfile::tempdir().unwrap();
    let sid = SandboxId::new();
    let chunks = vec![(0usize, vec![7u8; 16])];

    // Derive the full trace length from an un-cut run.
    let recording = CrashFs::recording();
    spool::write_spool(recording.as_ref(), tmp.path(), sid, refv(1), &chunks)
        .await
        .unwrap();
    let full = recording.trace();

    for k in 0..=full.len() {
        let fs = CrashFs::with_crash_at(Some(k));
        let result = spool::write_spool(fs.as_ref(), tmp.path(), sid, refv(2), &chunks).await;
        let trace = fs.trace();
        if k < full.len() {
            assert!(
                result.is_err(),
                "cut at op {k} must refuse the op and error the flow"
            );
            // The refused op is recorded (trace = k performed + 1 refused),
            // and the performed prefix matches the production sequence.
            assert_eq!(
                trace.len(),
                k + 1,
                "cut at {k}: ops 0..{k} ran, op {k} refused"
            );
            assert_eq!(
                &trace[..k],
                &full[..k],
                "the performed prefix is the real sequence"
            );
        } else {
            assert!(result.is_ok(), "a cut past the end completes the flow");
            assert_eq!(trace, full, "an un-reached cut leaves the full sequence");
        }
        // Restore a complete spool for the next iteration's baseline.
        spool::write_spool(
            &engram_host_core::TokioFs,
            tmp.path(),
            sid,
            refv(1),
            &chunks,
        )
        .await
        .unwrap();
    }
}
