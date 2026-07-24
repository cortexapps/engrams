//! ADR 0103, failure-matrix rows 14 (wrapper dies, command lives) and the
//! last-activity aging rule: journal liveness is about the COMMAND, not the
//! wrapper, and dead records get a full TTL diagnosis window measured from
//! their last observed activity — not from their start.
//!
//! All states are constructed externally through the public on-disk format
//! (ADR 0099 H5), same discipline as `exec_journal_crash.rs`.

use std::fs;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use engram_agentd::exec_journal::{AttachOrStart, ExecJournal, RequestRecord};

fn write_request_at(dir: &std::path::Path, command: &[String], created_at_unix_ms: u64) {
    fs::write(
        dir.join("request.json"),
        serde_json::to_vec(&RequestRecord {
            command: command.to_vec(),
            created_at_unix_ms,
        })
        .unwrap(),
    )
    .unwrap();
}

/// A pid that provably belonged to a process that has exited.
fn dead_pid() -> u32 {
    let mut child = Command::new("true").spawn().unwrap();
    let pid = child.id();
    child.wait().unwrap();
    pid
}

/// Backdate every file in `dir` (and the dir itself) far past the 24h TTL.
fn backdate_all(dir: &std::path::Path) {
    let stamp = "202001010000"; // 2020-01-01 00:00, years past any TTL
    let mut paths = vec![dir.to_path_buf()];
    for entry in fs::read_dir(dir).unwrap() {
        paths.push(entry.unwrap().path());
    }
    for path in paths {
        assert!(
            Command::new("touch")
                .arg("-t")
                .arg(stamp)
                .arg(&path)
                .status()
                .unwrap()
                .success(),
            "backdate {}",
            path.display()
        );
    }
}

/// An OOM-killed wrapper whose command survives in its own process group is
/// still a live recording: its dir is the spawn-dedupe marker, and GC'ing it
/// would let a retry double-spawn a running command. Constructed as: dead
/// `owner_pid`, live `pid` (this test process), ancient `request.json`.
#[tokio::test]
async fn dead_wrapper_with_live_command_is_never_collected() {
    let temp = tempfile::tempdir().unwrap();
    let command = vec!["sh".into(), "-c".into(), "long build".into()];

    let dir = temp.path().join("exec-wrapperless");
    fs::create_dir_all(&dir).unwrap();
    write_request_at(&dir, &command, 0); // ancient: eligible if judged dead
    fs::write(dir.join("owner_pid"), dead_pid().to_string()).unwrap();
    fs::write(dir.join("pid"), std::process::id().to_string()).unwrap();
    backdate_all(&dir); // even its mtimes are ancient — liveness must win

    let journal = ExecJournal::new(temp.path());
    journal.gc_expired().await;
    assert!(
        dir.exists(),
        "a journal whose command pid is alive must never be collected"
    );
}

/// The same wrapperless-but-live record must hold its concurrency cap slot:
/// it is still recording, so it is still "active".
#[tokio::test]
async fn dead_wrapper_with_live_command_still_holds_a_cap_slot() {
    let temp = tempfile::tempdir().unwrap();
    let command = vec!["sh".into(), "-c".into(), "long build".into()];

    for index in 0..33 {
        let dir = temp.path().join(format!("exec-wrapperless-{index}"));
        fs::create_dir_all(&dir).unwrap();
        write_request_at(&dir, &command, 0);
        fs::write(dir.join("owner_pid"), dead_pid().to_string()).unwrap();
        fs::write(dir.join("pid"), std::process::id().to_string()).unwrap();
    }

    let journal = ExecJournal::new(temp.path());
    match journal
        .attach_or_start("exec-fresh", &command)
        .await
        .unwrap()
    {
        AttachOrStart::DegradedStart { .. } => {}
        other => panic!(
            "33 live recordings must exhaust the cap even when their wrappers died; got {other:?}"
        ),
    }
}

/// Unclean death at hour 23 of a long exec must still get a full diagnosis
/// window: dead records age from their newest file mtime (last observed
/// activity), not from `request.created_at`.
#[tokio::test]
async fn unclean_death_ages_from_last_activity_not_from_start() {
    let temp = tempfile::tempdir().unwrap();
    let command = vec!["sh".into(), "-c".into(), "long build".into()];
    let ancient_start = 0;

    // Record A: started "ages ago" but produced output moments ago (fresh
    // mtimes) before dying — must be RETAINED for diagnosis.
    let recent = temp.path().join("exec-died-recently");
    fs::create_dir_all(&recent).unwrap();
    write_request_at(&recent, &command, ancient_start);
    fs::write(recent.join("owner_pid"), dead_pid().to_string()).unwrap();
    fs::write(recent.join("pid"), dead_pid().to_string()).unwrap();
    fs::write(recent.join("stdout"), b"output written just before death").unwrap();

    // Record B: identical shape, but every trace of activity is ancient —
    // past the TTL, reclaimable.
    let stale = temp.path().join("exec-died-long-ago");
    fs::create_dir_all(&stale).unwrap();
    write_request_at(&stale, &command, ancient_start);
    fs::write(stale.join("owner_pid"), dead_pid().to_string()).unwrap();
    fs::write(stale.join("pid"), dead_pid().to_string()).unwrap();
    fs::write(stale.join("stdout"), b"output from another era").unwrap();
    backdate_all(&stale);

    let journal = ExecJournal::new(temp.path());
    journal.gc_expired().await;

    assert!(
        recent.exists(),
        "a record with fresh activity must keep its full diagnosis window"
    );
    assert!(
        !stale.exists(),
        "a record whose newest activity predates the TTL must be reclaimed"
    );

    // Guard against the fixture lying about time: the recent record's
    // request timestamp really was ancient, so retention proves the aging
    // basis moved to activity mtime.
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    assert!(
        now_ms > 24 * 60 * 60 * 1000,
        "sanity: TTL fits in the epoch"
    );
}
