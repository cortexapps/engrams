//! ADR 0103 review round 8/9: CancelExec must be safe against hostile and
//! stale inputs. Two properties:
//!
//! 1. **No path traversal.** The raw caller-supplied exec_id is validated at
//!    the wire entry before any path is built — `../` cannot escape the
//!    journal root to SIGKILL an arbitrary process group.
//! 2. **The terminal marker gates the kill.** A journal whose `exit.json`
//!    exists is already terminal; cancel must be a no-op instead of
//!    SIGKILLing whatever process group now owns the recorded (reusable)
//!    pid.

use std::collections::HashMap;
use std::fs;
use std::os::unix::process::CommandExt;
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use engram_agentd::exec_journal::{ExecJournal, ExitRecord, JournalEntry, RequestRecord};
use engram_agentd::{
    read_msg, serve_connection_with_journal, write_msg, CaCertInstaller, CaCertPaths,
    HarnessSupervisor, WireRequest, WireResponse,
};

/// A long-lived child that is its own process-group leader, so a
/// `killpg` aimed at its pid would genuinely kill it.
fn group_leader_decoy() -> std::process::Child {
    let mut command = Command::new("sleep");
    command.arg("30");
    command.process_group(0);
    command.spawn().expect("spawn decoy")
}

fn decoy_is_alive(decoy: &mut std::process::Child) -> bool {
    std::thread::sleep(Duration::from_millis(150));
    decoy.try_wait().expect("try_wait decoy").is_none()
}

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

#[tokio::test]
async fn cancel_with_traversal_exec_id_is_refused_and_kills_nothing() {
    let temp = tempfile::tempdir().unwrap();
    let journal_root = temp.path().join("execs");
    fs::create_dir_all(&journal_root).unwrap();

    // A journal-shaped directory OUTSIDE the root, reachable only via `..`,
    // whose pid names a live process group.
    let mut decoy = group_leader_decoy();
    let escape = temp.path().join("evil");
    fs::create_dir_all(&escape).unwrap();
    fs::write(escape.join("pid"), decoy.id().to_string()).unwrap();

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
        Arc::new(ExecJournal::new(&journal_root)),
    ));
    write_msg(
        &mut client,
        &WireRequest::CancelExec {
            exec_id: "../evil".into(),
        },
    )
    .await
    .unwrap();
    let response: WireResponse = read_msg(&mut client).await.unwrap();
    assert!(
        matches!(response, WireResponse::Error { .. }),
        "a traversal exec_id must be refused with a typed error, got {response:?}"
    );
    assert!(
        decoy_is_alive(&mut decoy),
        "the escaped process group must never be signalled"
    );
    decoy.kill().ok();
    drop(client);
    server_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn cancel_of_a_completed_exec_is_a_noop_not_a_pid_reuse_kill() {
    let temp = tempfile::tempdir().unwrap();
    let command = vec!["true".to_string()];

    // A completed journal whose recorded pid has since been "reused" by an
    // unrelated live process group (the decoy stands in for the reuse).
    let mut decoy = group_leader_decoy();
    let dir = temp.path().join("exec-finished");
    fs::create_dir_all(&dir).unwrap();
    write_request_now(&dir, &command);
    fs::write(dir.join("pid"), decoy.id().to_string()).unwrap();
    fs::write(
        dir.join("exit.json"),
        serde_json::to_vec(&ExitRecord {
            exit: Some(0),
            finished_at_unix_ms: 1,
        })
        .unwrap(),
    )
    .unwrap();

    let entry = JournalEntry::from_dir(&dir).unwrap();
    engram_agentd::exec_journal::cancel(&entry)
        .await
        .expect("cancelling an already-terminal exec is a successful no-op");
    // Meaningful on Linux (CI): kill_process_group is a no-op on macOS.
    assert!(
        decoy_is_alive(&mut decoy),
        "a terminal journal must never SIGKILL its recorded (reusable) pid"
    );
    decoy.kill().ok();
    let _ = HashMap::<String, String>::new();
}

/// Platform-independent discriminator for the terminal short-circuit: a
/// terminal journal with NO pid file must cancel cleanly and instantly —
/// there is provably nothing left to kill — rather than erroring after the
/// pid-wait retries.
#[tokio::test]
async fn cancel_of_a_completed_exec_without_a_pid_file_is_ok() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path().join("exec-finished-no-pid");
    fs::create_dir_all(&dir).unwrap();
    write_request_now(&dir, &["true".to_string()]);
    fs::write(
        dir.join("exit.json"),
        serde_json::to_vec(&ExitRecord {
            exit: Some(0),
            finished_at_unix_ms: 1,
        })
        .unwrap(),
    )
    .unwrap();

    let entry = JournalEntry::from_dir(&dir).unwrap();
    engram_agentd::exec_journal::cancel(&entry)
        .await
        .expect("a terminal journal has nothing to kill; cancel must succeed");
}

/// The terminal marker only covers the CLEAN half of the PID-reuse hazard:
/// a wrapper stuck draining (daemonized child holding the pipe) or dead
/// uncleanly never writes `exit.json`, yet its command was reaped long ago
/// and the recorded pid is kernel-reusable. The (pid, starttime) identity
/// recorded at spawn is the gate: a mismatch proves the pid now belongs to
/// someone else — and a pid is only recyclable once our whole group is
/// empty, so there is provably nothing of ours left to kill.
#[tokio::test]
async fn cancel_of_a_recycled_pid_never_kills_the_new_owner() {
    let temp = tempfile::tempdir().unwrap();
    let command = vec!["sh".to_string(), "-c".to_string(), "sleep 30".to_string()];

    // The decoy stands in for the unrelated process group that inherited
    // the recorded pid. The recorded identity deliberately mismatches the
    // decoy's true start time.
    let mut decoy = group_leader_decoy();
    let dir = temp.path().join("exec-recycled-pid");
    fs::create_dir_all(&dir).unwrap();
    write_request_now(&dir, &command);
    fs::write(dir.join("pid"), decoy.id().to_string()).unwrap();
    let wrong_ticks = engram_agentd::exec_journal::process_start_ticks(decoy.id())
        .unwrap_or(0)
        .wrapping_add(99_999);
    fs::write(dir.join("pid_start"), wrong_ticks.to_string()).unwrap();

    let entry = JournalEntry::from_dir(&dir).unwrap();
    engram_agentd::exec_journal::cancel(&entry)
        .await
        .expect("a provably-recycled pid means our command is gone; cancel is a successful no-op");
    // Meaningful on Linux (CI): kill_process_group is a no-op on macOS.
    assert!(
        decoy_is_alive(&mut decoy),
        "a recycled pid must never be SIGKILLed on the old journal's behalf"
    );
    decoy.kill().ok();
}

/// The inverse guard: a MATCHING identity must still kill — the gate must
/// not turn cancel into a universal no-op.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn cancel_with_matching_identity_kills_the_group() {
    let temp = tempfile::tempdir().unwrap();
    let command = vec!["sleep".to_string(), "30".to_string()];

    let mut decoy = group_leader_decoy();
    let dir = temp.path().join("exec-live-match");
    fs::create_dir_all(&dir).unwrap();
    write_request_now(&dir, &command);
    fs::write(dir.join("pid"), decoy.id().to_string()).unwrap();
    let true_ticks = engram_agentd::exec_journal::process_start_ticks(decoy.id())
        .expect("a live decoy has readable stat");
    fs::write(dir.join("pid_start"), true_ticks.to_string()).unwrap();

    let entry = JournalEntry::from_dir(&dir).unwrap();
    engram_agentd::exec_journal::cancel(&entry)
        .await
        .expect("cancel of a verified live command succeeds");
    assert!(
        !decoy_is_alive(&mut decoy),
        "a verified (pid, starttime) identity must still be killed"
    );
    decoy.kill().ok();
}

/// Cancel of an exec whose group is already gone (finished, or the wrapper
/// died uncleanly without `exit.json`) is the cancel's GOAL STATE — it must
/// resolve Ok, not surface the killpg ESRCH as an error to the caller.
#[tokio::test]
async fn cancel_of_a_dead_group_is_ok_not_an_error() {
    let temp = tempfile::tempdir().unwrap();
    let command = vec!["true".to_string()];

    // A group leader that has already exited and been reaped: its pgid no
    // longer exists (killpg → ESRCH on Linux).
    let mut leader = {
        let mut c = Command::new("true");
        c.process_group(0);
        c.spawn().expect("spawn short-lived leader")
    };
    let pid = leader.id();
    leader.wait().expect("reap short-lived leader");

    let dir = temp.path().join("exec-dead-group");
    fs::create_dir_all(&dir).unwrap();
    write_request_now(&dir, &command);
    fs::write(dir.join("pid"), pid.to_string()).unwrap();

    let entry = JournalEntry::from_dir(&dir).unwrap();
    engram_agentd::exec_journal::cancel(&entry)
        .await
        .expect("nothing is running: cancel reached its goal state and must not error");
}
