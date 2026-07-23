//! Byte-level golden + variant-index pins for the host ↔ in-guest agentd
//! vsock protocol (`WireRequest`, `WireResponse`, `WireExecEvent`, and the
//! payload structs they carry).
//!
//! ## Why this exists
//!
//! `bincode` (1.x, the framing in `engram-agentd::proto`) is positional:
//! enums encode by `u32` variant *index*, structs by field *order*.
//! Neither is self-describing. The pre-existing round-trip tests in
//! `src/proto.rs` cannot catch a format break — both ends recompile
//! together in CI, so a reordered variant or an inserted field
//! round-trips green and then desyncs against a peer built from an older
//! tree.
//!
//! agentd is the worst-case skew boundary: it is BAKED into VM images /
//! base snapshots, so a freshly-deployed host-agent routinely talks to an
//! agentd built from a months-old tree (a session resumed from last
//! month's base snapshot runs last month's agentd). The `WireRequest`
//! doc comment in `src/proto.rs` spells out the "APPEND-ONLY" rule but
//! nothing enforced it until this test: inserting a variant mid-enum
//! shifts every later index and desyncs the pair.
//!
//! This test pins the exact bytes (`golden/<name>.bin`) plus, for every
//! enum, the `u32` variant index in `bytes[0..4]`. Reordering a variant
//! or adding a non-trailing field fails here with a message naming the
//! append-only rule, turning a silent fleet-desync into a CI failure.
//!
//! ## Regenerating the corpus (only when you INTENTIONALLY evolve a type)
//!
//! Adding a *trailing* enum variant or a *trailing* struct field is the
//! only wire-safe evolution. After such a change, regenerate:
//!
//! ```text
//!   cargo test -p engram-agentd --test wire_golden -- --ignored regen_golden
//! ```
//!
//! then `git add` the changed `golden/*.bin` and review the diff: an
//! EXISTING golden file changing bytes is a RED FLAG (you broke the wire
//! for an old baked agentd); only NEW files are expected.
//!
//! 2026-07 core-ops fold: the former standalone CA-install verb was
//! deleted and later indices renumbered, so these goldens were
//! deliberately regenerated — a zero-user clean break (see the
//! APPEND-ONLY note on `WireRequest` in `src/proto.rs`). This is the one
//! deliberate exception to the RED FLAG rule above; it is not a
//! precedent for future golden-byte diffs.
//!
//! NOTE: every sample uses an EMPTY or SINGLE-entry `HashMap` so the
//! encoding is deterministic (multi-entry map iteration order is not).

use std::collections::HashMap;
use std::path::PathBuf;

use engram_agentd::proto::{
    AgentReady, SpawnHarnessRequest, WireDownloadResponse, WireExecEvent, WireExecRequest,
    WireHandshake, WireHandshakeAck, WireRequest, WireResponse, WireStatResponse,
};
use serde::Serialize;

fn golden_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("golden")
        .join(format!("{name}.bin"))
}

fn assert_golden<T>(name: &str, value: &T)
where
    T: Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug,
{
    let encoded = bincode::serialize(value).expect("bincode encode");
    let path = golden_path(name);
    let golden = std::fs::read(&path).unwrap_or_else(|e| {
        panic!(
            "missing golden file {}: {e}\n\
             regenerate with: cargo test -p engram-agentd --test wire_golden -- --ignored regen_golden",
            path.display()
        )
    });
    assert_eq!(
        encoded, golden,
        "wire format for `{name}` changed: bincode output != golden bytes.\n\
         bincode is POSITIONAL — a reordered field/variant or an inserted \
         (non-trailing) field breaks the host↔agentd channel for every \
         agentd baked into an existing image / base snapshot.\n\
         If this is an intentional, wire-SAFE evolution (a TRAILING \
         field/variant only), regenerate the corpus per the module header."
    );
    let decoded: T = bincode::deserialize(&golden).expect("bincode decode golden");
    assert_eq!(
        &decoded, value,
        "golden bytes for `{name}` no longer decode to the expected value"
    );
}

fn assert_variant_index<T: Serialize>(value: &T, idx: u32, variant: &str) {
    let encoded = bincode::serialize(value).expect("bincode encode");
    assert!(
        encoded.len() >= 4,
        "enum encoding too short for `{variant}`"
    );
    assert_eq!(
        &encoded[0..4],
        &idx.to_le_bytes(),
        "variant index for `{variant}` is not {idx}.\n\
         agentd's WireRequest/WireResponse are APPEND-ONLY: a new variant \
         goes at the END so existing indices never shift. Reordering or \
         inserting desyncs every agentd baked into an existing image."
    );
}

// ---- canonical sample values (deterministic; ≤1 map entry) -------------

fn exec_request() -> WireExecRequest {
    WireExecRequest {
        command: vec!["echo".into(), "hello".into()],
        stdin: None,
        env: HashMap::from([("RUST_LOG".into(), "info".into())]),
        workdir: Some("/tmp".into()),
        timeout_ms: Some(5_000),
        exec_id: Some("exec-golden".into()),
        stdout_offset: Some(11),
        stderr_offset: Some(12),
        wake: Some(true),
        attach_only: true,
    }
}

fn spawn_harness() -> SpawnHarnessRequest {
    SpawnHarnessRequest {
        argv: vec!["/opt/engram/harness/harness".into()],
        env: HashMap::from([("ENGRAM_HARNESS_CWD".into(), "/workspace".into())]),
        session_env: HashMap::new(),
        host_ca_pem: Some("-----BEGIN CERTIFICATE-----\n...\n-----END CERTIFICATE-----\n".into()),
    }
}

// ---- WireRequest -------------------------------------------------------

#[test]
fn wire_request_golden_and_variant_indices() {
    let exec = WireRequest::Exec(exec_request());
    let stat = WireRequest::Stat {
        path: "/etc/hostname".into(),
    };
    let upload = WireRequest::Upload {
        path: "/tmp/x".into(),
        bytes: vec![1, 2, 3],
        mode: Some(0o755),
    };
    let download = WireRequest::Download {
        path: "/tmp/y".into(),
    };
    let start_shell = WireRequest::StartShell { port: Some(7681) };
    let spawn = WireRequest::SpawnHarness(spawn_harness());
    let start_browser = WireRequest::StartBrowser { port: Some(5900) };
    let start_ide = WireRequest::StartIde { port: Some(13337) };
    let cancel_exec = WireRequest::CancelExec {
        exec_id: "exec-golden".into(),
    };

    assert_golden("request_exec", &exec);
    assert_golden("request_stat", &stat);
    assert_golden("request_upload", &upload);
    assert_golden("request_download", &download);
    assert_golden("request_ping", &WireRequest::Ping);
    assert_golden("request_shutdown", &WireRequest::Shutdown);
    assert_golden("request_guest_ip", &WireRequest::GuestIp);
    assert_golden("request_start_shell", &start_shell);
    assert_golden("request_spawn_harness", &spawn);
    assert_golden("request_sync", &WireRequest::Sync);
    assert_golden("request_start_browser", &start_browser);
    assert_golden("request_stop_browser", &WireRequest::StopBrowser);
    assert_golden("request_start_ide", &start_ide);
    assert_golden("request_stop_ide", &WireRequest::StopIde);
    assert_golden("request_cancel_exec", &cancel_exec);

    assert_variant_index(&exec, 0, "WireRequest::Exec");
    assert_variant_index(&stat, 1, "WireRequest::Stat");
    assert_variant_index(&upload, 2, "WireRequest::Upload");
    assert_variant_index(&download, 3, "WireRequest::Download");
    assert_variant_index(&WireRequest::Ping, 4, "WireRequest::Ping");
    assert_variant_index(&WireRequest::Shutdown, 5, "WireRequest::Shutdown");
    assert_variant_index(&WireRequest::GuestIp, 6, "WireRequest::GuestIp");
    assert_variant_index(&start_shell, 7, "WireRequest::StartShell");
    assert_variant_index(&spawn, 8, "WireRequest::SpawnHarness");
    // 2026-07 core-ops fold: the former standalone CA-install verb
    // (index 9) was deleted; Sync/StartBrowser/StopBrowser shift down
    // one index each.
    assert_variant_index(&WireRequest::Sync, 9, "WireRequest::Sync");
    assert_variant_index(&start_browser, 10, "WireRequest::StartBrowser");
    assert_variant_index(&WireRequest::StopBrowser, 11, "WireRequest::StopBrowser");
    assert_variant_index(&WireRequest::RefreshAgent, 12, "WireRequest::RefreshAgent");
    // ADR 0085: appended after RefreshAgent.
    assert_variant_index(&start_ide, 13, "WireRequest::StartIde");
    assert_variant_index(&WireRequest::StopIde, 14, "WireRequest::StopIde");
    assert_variant_index(
        &WireRequest::StepClock { unix_nanos: 123 },
        15,
        "WireRequest::StepClock",
    );
    assert_variant_index(&cancel_exec, 16, "WireRequest::CancelExec");
}

// ---- WireResponse ------------------------------------------------------

#[test]
fn wire_response_golden_and_variant_indices() {
    let stat = WireResponse::Stat(WireStatResponse {
        exists: true,
        size: 4096,
        mtime_unix: 1_770_000_000,
        is_dir: false,
    });
    let download = WireResponse::Download(WireDownloadResponse {
        bytes: vec![0xDE, 0xAD, 0xBE, 0xEF],
    });
    let guest_ip = WireResponse::GuestIp(Some("169.254.0.21".into()));
    let shell_ready = WireResponse::ShellReady {
        port: 7681,
        spawned: true,
    };
    let harness_spawned = WireResponse::HarnessSpawned {
        pid: Some(42),
        ca_changed: Some(true),
    };
    let error = WireResponse::Error {
        kind: "NotFound".into(),
        message: "no such file".into(),
    };
    // Issue #569 (2026-07): `cdp_warning` was added to `BrowserReady` IN
    // PLACE — a deliberate wire break (see the variant's doc comment in
    // proto.rs). The `response_browser_ready` golden bytes changed with it;
    // both cdp_warning arms are pinned so the Option encoding stays fixed.
    let browser_ready = WireResponse::BrowserReady {
        port: 5900,
        spawned: true,
        cdp_warning: None,
    };
    let browser_ready_with_warning = WireResponse::BrowserReady {
        port: 5900,
        spawned: true,
        cdp_warning: Some("chromium CDP (:9222) not responding".into()),
    };
    let ide_ready = WireResponse::IdeReady {
        port: 13337,
        spawned: true,
    };

    assert_golden("response_stat", &stat);
    assert_golden("response_upload_ok", &WireResponse::UploadOk);
    assert_golden("response_download", &download);
    assert_golden("response_pong", &WireResponse::Pong);
    assert_golden("response_shutdown_ack", &WireResponse::ShutdownAck);
    assert_golden("response_guest_ip", &guest_ip);
    assert_golden("response_shell_ready", &shell_ready);
    assert_golden("response_harness_spawned", &harness_spawned);
    assert_golden("response_error", &error);
    assert_golden("response_synced", &WireResponse::Synced);
    assert_golden("response_browser_ready", &browser_ready);
    assert_golden(
        "response_browser_ready_with_warning",
        &browser_ready_with_warning,
    );
    assert_golden("response_browser_stopped", &WireResponse::BrowserStopped);
    assert_golden("response_ide_ready", &ide_ready);
    assert_golden("response_ide_stopped", &WireResponse::IdeStopped);
    assert_golden("response_exec_cancelled", &WireResponse::ExecCancelled);

    assert_variant_index(&stat, 0, "WireResponse::Stat");
    assert_variant_index(&WireResponse::UploadOk, 1, "WireResponse::UploadOk");
    assert_variant_index(&download, 2, "WireResponse::Download");
    assert_variant_index(&WireResponse::Pong, 3, "WireResponse::Pong");
    assert_variant_index(&WireResponse::ShutdownAck, 4, "WireResponse::ShutdownAck");
    assert_variant_index(&guest_ip, 5, "WireResponse::GuestIp");
    assert_variant_index(&shell_ready, 6, "WireResponse::ShellReady");
    assert_variant_index(&harness_spawned, 7, "WireResponse::HarnessSpawned");
    // 2026-07 core-ops fold: the former standalone CA-install ack
    // (index 8) was deleted; Error/Synced/BrowserReady/BrowserStopped
    // shift down one index each.
    assert_variant_index(&error, 8, "WireResponse::Error");
    assert_variant_index(&WireResponse::Synced, 9, "WireResponse::Synced");
    assert_variant_index(&browser_ready, 10, "WireResponse::BrowserReady");
    assert_variant_index(
        &WireResponse::BrowserStopped,
        11,
        "WireResponse::BrowserStopped",
    );
    assert_variant_index(
        &WireResponse::AgentRefreshed {
            restarting: false,
            sha256: None,
        },
        12,
        "WireResponse::AgentRefreshed",
    );
    // ADR 0085: appended after AgentRefreshed.
    assert_variant_index(&ide_ready, 13, "WireResponse::IdeReady");
    assert_variant_index(&WireResponse::IdeStopped, 14, "WireResponse::IdeStopped");
    assert_variant_index(
        &WireResponse::ClockStepped {
            applied_offset_nanos: Some(123),
        },
        15,
        "WireResponse::ClockStepped",
    );
    assert_variant_index(
        &WireResponse::ExecCancelled,
        16,
        "WireResponse::ExecCancelled",
    );
}

// ---- WireExecEvent -----------------------------------------------------

#[test]
fn wire_exec_event_golden_and_variant_indices() {
    let stdout = WireExecEvent::Stdout(b"hello\n".to_vec());
    let stderr = WireExecEvent::Stderr(vec![0xff, 0x00, 0xff]);
    let exit = WireExecEvent::Exit(Some(0));
    let started = WireExecEvent::Started("exec-golden".into());
    let degraded = WireExecEvent::Degraded("ENOSPC".into());

    assert_golden("exec_event_stdout", &stdout);
    assert_golden("exec_event_stderr", &stderr);
    assert_golden("exec_event_exit", &exit);
    assert_golden("exec_event_started", &started);
    assert_golden("exec_event_degraded", &degraded);

    assert_variant_index(&stdout, 0, "WireExecEvent::Stdout");
    assert_variant_index(&stderr, 1, "WireExecEvent::Stderr");
    assert_variant_index(&exit, 2, "WireExecEvent::Exit");
    assert_variant_index(&started, 3, "WireExecEvent::Started");
    assert_variant_index(&degraded, 4, "WireExecEvent::Degraded");
}

// ---- handshake + ready structs -----------------------------------------

#[test]
fn handshake_and_ready_structs_golden() {
    assert_golden(
        "agent_ready",
        &AgentReady {
            agent_version: "engram-agentd/0.1.0".into(),
        },
    );
    assert_golden(
        "wire_handshake",
        &WireHandshake {
            token: "tok".into(),
            agent_version: "engram-agentd/0.1.0".into(),
        },
    );
    assert_golden(
        "wire_handshake_ack_ok",
        &WireHandshakeAck {
            ok: true,
            message: None,
        },
    );
    assert_golden(
        "wire_handshake_ack_rejected",
        &WireHandshakeAck {
            ok: false,
            message: Some("bad token".into()),
        },
    );
}

/// Writer for the golden corpus. `#[ignore]`d so a normal `cargo test`
/// never regenerates (which would mask a real break). See module header.
#[test]
#[ignore = "regenerates the golden corpus; run explicitly when intentionally evolving a wire type"]
fn regen_golden() {
    fn write<T: Serialize>(name: &str, value: &T) {
        let bytes = bincode::serialize(value).expect("bincode encode");
        let path = golden_path(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &bytes).unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
        eprintln!("wrote {} ({} bytes)", path.display(), bytes.len());
    }

    write("request_exec", &WireRequest::Exec(exec_request()));
    write(
        "request_stat",
        &WireRequest::Stat {
            path: "/etc/hostname".into(),
        },
    );
    write(
        "request_upload",
        &WireRequest::Upload {
            path: "/tmp/x".into(),
            bytes: vec![1, 2, 3],
            mode: Some(0o755),
        },
    );
    write(
        "request_download",
        &WireRequest::Download {
            path: "/tmp/y".into(),
        },
    );
    write("request_ping", &WireRequest::Ping);
    write("request_shutdown", &WireRequest::Shutdown);
    write("request_guest_ip", &WireRequest::GuestIp);
    write(
        "request_start_shell",
        &WireRequest::StartShell { port: Some(7681) },
    );
    write(
        "request_spawn_harness",
        &WireRequest::SpawnHarness(spawn_harness()),
    );
    write("request_sync", &WireRequest::Sync);
    write(
        "request_start_browser",
        &WireRequest::StartBrowser { port: Some(5900) },
    );
    write("request_stop_browser", &WireRequest::StopBrowser);
    write(
        "request_start_ide",
        &WireRequest::StartIde { port: Some(13337) },
    );
    write("request_stop_ide", &WireRequest::StopIde);
    write(
        "request_cancel_exec",
        &WireRequest::CancelExec {
            exec_id: "exec-golden".into(),
        },
    );

    write(
        "response_stat",
        &WireResponse::Stat(WireStatResponse {
            exists: true,
            size: 4096,
            mtime_unix: 1_770_000_000,
            is_dir: false,
        }),
    );
    write("response_upload_ok", &WireResponse::UploadOk);
    write(
        "response_download",
        &WireResponse::Download(WireDownloadResponse {
            bytes: vec![0xDE, 0xAD, 0xBE, 0xEF],
        }),
    );
    write("response_pong", &WireResponse::Pong);
    write("response_shutdown_ack", &WireResponse::ShutdownAck);
    write(
        "response_guest_ip",
        &WireResponse::GuestIp(Some("169.254.0.21".into())),
    );
    write(
        "response_shell_ready",
        &WireResponse::ShellReady {
            port: 7681,
            spawned: true,
        },
    );
    write(
        "response_harness_spawned",
        &WireResponse::HarnessSpawned {
            pid: Some(42),
            ca_changed: Some(true),
        },
    );
    write(
        "response_error",
        &WireResponse::Error {
            kind: "NotFound".into(),
            message: "no such file".into(),
        },
    );
    write("response_synced", &WireResponse::Synced);
    write(
        "response_browser_ready",
        &WireResponse::BrowserReady {
            port: 5900,
            spawned: true,
            cdp_warning: None,
        },
    );
    write(
        "response_browser_ready_with_warning",
        &WireResponse::BrowserReady {
            port: 5900,
            spawned: true,
            cdp_warning: Some("chromium CDP (:9222) not responding".into()),
        },
    );
    write("response_browser_stopped", &WireResponse::BrowserStopped);
    write(
        "response_ide_ready",
        &WireResponse::IdeReady {
            port: 13337,
            spawned: true,
        },
    );
    write("response_ide_stopped", &WireResponse::IdeStopped);
    write("response_exec_cancelled", &WireResponse::ExecCancelled);

    write(
        "exec_event_stdout",
        &WireExecEvent::Stdout(b"hello\n".to_vec()),
    );
    write(
        "exec_event_stderr",
        &WireExecEvent::Stderr(vec![0xff, 0x00, 0xff]),
    );
    write("exec_event_exit", &WireExecEvent::Exit(Some(0)));
    write(
        "exec_event_started",
        &WireExecEvent::Started("exec-golden".into()),
    );
    write(
        "exec_event_degraded",
        &WireExecEvent::Degraded("ENOSPC".into()),
    );

    write(
        "agent_ready",
        &AgentReady {
            agent_version: "engram-agentd/0.1.0".into(),
        },
    );
    write(
        "wire_handshake",
        &WireHandshake {
            token: "tok".into(),
            agent_version: "engram-agentd/0.1.0".into(),
        },
    );
    write(
        "wire_handshake_ack_ok",
        &WireHandshakeAck {
            ok: true,
            message: None,
        },
    );
    write(
        "wire_handshake_ack_rejected",
        &WireHandshakeAck {
            ok: false,
            message: Some("bad token".into()),
        },
    );
}
