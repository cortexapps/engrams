//! The crash schedule that derives from production file operations.
//!
//! `durable_record::persist` uses the injected `HostFs` seam. [`CrashFs`]
//! records that sequence and can stop at one selected operation. These tests
//! keep the crash schedule equal to the production sequence.

use engram_dst_host::{CrashFs, FsOp};
use engram_host_agent::durable_record;

#[derive(serde::Serialize, serde::Deserialize)]
struct Rec {
    id: String,
}

/// Record the production operation sequence for a durable record.
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
            FsOp::CreateDir,
            FsOp::Write,
            FsOp::SyncFile,
            FsOp::Rename,
            FsOp::SyncDir,
        ],
        "the durable record operation sequence changed",
    );
}

/// Cut the durable record write at every operation boundary.
#[tokio::test]
async fn crash_schedule_covers_every_op_boundary() {
    let recording_dir = tempfile::tempdir().unwrap();
    let recording = CrashFs::recording();
    let rec = Rec { id: "r".into() };
    durable_record::persist(
        recording.as_ref(),
        recording_dir.path(),
        &rec.id,
        &rec,
        "rec",
    )
    .await
    .unwrap();
    let full = recording.trace();

    for cut in 0..=full.len() {
        let tmp = tempfile::tempdir().unwrap();
        let fs = CrashFs::with_crash_at(Some(cut));
        let result = durable_record::persist(fs.as_ref(), tmp.path(), &rec.id, &rec, "rec").await;
        let trace = fs.trace();
        if cut < full.len() {
            assert!(result.is_err(), "cut {cut} must stop the write");
            assert_eq!(trace.len(), cut + 1, "cut {cut} must record the refused op");
            assert_eq!(&trace[..cut], &full[..cut], "the prefix must be exact");
        } else {
            assert!(result.is_ok(), "a cut after the sequence must not run");
            assert_eq!(trace, full, "the complete sequence must be exact");
        }
    }
}

/// Seal each durable record under its record id.
#[tokio::test]
async fn persisted_records_are_sealed_envelopes() {
    use engram_host_agent::durable_envelope;

    let tmp = tempfile::tempdir().unwrap();
    let rec = Rec { id: "r".into() };
    durable_record::persist(&engram_host_core::TokioFs, tmp.path(), &rec.id, &rec, "rec")
        .await
        .unwrap();
    let on_disk = tokio::fs::read(tmp.path().join("r.json")).await.unwrap();
    let body = durable_envelope::open(&on_disk, "r").expect("record id must open the record");
    let back: Rec = serde_json::from_slice(&body).unwrap();
    assert_eq!(back.id, "r");
    assert!(
        durable_envelope::open(&on_disk, "not-r").is_err(),
        "a different record id must fail",
    );
}
