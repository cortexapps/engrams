//! ADR 0103 / ADR 0099 H5: construct post-crash journal states entirely
//! through the public on-disk format. No failpoints or test-only filesystem.

use std::collections::HashMap;
use std::fs;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use engram_agentd::exec_journal::{
    AttachOrStart, AttachState, ExecJournal, ExitRecord, JournalEntry, RequestRecord,
};
use engram_agentd::{
    read_msg, serve_connection_with_journal, write_msg, CaCertInstaller, CaCertPaths,
    HarnessSupervisor, WireExecEvent, WireExecRequest, WireRequest,
};
use proptest::prelude::*;

fn write_request(dir: &std::path::Path, command: &[String]) {
    fs::write(
        dir.join("request.json"),
        serde_json::to_vec(&RequestRecord {
            command: command.to_vec(),
            created_at_unix_ms: 0,
        })
        .unwrap(),
    )
    .unwrap();
}

/// Like `write_request` but stamped now. Fixtures that flow through
/// `attach_or_start` (which GC-reclaims provably-dead records past the TTL)
/// must sit inside the diagnosis window to model a fresh crash rather than
/// an expired one.
fn write_request_now(dir: &std::path::Path, command: &[String]) {
    fs::write(
        dir.join("request.json"),
        serde_json::to_vec(&RequestRecord {
            command: command.to_vec(),
            created_at_unix_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64,
        })
        .unwrap(),
    )
    .unwrap();
}

fn entry(root: &std::path::Path, exec_id: &str) -> JournalEntry {
    let dir = root.join(exec_id);
    fs::create_dir_all(&dir).unwrap();
    JournalEntry::from_dir(dir).unwrap()
}

#[tokio::test]
async fn crash_states_never_turn_incomplete_records_into_exact_exits() {
    let temp = tempfile::tempdir().unwrap();
    let command = vec!["sh".to_string(), "-c".to_string(), "echo ok".to_string()];
    let payload = b"stdout that may tear at any byte boundary";

    // Exhaustive truncation sweep: every byte prefix is a valid recoverable
    // output state, and none makes the missing completeness marker exact.
    for offset in 0..=payload.len() {
        let record = entry(temp.path(), &format!("torn-{offset}"));
        write_request(record.dir(), &command);
        fs::write(record.dir().join("pid"), std::process::id().to_string()).unwrap();
        fs::write(record.stdout_path(), &payload[..offset]).unwrap();
        assert_eq!(record.state_for(&command).await, AttachState::Running);
    }

    let tmp_only = entry(temp.path(), "tmp-only");
    write_request(tmp_only.dir(), &command);
    fs::write(tmp_only.dir().join("pid"), std::process::id().to_string()).unwrap();
    fs::write(
        tmp_only.dir().join("exit.json.tmp"),
        serde_json::to_vec(&ExitRecord {
            exit: Some(0),
            finished_at_unix_ms: 1,
        })
        .unwrap(),
    )
    .unwrap();
    assert_eq!(tmp_only.state_for(&command).await, AttachState::Running);

    let dead = entry(temp.path(), "dead");
    write_request(dead.dir(), &command);
    fs::write(dead.dir().join("pid"), u32::MAX.to_string()).unwrap();
    assert!(matches!(
        dead.state_for(&command).await,
        AttachState::Died { .. }
    ));

    // The command can be dead while its wrapper is still draining pipes and
    // preparing exit.json. That externally visible state is Running, not a
    // fabricated terminal result.
    let finishing = entry(temp.path(), "finishing");
    write_request(finishing.dir(), &command);
    fs::write(finishing.dir().join("pid"), u32::MAX.to_string()).unwrap();
    fs::write(
        finishing.dir().join("owner_pid"),
        std::process::id().to_string(),
    )
    .unwrap();
    assert_eq!(finishing.state_for(&command).await, AttachState::Running);

    let garbage = entry(temp.path(), "garbage");
    write_request(garbage.dir(), &command);
    fs::write(garbage.dir().join("pid"), std::process::id().to_string()).unwrap();
    fs::write(garbage.dir().join("unrelated.partial.swp"), b"garbage").unwrap();
    fs::create_dir(garbage.dir().join("lost+found")).unwrap();
    assert_eq!(garbage.state_for(&command).await, AttachState::Running);

    let mismatch = entry(temp.path(), "mismatch");
    write_request(mismatch.dir(), &["first".into()]);
    assert_eq!(
        mismatch.state_for(&["second".into()]).await,
        AttachState::Mismatch {
            recorded_command: vec!["first".into()]
        }
    );
}

#[tokio::test]
async fn concurrent_same_ticket_grants_exactly_one_spawn_authorization() {
    let temp = tempfile::tempdir().unwrap();
    let journal = ExecJournal::new(temp.path());
    let command = vec!["true".to_string()];
    let (left, right) = tokio::join!(
        journal.attach_or_start("one-ticket", &command),
        journal.attach_or_start("one-ticket", &command),
    );
    let outcomes = [left.unwrap(), right.unwrap()];
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| matches!(outcome, AttachOrStart::Start(_)))
            .count(),
        1
    );
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| matches!(outcome, AttachOrStart::Attach(_)))
            .count(),
        1
    );
}

#[tokio::test]
async fn command_mismatch_is_loud_before_any_recorded_output_is_replayed() {
    let temp = tempfile::tempdir().unwrap();
    let exec_id = "first-writer-wins";
    let record = entry(temp.path(), exec_id);
    write_request_now(record.dir(), &["first-command".into()]);
    fs::write(record.stdout_path(), b"must-not-leak").unwrap();

    let (mut client, server) = tokio::io::duplex(4096);
    let ca = CaCertInstaller::new(CaCertPaths {
        bundle: temp.path().join("ca-bundle"),
        extra_cert: temp.path().join("ca-extra"),
    });
    let server_task = tokio::spawn(serve_connection_with_journal(
        server,
        None,
        HarnessSupervisor::new(),
        Arc::new(ca),
        Arc::new(ExecJournal::new(temp.path())),
    ));
    write_msg(
        &mut client,
        &WireRequest::Exec(WireExecRequest {
            command: vec!["second-command".into()],
            stdin: None,
            env: HashMap::new(),
            workdir: None,
            timeout_ms: None,
            exec_id: Some(exec_id.into()),
            stdout_offset: Some(0),
            stderr_offset: Some(0),
            wake: None,
            attach_only: false,
        }),
    )
    .await
    .unwrap();
    assert!(matches!(
        read_msg::<_, WireExecEvent>(&mut client).await.unwrap(),
        WireExecEvent::Started(id) if id == exec_id
    ));
    let mismatch = read_msg::<_, WireExecEvent>(&mut client).await.unwrap();
    assert!(matches!(
        mismatch,
        WireExecEvent::Refused { reason }
            if reason.contains("first writer wins") && !reason.contains("must-not-leak")
    ));
    server_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn attach_only_missing_journal_never_authorizes_a_second_spawn() {
    let temp = tempfile::tempdir().unwrap();
    let exec_id = "gc-miss";
    let (mut client, server) = tokio::io::duplex(4096);
    let ca = CaCertInstaller::new(CaCertPaths {
        bundle: temp.path().join("ca-bundle"),
        extra_cert: temp.path().join("ca-extra"),
    });
    let server_task = tokio::spawn(serve_connection_with_journal(
        server,
        None,
        HarnessSupervisor::new(),
        Arc::new(ca),
        Arc::new(ExecJournal::new(temp.path())),
    ));
    write_msg(
        &mut client,
        &WireRequest::Exec(WireExecRequest {
            command: vec!["must-not-run".into()],
            stdin: None,
            env: HashMap::new(),
            workdir: None,
            timeout_ms: None,
            exec_id: Some(exec_id.into()),
            stdout_offset: Some(0),
            stderr_offset: Some(0),
            wake: None,
            attach_only: true,
        }),
    )
    .await
    .unwrap();
    assert!(matches!(
        read_msg::<_, WireExecEvent>(&mut client).await.unwrap(),
        WireExecEvent::Started(id) if id == exec_id
    ));
    assert!(matches!(
        read_msg::<_, WireExecEvent>(&mut client).await.unwrap(),
        WireExecEvent::Refused { reason }
            if reason.contains("refusing to spawn a second command")
    ));
    server_task.await.unwrap().unwrap();
    assert!(!temp.path().join(exec_id).exists());
}

#[tokio::test]
async fn nonzero_offsets_against_vacant_journal_refuse_without_spawn_or_leak() {
    let temp = tempfile::tempdir().unwrap();
    let exec_id = "vacant-with-offsets";
    let command = vec!["true".into()];

    let make_request = |stdout_offset| {
        WireRequest::Exec(WireExecRequest {
            command: command.clone(),
            stdin: None,
            env: HashMap::new(),
            workdir: Some(temp.path().to_string_lossy().into_owned()),
            timeout_ms: None,
            exec_id: Some(exec_id.into()),
            stdout_offset: Some(stdout_offset),
            stderr_offset: Some(0),
            wake: None,
            attach_only: false,
        })
    };

    let (mut client, server) = tokio::io::duplex(4096);
    let ca = CaCertInstaller::new(CaCertPaths {
        bundle: temp.path().join("ca-bundle"),
        extra_cert: temp.path().join("ca-extra"),
    });
    let server_task = tokio::spawn(serve_connection_with_journal(
        server,
        None,
        HarnessSupervisor::new(),
        Arc::new(ca),
        Arc::new(ExecJournal::new(temp.path().join("journal"))),
    ));
    write_msg(&mut client, &make_request(1)).await.unwrap();
    assert!(matches!(
        read_msg::<_, WireExecEvent>(&mut client).await.unwrap(),
        WireExecEvent::Started(id) if id == exec_id
    ));
    assert!(matches!(
        read_msg::<_, WireExecEvent>(&mut client).await.unwrap(),
        WireExecEvent::Refused { reason }
            if reason.contains("no journal for exec_id vacant-with-offsets")
                && reason.contains("non-zero replay offsets")
                && reason.contains("refusing to spawn from scratch")
    ));
    server_task.await.unwrap().unwrap();
    assert!(!temp.path().join("journal").join(exec_id).exists());

    // Cleanup of the refused vacant authorization matters: a normal
    // zero-offset attach with the same ticket must still win Start.
    let (mut client, server) = tokio::io::duplex(4096);
    let ca = CaCertInstaller::new(CaCertPaths {
        bundle: temp.path().join("ca-bundle-2"),
        extra_cert: temp.path().join("ca-extra-2"),
    });
    let server_task = tokio::spawn(serve_connection_with_journal(
        server,
        None,
        HarnessSupervisor::new(),
        Arc::new(ca),
        Arc::new(ExecJournal::new(temp.path().join("journal"))),
    ));
    write_msg(&mut client, &make_request(0)).await.unwrap();
    assert!(matches!(
        read_msg::<_, WireExecEvent>(&mut client).await.unwrap(),
        WireExecEvent::Started(id) if id == exec_id
    ));
    loop {
        match read_msg::<_, WireExecEvent>(&mut client).await.unwrap() {
            WireExecEvent::Exit(Some(0)) => break,
            WireExecEvent::Stdout(_) | WireExecEvent::Stderr(_) => {}
            other => panic!("zero-offset follow-up must start cleanly, got {other:?}"),
        }
    }
    server_task.await.unwrap().unwrap();
    assert!(
        temp.path()
            .join("journal")
            .join(exec_id)
            .join("request.json")
            .exists(),
        "the zero-offset follow-up must receive a fresh Start authorization"
    );
}

/// PR #874 review finding: journals whose wrapper died without writing
/// `exit.json` must not occupy active-cap slots forever. 32 unclean wrapper
/// deaths (OOM-kill etc.) would otherwise silently degrade every later exec
/// on a long-lived session to stage-1.
#[tokio::test]
async fn dead_wrapper_journals_free_their_active_slots() {
    let temp = tempfile::tempdir().unwrap();
    let journal = ExecJournal::new(temp.path());
    let command = vec!["true".to_string()];
    for i in 0..32 {
        let record = entry(temp.path(), &format!("dead-{i}"));
        write_request_now(record.dir(), &command);
        fs::write(record.dir().join("pid"), u32::MAX.to_string()).unwrap();
    }
    assert!(
        matches!(
            journal
                .attach_or_start("fresh-after-deaths", &command)
                .await
                .unwrap(),
            AttachOrStart::Start(_)
        ),
        "dead-without-marker journals must not consume the durable-exec budget"
    );
}

/// The inverse guard: genuinely live recordings still hold the cap, so the
/// liveness-aware count cannot quietly disable the resource bound.
#[tokio::test]
async fn live_wrapper_journals_still_hold_the_cap() {
    let temp = tempfile::tempdir().unwrap();
    let journal = ExecJournal::new(temp.path());
    let command = vec!["true".to_string()];
    // 33 live records (this test process stands in for the wrapper): with
    // the new dir itself the count is 34 > 32, so the start degrades.
    for i in 0..33 {
        let record = entry(temp.path(), &format!("live-{i}"));
        write_request_now(record.dir(), &command);
        fs::write(record.dir().join("pid"), std::process::id().to_string()).unwrap();
    }
    assert!(
        matches!(
            journal
                .attach_or_start("fresh-over-cap", &command)
                .await
                .unwrap(),
            AttachOrStart::DegradedStart { .. }
        ),
        "live recordings past the cap must still degrade the next start"
    );
}

/// Provably-dead incomplete records are reclaimed after the TTL (bounding
/// disk), but stay diagnosable inside it, and live recordings are never
/// collected regardless of age.
#[tokio::test]
async fn gc_reclaims_dead_journals_after_ttl_but_retains_fresh_and_live() {
    let temp = tempfile::tempdir().unwrap();
    let journal = ExecJournal::new(temp.path());
    let command = vec!["true".to_string()];

    // Ancient + dead pid → collectible. Dead records age from their newest
    // file mtime (last observed activity), so "ancient" means the mtimes
    // are backdated too, not just `created_at`.
    let ancient_dead = entry(temp.path(), "ancient-dead");
    write_request(ancient_dead.dir(), &command);
    fs::write(ancient_dead.dir().join("pid"), u32::MAX.to_string()).unwrap();
    for path in [
        ancient_dead.dir().to_path_buf(),
        ancient_dead.dir().join("request.json"),
        ancient_dead.dir().join("pid"),
    ] {
        assert!(
            std::process::Command::new("touch")
                .arg("-t")
                .arg("202001010000")
                .arg(&path)
                .status()
                .unwrap()
                .success(),
            "backdate {}",
            path.display()
        );
    }

    // Fresh + dead pid → inside the diagnosis window, retained.
    let fresh_dead = entry(temp.path(), "fresh-dead");
    fs::write(
        fresh_dead.dir().join("request.json"),
        serde_json::to_vec(&RequestRecord {
            command: command.clone(),
            created_at_unix_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64,
        })
        .unwrap(),
    )
    .unwrap();
    fs::write(fresh_dead.dir().join("pid"), u32::MAX.to_string()).unwrap();

    // Ancient but live (this process's pid) → never collected.
    let ancient_live = entry(temp.path(), "ancient-live");
    write_request(ancient_live.dir(), &command);
    fs::write(
        ancient_live.dir().join("pid"),
        std::process::id().to_string(),
    )
    .unwrap();

    journal.gc_expired().await;

    assert!(
        !ancient_dead.dir().exists(),
        "a provably-dead record past the TTL must be reclaimed"
    );
    assert!(
        fresh_dead.dir().exists(),
        "a dead record inside the TTL stays diagnosable"
    );
    assert!(
        ancient_live.dir().exists(),
        "a live recording is never collected, whatever its age"
    );
}

/// PR #874 review finding: the `Died` terminal must drain like `Complete`
/// does. A crashed wrapper (SIGKILL/OOM — pid dead, no exit.json) leaves
/// static journal files; an attach reads them 8 KiB per iteration, and
/// returning on the iteration that first observes `Died` dropped everything
/// past the current chunk — the crash diagnostics the caller attached for.
#[tokio::test]
async fn died_attach_drains_the_full_journal_before_exit_none() {
    let temp = tempfile::tempdir().unwrap();
    let exec_id = "died-drain";
    let command = vec!["sh".to_string(), "-c".to_string(), "crashy".to_string()];
    let record = entry(temp.path(), exec_id);
    write_request_now(record.dir(), &command);
    fs::write(record.dir().join("pid"), u32::MAX.to_string()).unwrap();
    // Several read-chunks (8 KiB each) on both streams, sizes offset so the
    // final partial chunks differ.
    let stdout_payload: Vec<u8> = (0..(3 * 8 * 1024 + 17)).map(|i| (i % 251) as u8).collect();
    let stderr_payload: Vec<u8> = (0..(2 * 8 * 1024 + 5)).map(|i| (i % 241) as u8).collect();
    fs::write(record.stdout_path(), &stdout_payload).unwrap();
    fs::write(record.stderr_path(), &stderr_payload).unwrap();

    let (mut client, server) = tokio::io::duplex(128 * 1024);
    let ca = CaCertInstaller::new(CaCertPaths {
        bundle: temp.path().join("ca-bundle"),
        extra_cert: temp.path().join("ca-extra"),
    });
    let server_task = tokio::spawn(serve_connection_with_journal(
        server,
        None,
        HarnessSupervisor::new(),
        Arc::new(ca),
        Arc::new(ExecJournal::new(temp.path())),
    ));
    write_msg(
        &mut client,
        &WireRequest::Exec(WireExecRequest {
            command,
            stdin: None,
            env: HashMap::new(),
            workdir: None,
            timeout_ms: None,
            exec_id: Some(exec_id.into()),
            stdout_offset: Some(0),
            stderr_offset: Some(0),
            wake: None,
            attach_only: false,
        }),
    )
    .await
    .unwrap();

    assert_eq!(
        read_msg::<_, WireExecEvent>(&mut client).await.unwrap(),
        WireExecEvent::Started(exec_id.into())
    );
    let mut replayed_stdout = Vec::new();
    let mut replayed_stderr = Vec::new();
    loop {
        match read_msg::<_, WireExecEvent>(&mut client).await.unwrap() {
            WireExecEvent::Stdout(bytes) => replayed_stdout.extend(bytes),
            WireExecEvent::Stderr(bytes) => replayed_stderr.extend(bytes),
            WireExecEvent::Exit(status) => {
                assert_eq!(status, None, "a died record must never fabricate an exit");
                break;
            }
            WireExecEvent::Started(id) => panic!("duplicate Started({id})"),
            WireExecEvent::Degraded(reason) => panic!("healthy died record degraded: {reason}"),
            WireExecEvent::Refused { reason } => panic!("healthy died record refused: {reason}"),
        }
    }
    assert_eq!(replayed_stdout, stdout_payload, "stdout tail dropped");
    let (diagnostic_tail, message) = replayed_stderr.split_at(stderr_payload.len());
    assert_eq!(diagnostic_tail, stderr_payload, "stderr tail dropped");
    assert!(
        String::from_utf8_lossy(message).contains("died without exit.json"),
        "the died terminal must still be loud after the drain"
    );
    server_task.await.unwrap().unwrap();
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn cancel_kills_the_recorded_child_process_group() {
    let temp = tempfile::tempdir().unwrap();
    let journal = ExecJournal::new(temp.path());
    let command = vec!["sh".to_string(), "-c".to_string(), "sleep 30".to_string()];
    let record = match journal
        .attach_or_start("cancel-process-group", &command)
        .await
        .unwrap()
    {
        AttachOrStart::Start(record) => record,
        other => panic!("fresh ticket must authorize start, got {other:?}"),
    };
    let wrapper_record = record.clone();
    let wrapper_command = command.clone();
    let wrapper = tokio::spawn(async move {
        engram_agentd::exec_journal::run_wrapper(wrapper_record, wrapper_command, None).await
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while record.pid().await.is_err() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("wrapper publishes its cancellable process-group pid");
    engram_agentd::exec_journal::cancel(&record).await.unwrap();
    let exit = tokio::time::timeout(std::time::Duration::from_secs(2), wrapper)
        .await
        .expect("cancelled wrapper completes")
        .unwrap()
        .unwrap();
    assert_eq!(exit, None);
    assert_eq!(record.exit().await.unwrap().exit, None);
}

fn replay_case(stdout: Vec<u8>, stderr: Vec<u8>, out_seed: u16, err_seed: u16) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async move {
        let temp = tempfile::tempdir().unwrap();
        let exec_id = "offset-replay";
        let command = vec!["printf".to_string(), "journal".to_string()];
        let record = entry(temp.path(), exec_id);
        write_request_now(record.dir(), &command);
        fs::write(record.stdout_path(), &stdout).unwrap();
        fs::write(record.stderr_path(), &stderr).unwrap();
        fs::write(
            record.dir().join("exit.json"),
            serde_json::to_vec(&ExitRecord {
                exit: Some(7),
                finished_at_unix_ms: SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_millis() as u64,
            })
            .unwrap(),
        )
        .unwrap();
        let stdout_offset = usize::from(out_seed) % (stdout.len() + 1);
        let stderr_offset = usize::from(err_seed) % (stderr.len() + 1);

        let (mut client, server) = tokio::io::duplex(128 * 1024);
        let ca = CaCertInstaller::new(CaCertPaths {
            bundle: temp.path().join("ca-bundle"),
            extra_cert: temp.path().join("ca-extra"),
        });
        let server_task = tokio::spawn(serve_connection_with_journal(
            server,
            None,
            HarnessSupervisor::new(),
            Arc::new(ca),
            Arc::new(ExecJournal::new(temp.path())),
        ));
        write_msg(
            &mut client,
            &WireRequest::Exec(WireExecRequest {
                command,
                stdin: None,
                env: HashMap::new(),
                workdir: None,
                timeout_ms: None,
                exec_id: Some(exec_id.into()),
                stdout_offset: Some(stdout_offset as u64),
                stderr_offset: Some(stderr_offset as u64),
                wake: None,
                attach_only: false,
            }),
        )
        .await
        .unwrap();

        assert_eq!(
            read_msg::<_, WireExecEvent>(&mut client).await.unwrap(),
            WireExecEvent::Started(exec_id.into())
        );
        let mut replayed_stdout = Vec::new();
        let mut replayed_stderr = Vec::new();
        loop {
            match read_msg::<_, WireExecEvent>(&mut client).await.unwrap() {
                WireExecEvent::Stdout(bytes) => replayed_stdout.extend(bytes),
                WireExecEvent::Stderr(bytes) => replayed_stderr.extend(bytes),
                WireExecEvent::Exit(status) => {
                    assert_eq!(status, Some(7));
                    break;
                }
                WireExecEvent::Started(id) => panic!("duplicate Started({id})"),
                WireExecEvent::Degraded(reason) => panic!("completed record degraded: {reason}"),
                WireExecEvent::Refused { reason } => {
                    panic!("completed record unexpectedly refused: {reason}")
                }
            }
        }
        assert_eq!(replayed_stdout, stdout[stdout_offset..]);
        assert_eq!(replayed_stderr, stderr[stderr_offset..]);
        server_task.await.unwrap().unwrap();
    });
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 96,
        failure_persistence: Some(Box::new(
            proptest::test_runner::FileFailurePersistence::Direct(
                "tests/exec_journal_offsets.proptest-regressions",
            ),
        )),
        ..ProptestConfig::default()
    })]

    #[test]
    fn offset_replay_has_no_gaps_or_duplicate_bytes(
        stdout in proptest::collection::vec(any::<u8>(), 0..20_000),
        stderr in proptest::collection::vec(any::<u8>(), 0..20_000),
        out_seed in any::<u16>(),
        err_seed in any::<u16>(),
    ) {
        replay_case(stdout, stderr, out_seed, err_seed);
    }
}
